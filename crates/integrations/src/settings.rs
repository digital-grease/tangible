// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! What an administrator configures for an integration, checked (ADR 0016).
//!
//! An integration's settings are stored as JSON, but nothing unchecked goes
//! in: each kind has a typed form that unknown fields, bad URLs and unsafe
//! paths fail to parse into. Its secrets (a password, an API key) are not
//! settings: they are named here, entered separately, and stored sealed.
//!
//! Only qBittorrent so far (docs/plans/acquisition.md A4). SABnzbd, NZBGet
//! and S3 follow in their own phases.

use serde::{Deserialize, Serialize};

use crate::outbound::{OutboundPolicy, Url};

/// The kinds an administrator may configure through the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigurableKind {
    /// A qBittorrent instance's WebUI.
    Qbittorrent,
}

impl ConfigurableKind {
    /// The kind for a stored name, if it may be configured.
    #[must_use]
    pub fn parse(kind: &str) -> Option<Self> {
        match kind {
            "qbittorrent" => Some(Self::Qbittorrent),
            _ => None,
        }
    }

    /// Its stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Qbittorrent => "qbittorrent",
        }
    }

    /// The secrets it takes, by field name.
    #[must_use]
    pub const fn secret_fields(self) -> &'static [&'static str] {
        match self {
            Self::Qbittorrent => &["password"],
        }
    }
}

/// How a qBittorrent integration reaches its client and its client's
/// downloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QbittorrentSettings {
    /// The WebUI's address, such as `http://192.168.1.13:8080/`.
    pub base_url: String,
    /// The WebUI user.
    pub username: String,
    /// The category Tangible adds torrents under and watches.
    #[serde(default = "default_category")]
    pub category: String,
    /// Where qBittorrent says that category's downloads are, as qBittorrent
    /// sees it: an absolute path, such as `/downloads/tangible`.
    pub client_path: String,
    /// The watched root on this server where the same folder is mounted,
    /// read-only, by its id in `TANGIBLE_WATCH_ROOTS`.
    pub local_root: String,
}

fn default_category() -> String {
    "tangible".to_owned()
}

/// Why settings were refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SettingsError(pub String);

impl QbittorrentSettings {
    /// Check settings an administrator sent, answering them as they are
    /// stored.
    ///
    /// # Errors
    ///
    /// [`SettingsError`] saying what will not do. `known_roots` are the
    /// watched roots this server has; `policy` is what integrations are
    /// reached under, so the blocks an administrator allowed count.
    pub fn checked(
        value: &serde_json::Value,
        known_roots: &[String],
        policy: &OutboundPolicy,
    ) -> Result<Self, SettingsError> {
        let refuse = |what: &str| SettingsError(what.to_owned());
        let settings: Self = serde_json::from_value(value.clone())
            .map_err(|error| SettingsError(format!("the settings do not fit: {error}")))?;
        let url =
            Url::parse(settings.base_url.trim()).map_err(|_| refuse("base_url is not a URL"))?;
        policy
            .check_url(&url)
            .map_err(|refusal| SettingsError(format!("base_url will not be reached: {refusal}")))?;
        if url.query().is_some() || url.fragment().is_some() {
            return Err(refuse("base_url must not have a query or fragment"));
        }
        if settings.username.trim().is_empty() || settings.username.len() > 200 {
            return Err(refuse("username must be 1 to 200 characters"));
        }
        if settings.username.chars().any(char::is_control) {
            return Err(refuse("username must not contain control characters"));
        }
        let category_ok = (1..=64).contains(&settings.category.len())
            && settings
                .category
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '));
        if !category_ok {
            return Err(refuse(
                "category must be 1 to 64 letters, digits, spaces, '-', '_' or '.'",
            ));
        }
        let path = settings.client_path.trim_end_matches('/');
        let path_ok = path.starts_with('/')
            && path.len() <= 1024
            && !path
                .split('/')
                .any(|segment| segment == ".." || segment == ".")
            && !path.chars().any(char::is_control);
        if !path_ok {
            return Err(refuse(
                "client_path must be an absolute path as qBittorrent reports it, without . or ..",
            ));
        }
        if !known_roots.contains(&settings.local_root) {
            return Err(SettingsError(format!(
                "local_root must be one of this server's watched roots ({})",
                if known_roots.is_empty() {
                    "none are configured; set TANGIBLE_WATCH_ROOTS".to_owned()
                } else {
                    known_roots.join(", ")
                }
            )));
        }
        Ok(Self {
            base_url: url.to_string(),
            username: settings.username.trim().to_owned(),
            category: settings.category,
            client_path: if path.is_empty() {
                "/".to_owned()
            } else {
                path.to_owned()
            },
            local_root: settings.local_root,
        })
    }
}

