// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Opening a configured download client (ADR 0016): its stored settings,
//! checked again as they are read, and its secret, opened with the server's
//! key only for as long as the client needs it. Shared by the routes that
//! submit and test, and by the import runner that follows a job to the end.
//! S3, RomM's API and Prowlarr are opened the same way.

use tangible_db::Database;
use tangible_db::integrations::{IntegrationRecord, sealed_secret};
use tangible_domain::SecretString;
use tangible_integrations::download::{ClientError, JobStatus};
use tangible_integrations::nzbget::NzbgetClient;
use tangible_integrations::outbound::{OutboundClient, Url};
use tangible_integrations::prowlarr::ProwlarrClient;
use tangible_integrations::qbittorrent::QbittorrentClient;
use tangible_integrations::romm_api::RommClient;
use tangible_integrations::s3::S3Client;
use tangible_integrations::sabnzbd::SabnzbdClient;
use tangible_integrations::settings::{
    NzbgetSettings, ProwlarrSettings, QbittorrentSettings, RommApiSettings, S3Settings,
    SabnzbdSettings,
};
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
    /// NZBGet.
    Nzbget(NzbgetClient),
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
            Self::Nzbget(_) => "NZBGet",
        }
    }

    /// The import source kind a job in this client becomes.
    #[must_use]
    pub const fn source_kind(&self) -> &'static str {
        match self {
            Self::Qbittorrent(_) => "qbittorrent_job",
            Self::Sabnzbd(_) => "sabnzbd_job",
            Self::Nzbget(_) => "nzbget_job",
        }
    }

    /// Where the client's job `id` stands (a torrent's infohash, a SABnzbd
    /// `nzo_id`, an NZBGet NZBID), or `None` when the client has no such
    /// job.
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
            Self::Nzbget(client) => match id.parse::<i64>() {
                Ok(nzbid) => client.job(nzbid).await,
                Err(_) => Ok(None),
            },
        }
    }

    /// Forget job `id` in a Usenet client's history, once Tangible has
    /// deleted its files (docs/plans/source-retention.md): SABnzbd archives
    /// the entry, NZBGet hides it. Answers whether the client had it, where
    /// it says. qBittorrent's jobs are removed with
    /// [`QbittorrentClient::remove`], never through this.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; [`ClientError::BadResponse`] for qBittorrent.
    pub async fn forget(&mut self, id: &str) -> Result<bool, ClientError> {
        match self {
            Self::Qbittorrent(_) => Err(ClientError::bad(
                "qBittorrent",
                "a torrent is removed, not forgotten",
            )),
            Self::Sabnzbd(client) => client.forget(id).await.map(|()| true),
            Self::Nzbget(client) => match id.parse::<i64>() {
                Ok(nzbid) => client.forget(nzbid).await,
                Err(_) => Ok(false),
            },
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
            Self::Nzbget(client) => {
                let version = client.version().await?;
                let categories = client.categories().await?;
                // NZBGet matches category names regardless of case.
                Ok((
                    version,
                    categories.iter().any(|c| c.eq_ignore_ascii_case(category)),
                ))
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
/// `INTEGRATION_MISCONFIGURED` for settings that no longer read, a SABnzbd
/// without its API key or an NZBGet without its password; `SECRET_KEY_MISSING` or
/// `INTEGRATION_SECRET_UNREADABLE` for a secret that cannot be opened;
/// `STORAGE_UNAVAILABLE` for a database failure.
pub async fn open_client(
    database: &Database,
    http: Option<&OutboundClient>,
    key: Option<&SecretKey>,
    record: &IntegrationRecord,
) -> Result<(DownloadClient, Folders), OpenError> {
    if !matches!(record.kind.as_str(), "qbittorrent" | "sabnzbd" | "nzbget") {
        return Err(OpenError::new(
            "INTEGRATION_OFF",
            format!("{} is not a download client", record.name),
        ));
    }
    let http = in_use(record, http)?;
    let opener = Opener {
        database,
        http,
        key,
        record,
    };
    match record.kind.as_str() {
        "qbittorrent" => opener.qbittorrent().await,
        "sabnzbd" => opener.sabnzbd().await,
        _ => opener.nzbget().await,
    }
}

/// A configured S3 integration's client and its settings, ready to use.
///
/// # Errors
///
/// As [`open_client`]; `INTEGRATION_MISCONFIGURED` too for one without its
/// access key id or secret.
pub async fn open_s3(
    database: &Database,
    http: Option<&OutboundClient>,
    key: Option<&SecretKey>,
    record: &IntegrationRecord,
) -> Result<(S3Client, S3Settings), OpenError> {
    if record.kind != "s3" {
        return Err(OpenError::new(
            "INTEGRATION_OFF",
            format!("{} is not an S3 integration", record.name),
        ));
    }
    let http = in_use(record, http)?;
    Opener {
        database,
        http,
        key,
        record,
    }
    .s3()
    .await
}

/// A configured RomM API integration's client and its settings, ready to
/// use.
///
/// # Errors
///
/// As [`open_client`]; `INTEGRATION_MISCONFIGURED` too for one without its
/// client token.
pub async fn open_romm(
    database: &Database,
    http: Option<&OutboundClient>,
    key: Option<&SecretKey>,
    record: &IntegrationRecord,
) -> Result<(RommClient, RommApiSettings), OpenError> {
    if record.kind != "romm_api" {
        return Err(OpenError::new(
            "INTEGRATION_OFF",
            format!("{} is not RomM's API", record.name),
        ));
    }
    let http = in_use(record, http)?;
    Opener {
        database,
        http,
        key,
        record,
    }
    .romm()
    .await
}

/// A configured Prowlarr's client, ready to use.
///
/// # Errors
///
/// As [`open_client`]; `INTEGRATION_MISCONFIGURED` too for one without its
/// API key.
pub async fn open_prowlarr(
    database: &Database,
    http: Option<&OutboundClient>,
    key: Option<&SecretKey>,
    record: &IntegrationRecord,
) -> Result<ProwlarrClient, OpenError> {
    if record.kind != "prowlarr" {
        return Err(OpenError::new(
            "INTEGRATION_OFF",
            format!("{} is not a Prowlarr", record.name),
        ));
    }
    let http = in_use(record, http)?;
    let opener = Opener {
        database,
        http,
        key,
        record,
    };
    let settings: ProwlarrSettings = opener.settings()?;
    Ok(ProwlarrClient::new(
        http.clone(),
        opener.url(&settings.base_url)?,
        opener.required_secret("api_key", "API key").await?,
    ))
}

/// The integrations client, for an integration that is turned on.
fn in_use<'a>(
    record: &IntegrationRecord,
    http: Option<&'a OutboundClient>,
) -> Result<&'a OutboundClient, OpenError> {
    if !record.enabled {
        return Err(OpenError::new(
            "INTEGRATION_OFF",
            format!(
                "{} is turned off; an administrator can turn it on",
                record.name
            ),
        ));
    }
    http.ok_or_else(|| OpenError::new("INTEGRATION_OFF", "this server reaches no integrations"))
}

/// What opening one integration's client takes.
struct Opener<'a> {
    database: &'a Database,
    http: &'a OutboundClient,
    key: Option<&'a SecretKey>,
    record: &'a IntegrationRecord,
}

