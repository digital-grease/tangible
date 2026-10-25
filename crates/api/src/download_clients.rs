// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Opening a configured download client (ADR 0016): its stored settings,
//! checked again as they are read, and its password, opened with the
//! server's key only for as long as the client needs it. Shared by the
//! routes that submit and test, and by the import runner that follows a
//! torrent to the end.

use tangible_db::Database;
use tangible_db::integrations::{IntegrationRecord, sealed_secret};
use tangible_integrations::outbound::{OutboundClient, Url};
use tangible_integrations::qbittorrent::{QbittorrentClient, QbittorrentError};
use tangible_integrations::settings::QbittorrentSettings;

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
    pub fn from_client(error: &QbittorrentError) -> Self {
        Self::new(error.code(), error.to_string())
    }
}

/// A qBittorrent integration's client and its settings, ready to use.
///
/// # Errors
///
/// [`OpenError`]: `INTEGRATION_OFF` for one turned off or of another kind,
/// `INTEGRATION_MISCONFIGURED` for settings that no longer read,
/// `SECRET_KEY_MISSING` or `INTEGRATION_SECRET_UNREADABLE` for a password
/// that cannot be opened, `INTEGRATION_OFF` too for a server with no client
/// to reach integrations with, `STORAGE_UNAVAILABLE` for a database
/// failure.
pub async fn open_qbittorrent(
    database: &Database,
    http: Option<&OutboundClient>,
    key: Option<&SecretKey>,
    record: &IntegrationRecord,
) -> Result<(QbittorrentClient, QbittorrentSettings), OpenError> {
    if record.kind != "qbittorrent" {
        return Err(OpenError::new(
            "INTEGRATION_OFF",
            format!("{} is not a qBittorrent integration", record.name),
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
    let settings: QbittorrentSettings = serde_json::from_value(record.configuration.clone())
        .map_err(|_| {
            OpenError::new(
                "INTEGRATION_MISCONFIGURED",
                format!(
                    "{}'s settings no longer read; an administrator can save them again",
                    record.name
                ),
            )
        })?;
    let base = Url::parse(&settings.base_url).map_err(|_| {
        OpenError::new(
            "INTEGRATION_MISCONFIGURED",
            format!("{}'s address no longer reads", record.name),
        )
    })?;
    let password = match sealed_secret(database.pool(), record.id, "password").await {
        Ok(None) => None,
        Ok(Some(row)) => {
            let Some(key) = key else {
                return Err(OpenError::new(
                    "SECRET_KEY_MISSING",
                    "the password is stored, and this server holds no key to open it \
                     (TANGIBLE_SECRET_KEY_FILE)",
                ));
            };
            Some(
                key.open(&record.id.to_string(), "password", &Sealed::from(&row))
                    .map_err(|error| {
                        OpenError::new(
                            "INTEGRATION_SECRET_UNREADABLE",
                            format!(
                                "the stored password does not open: {error}; restore the key it \
                                 was sealed with, or enter the password again"
                            ),
                        )
                    })?,
            )
        }
        Err(error) => {
            tracing::error!(error = ?error, "an integration's password could not be read");
            return Err(OpenError::new(
                "STORAGE_UNAVAILABLE",
                "the integration's password could not be read",
            ));
        }
    };
    let client = QbittorrentClient::new(http.clone(), base, &settings.username, password);
    Ok((client, settings))
}
