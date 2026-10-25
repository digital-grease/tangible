// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Opening a configured download client (ADR 0016): its stored settings,
//! checked again as they are read, and its secret, opened with the server's
//! key only for as long as the client needs it. Shared by the routes that
//! submit and test, and by the import runner that follows a job to the end.

use tangible_db::Database;
use tangible_db::integrations::{IntegrationRecord, sealed_secret};
use tangible_domain::SecretString;
use tangible_integrations::download::{ClientError, JobStatus};
use tangible_integrations::outbound::{OutboundClient, Url};
use tangible_integrations::qbittorrent::QbittorrentClient;
use tangible_integrations::sabnzbd::SabnzbdClient;
use tangible_integrations::settings::{QbittorrentSettings, SabnzbdSettings};
use tangible_integrations::torrent::Infohash;

use crate::secrets::{Sealed, SecretKey};

/// Why a client could not be opened: a stable code and a reason. Neither
/// ever holds a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenError {
    /// Such as `INTEGRATION_OFF`.
    pub code: &'static str,
    /// What to do about it.
    pub detail: String,
}

impl OpenError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    /// From a client error, keeping its code.
    #[must_use]
    pub fn from_client(error: &ClientError) -> Self {
        Self::new(error.code(), error.to_string())
    }
}

/// A download client, ready to use.
#[derive(Debug)]
pub enum DownloadClient {
    /// qBittorrent.
    Qbittorrent(QbittorrentClient),
    /// SABnzbd.
    Sabnzbd(SabnzbdClient),
}

/// Where a client files Tangible's downloads, and where they are read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folders {
    /// The client's category for Tangible.
    pub category: String,
    /// That category's folder, as the client reports it.
    pub client_path: String,
    /// The watched root where the same folder is mounted here.
    pub local_root: String,
}

impl DownloadClient {
    /// The client, in words.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Qbittorrent(_) => "qBittorrent",
            Self::Sabnzbd(_) => "SABnzbd",
        }
    }

    /// The import source kind a job in this client becomes.
    #[must_use]
    pub const fn source_kind(&self) -> &'static str {
        match self {
            Self::Qbittorrent(_) => "qbittorrent_job",
            Self::Sabnzbd(_) => "sabnzbd_job",
        }
    }

    /// Where the client's job `id` stands (a torrent's infohash, a SABnzbd
    /// `nzo_id`), or `None` when the client has no such job.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn job(&mut self, id: &str) -> Result<Option<JobStatus>, ClientError> {
        match self {
            Self::Qbittorrent(client) => {
                let Some(hash) = Infohash::parse(id) else {
                    return Ok(None);
                };
                Ok(client
                    .torrent(&hash)
                    .await?
                    .map(|torrent| JobStatus::new(torrent.phase(), &torrent.state)))
            }
            Self::Sabnzbd(client) => client.job(id).await,
        }
    }

    /// Sign in, and say the client's version, and whether it has
    /// `category`. qBittorrent makes a category it lacks, so always has it.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn version_and_category(
        &mut self,
        category: &str,
    ) -> Result<(String, bool), ClientError> {
        match self {
            Self::Qbittorrent(client) => Ok((client.api_version().await?, true)),
            Self::Sabnzbd(client) => {
                let version = client.version().await?;
                let categories = client.categories().await?;
                Ok((version, categories.iter().any(|c| c == category)))
            }
        }
    }
}

/// A configured download client and its folders, ready to use.
///
/// # Errors
///
/// [`OpenError`]: `INTEGRATION_OFF` for one turned off, of a kind that is
/// not a download client, or on a server that reaches no integrations;
/// `INTEGRATION_MISCONFIGURED` for settings that no longer read or a
/// SABnzbd without its API key; `SECRET_KEY_MISSING` or
/// `INTEGRATION_SECRET_UNREADABLE` for a secret that cannot be opened;
/// `STORAGE_UNAVAILABLE` for a database failure.
pub async fn open_client(
    database: &Database,
    http: Option<&OutboundClient>,
    key: Option<&SecretKey>,
    record: &IntegrationRecord,
) -> Result<(DownloadClient, Folders), OpenError> {
    if !matches!(record.kind.as_str(), "qbittorrent" | "sabnzbd") {
        return Err(OpenError::new(
            "INTEGRATION_OFF",
            format!("{} is not a download client", record.name),
        ));
    }
    if !record.enabled {
        return Err(OpenError::new(
            "INTEGRATION_OFF",
            format!(
                "{} is turned off; an administrator can turn it on",
                record.name
            ),
        ));
    }
    let Some(http) = http else {
        return Err(OpenError::new(
            "INTEGRATION_OFF",
            "this server reaches no integrations",
        ));
    };
    let misconfigured = || {
        OpenError::new(
            "INTEGRATION_MISCONFIGURED",
            format!(
                "{}'s settings no longer read; an administrator can save them again",
                record.name
            ),
        )
    };
    let parse_url = |text: &str| Url::parse(text).map_err(|_| misconfigured());
    if record.kind == "qbittorrent" {
        let settings: QbittorrentSettings =
            serde_json::from_value(record.configuration.clone()).map_err(|_| misconfigured())?;
        let password = secret(database, key, record, "password").await?;
        let client = QbittorrentClient::new(
            http.clone(),
            parse_url(&settings.base_url)?,
            &settings.username,
            password,
        );
        return Ok((
            DownloadClient::Qbittorrent(client),
            Folders {
                category: settings.category,
                client_path: settings.client_path,
                local_root: settings.local_root,
            },
        ));
    }
    let settings: SabnzbdSettings =
        serde_json::from_value(record.configuration.clone()).map_err(|_| misconfigured())?;
    let api_key = secret(database, key, record, "api_key")
        .await?
        .ok_or_else(|| {
            OpenError::new(
                "INTEGRATION_MISCONFIGURED",
                format!(
                    "{} has no API key; an administrator can set one",
                    record.name
                ),
            )
        })?;
    let client = SabnzbdClient::new(http.clone(), parse_url(&settings.base_url)?, api_key);
    Ok((
        DownloadClient::Sabnzbd(client),
        Folders {
            category: settings.category,
            client_path: settings.client_path,
            local_root: settings.local_root,
        },
    ))
}

/// An integration's secret `field`, opened, if one is stored.
async fn secret(
    database: &Database,
    key: Option<&SecretKey>,
    record: &IntegrationRecord,
    field: &str,
) -> Result<Option<SecretString>, OpenError> {
    let row = match sealed_secret(database.pool(), record.id, field).await {
        Ok(None) => return Ok(None),
        Ok(Some(row)) => row,
        Err(error) => {
            tracing::error!(error = ?error, "an integration's secret could not be read");
            return Err(OpenError::new(
                "STORAGE_UNAVAILABLE",
                "the integration's secret could not be read",
            ));
        }
    };
    let Some(key) = key else {
        return Err(OpenError::new(
            "SECRET_KEY_MISSING",
            format!(
                "the {field} is stored, and this server holds no key to open it \
                 (TANGIBLE_SECRET_KEY_FILE)"
            ),
        ));
    };
    key.open(&record.id.to_string(), field, &Sealed::from(&row))
        .map(Some)
        .map_err(|error| {
            OpenError::new(
                "INTEGRATION_SECRET_UNREADABLE",
                format!(
                    "the stored {field} does not open: {error}; restore the key it was sealed \
                     with, or enter it again"
                ),
            )
        })
}