impl Opener<'_> {
    fn misconfigured(&self) -> OpenError {
        OpenError::new(
            "INTEGRATION_MISCONFIGURED",
            format!(
                "{}'s settings no longer read; an administrator can save them again",
                self.record.name
            ),
        )
    }

    fn settings<T: serde::de::DeserializeOwned>(&self) -> Result<T, OpenError> {
        serde_json::from_value(self.record.configuration.clone()).map_err(|_| self.misconfigured())
    }

    fn url(&self, text: &str) -> Result<Url, OpenError> {
        Url::parse(text).map_err(|_| self.misconfigured())
    }

    async fn secret(&self, field: &str) -> Result<Option<SecretString>, OpenError> {
        secret(self.database, self.key, self.record, field).await
    }

    /// A secret the client cannot work without.
    async fn required_secret(&self, field: &str, what: &str) -> Result<SecretString, OpenError> {
        self.secret(field).await?.ok_or_else(|| {
            OpenError::new(
                "INTEGRATION_MISCONFIGURED",
                format!(
                    "{} has no {what}; an administrator can set one",
                    self.record.name
                ),
            )
        })
    }

    async fn qbittorrent(&self) -> Result<(DownloadClient, Folders), OpenError> {
        let settings: QbittorrentSettings = self.settings()?;
        let client = QbittorrentClient::new(
            self.http.clone(),
            self.url(&settings.base_url)?,
            &settings.username,
            self.secret("password").await?,
        );
        Ok((
            DownloadClient::Qbittorrent(client),
            folders(settings.category, settings.client_path, settings.local_root),
        ))
    }

    async fn sabnzbd(&self) -> Result<(DownloadClient, Folders), OpenError> {
        let settings: SabnzbdSettings = self.settings()?;
        let client = SabnzbdClient::new(
            self.http.clone(),
            self.url(&settings.base_url)?,
            self.required_secret("api_key", "API key").await?,
        );
        Ok((
            DownloadClient::Sabnzbd(client),
            folders(settings.category, settings.client_path, settings.local_root),
        ))
    }

    async fn s3(&self) -> Result<(S3Client, S3Settings), OpenError> {
        let settings: S3Settings = self.settings()?;
        let access_key_id = self
            .required_secret("access_key_id", "access key id")
            .await?;
        let secret = self
            .required_secret("secret_access_key", "secret access key")
            .await?;
        let client = S3Client::new(
            self.http.clone(),
            self.url(&settings.endpoint)?,
            &settings.region,
            settings.path_style,
            &access_key_id,
            &secret,
        );
        Ok((client, settings))
    }

    async fn romm(&self) -> Result<(RommClient, RommApiSettings), OpenError> {
        let settings: RommApiSettings = self.settings()?;
        let client = RommClient::new(
            self.http.clone(),
            self.url(&settings.base_url)?,
            self.required_secret("token", "client token").await?,
        );
        Ok((client, settings))
    }

    async fn nzbget(&self) -> Result<(DownloadClient, Folders), OpenError> {
        let settings: NzbgetSettings = self.settings()?;
        let client = NzbgetClient::new(
            self.http.clone(),
            self.url(&settings.base_url)?,
            &settings.username,
            self.required_secret("password", "password").await?,
        )
        .map_err(|_| self.misconfigured())?;
        Ok((
            DownloadClient::Nzbget(client),
            folders(settings.category, settings.client_path, settings.local_root),
        ))
    }
}

const fn folders(category: String, client_path: String, local_root: String) -> Folders {
    Folders {
        category,
        client_path,
        local_root,
    }
}

/// An integration's secret `field`, opened, if one is stored.
pub(crate) async fn secret(
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