/// Check settings for `kind`, answering them as stored.
///
/// # Errors
///
/// [`SettingsError`].
pub fn checked_settings(
    kind: ConfigurableKind,
    value: &serde_json::Value,
    known_roots: &[String],
    policy: &OutboundPolicy,
) -> Result<serde_json::Value, SettingsError> {
    match kind {
        ConfigurableKind::Qbittorrent => {
            let settings = QbittorrentSettings::checked(value, known_roots, policy)?;
            serde_json::to_value(settings).map_err(|error| {
                SettingsError(format!("the settings could not be stored: {error}"))
            })
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn roots() -> Vec<String> {
        vec!["downloads".to_owned()]
    }

    fn policy() -> OutboundPolicy {
        OutboundPolicy::for_integrations(Vec::new(), None)
    }

    fn good() -> serde_json::Value {
        serde_json::json!({
            "base_url": "http://192.168.1.13:8080",
            "username": "admin",
            "client_path": "/downloads/tangible/",
            "local_root": "downloads",
        })
    }

    #[test]
    fn good_settings_are_stored_tidied() {
        let stored =
            checked_settings(ConfigurableKind::Qbittorrent, &good(), &roots(), &policy()).unwrap();
        assert_eq!(
            stored,
            serde_json::json!({
                "base_url": "http://192.168.1.13:8080/",
                "username": "admin",
                "category": "tangible",
                "client_path": "/downloads/tangible",
                "local_root": "downloads",
            })
        );
    }

    #[test]
    fn settings_that_will_not_do_are_refused_saying_why() {
        let with = |field: &str, value: serde_json::Value| {
            let mut settings = good();
            settings[field] = value;
            settings
        };
        for (settings, says) in [
            (with("base_url", "ftp://x/".into()), "base_url"),
            (
                with("base_url", "http://127.0.0.1:8080/".into()),
                "base_url",
            ),
            (
                with("base_url", "http://169.254.169.254/".into()),
                "base_url",
            ),
            (with("base_url", "http://u:p@nas:8080/".into()), "base_url"),
            (with("base_url", "http://nas:8080/?x=1".into()), "query"),
            (with("username", "".into()), "username"),
            (with("category", "../x".into()), "category"),
            (with("client_path", "relative".into()), "client_path"),
            (
                with("client_path", "/downloads/../etc".into()),
                "client_path",
            ),
            (with("local_root", "elsewhere".into()), "local_root"),
            (with("password", "hunter2".into()), "do not fit"),
        ] {
            let error = checked_settings(
                ConfigurableKind::Qbittorrent,
                &settings,
                &roots(),
                &policy(),
            )
            .expect_err("refused");
            assert!(error.0.contains(says), "{settings}: {error}");
        }
    }

    #[test]
    fn an_allowed_block_lets_a_loopback_client_through() {
        let mut settings = good();
        settings["base_url"] = "http://127.0.0.1:8080/".into();
        assert!(
            checked_settings(
                ConfigurableKind::Qbittorrent,
                &settings,
                &roots(),
                &policy()
            )
            .is_err()
        );
        let allowed = OutboundPolicy::for_integrations(vec!["127.0.0.1/32".parse().unwrap()], None);
        assert!(
            checked_settings(ConfigurableKind::Qbittorrent, &settings, &roots(), &allowed).is_ok()
        );
    }

    #[test]
    fn only_known_kinds_are_configured_and_each_names_its_secrets() {
        assert_eq!(
            ConfigurableKind::parse("qbittorrent"),
            Some(ConfigurableKind::Qbittorrent)
        );
        for other in ["romm", "sabnzbd", "s3", ""] {
            assert_eq!(ConfigurableKind::parse(other), None, "{other}");
        }
        assert_eq!(ConfigurableKind::Qbittorrent.secret_fields(), ["password"]);
    }
}
