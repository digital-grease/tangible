// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! What an administrator configures for an integration, checked (ADR 0016).
//!
//! An integration's settings are stored as JSON, but nothing unchecked goes
//! in: each kind has a typed form that unknown fields, bad URLs and unsafe
//! paths fail to parse into. Its secrets (a password, an API key) are not
//! settings: they are named here, entered separately, and stored sealed.
//!
//! qBittorrent (docs/plans/acquisition.md A4), SABnzbd and NZBGet (A5),
//! S3-compatible storage (A6), and RomM's API (docs/plans/romm-api.md M1).

use serde::{Deserialize, Serialize};

use crate::outbound::{OutboundPolicy, Url};

/// The kinds an administrator may configure through the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigurableKind {
    /// A qBittorrent instance's WebUI.
    Qbittorrent,
    /// A SABnzbd's API.
    Sabnzbd,
    /// An NZBGet's JSON-RPC API.
    Nzbget,
    /// An S3-compatible endpoint and the buckets operators may import from.
    S3,
    /// The API of the RomM the export writes into.
    RommApi,
}

impl ConfigurableKind {
    /// The kind for a stored name, if it may be configured.
    #[must_use]
    pub fn parse(kind: &str) -> Option<Self> {
        match kind {
            "qbittorrent" => Some(Self::Qbittorrent),
            "sabnzbd" => Some(Self::Sabnzbd),
            "nzbget" => Some(Self::Nzbget),
            "s3" => Some(Self::S3),
            "romm_api" => Some(Self::RommApi),
            _ => None,
        }
    }

    /// Its stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Qbittorrent => "qbittorrent",
            Self::Sabnzbd => "sabnzbd",
            Self::Nzbget => "nzbget",
            Self::S3 => "s3",
            Self::RommApi => "romm_api",
        }
    }

    /// The secrets it takes, by field name.
    #[must_use]
    pub const fn secret_fields(self) -> &'static [&'static str] {
        match self {
            Self::Qbittorrent | Self::Nzbget => &["password"],
            Self::Sabnzbd => &["api_key"],
            Self::S3 => &["access_key_id", "secret_access_key"],
            Self::RommApi => &["token"],
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
    /// What becomes of a torrent once its import is in the library and
    /// checked: kept seeding (the default), or removed with its files.
    #[serde(default)]
    pub after_import: AfterImport,
}

/// What becomes of an import's source once the library holds a checked
/// copy (docs/plans/source-retention.md, ADR 0017).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AfterImport {
    /// Left where it is.
    #[default]
    Keep,
    /// Removed: a torrent with its files, a Usenet download's files and
    /// history entry, an S3 object version.
    Remove,
}

impl AfterImport {
    /// As stored: `keep` or `remove`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Keep => "keep",
            Self::Remove => "remove",
        }
    }
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
        let settings: Self = parsed(value)?;
        Ok(Self {
            base_url: checked_base_url(&settings.base_url, policy)?,
            username: checked_username(&settings.username)?,
            category: checked_category(settings.category)?,
            client_path: checked_client_path(&settings.client_path, "qBittorrent")?,
            local_root: checked_local_root(settings.local_root, known_roots)?,
            after_import: settings.after_import,
        })
    }
}

/// How a SABnzbd integration reaches its client and its client's downloads.
/// Its API key is a secret, not a setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SabnzbdSettings {
    /// SABnzbd's address, with any URL base it is served under, such as
    /// `http://192.168.1.13:8080/` or `http://nas/sabnzbd/`.
    pub base_url: String,
    /// The category Tangible adds NZBs under. It must exist in SABnzbd,
    /// with its own folder: SABnzbd files an unknown one under the default.
    #[serde(default = "default_category")]
    pub category: String,
    /// Where SABnzbd puts that category's finished downloads, as SABnzbd
    /// sees it: an absolute path, such as `/downloads/complete/tangible`.
    pub client_path: String,
    /// The watched root on this server where the same folder is mounted,
    /// read-only, by its id in `TANGIBLE_WATCH_ROOTS`.
    pub local_root: String,
}

impl SabnzbdSettings {
    /// Check settings an administrator sent, answering them as they are
    /// stored.
    ///
    /// # Errors
    ///
    /// As [`QbittorrentSettings::checked`].
    pub fn checked(
        value: &serde_json::Value,
        known_roots: &[String],
        policy: &OutboundPolicy,
    ) -> Result<Self, SettingsError> {
        let settings: Self = parsed(value)?;
        Ok(Self {
            base_url: checked_base_url(&settings.base_url, policy)?,
            category: checked_category(settings.category)?,
            client_path: checked_client_path(&settings.client_path, "SABnzbd")?,
            local_root: checked_local_root(settings.local_root, known_roots)?,
        })
    }
}

/// How an NZBGet integration reaches its client and its client's downloads.
/// Its password is a secret, not a setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NzbgetSettings {
    /// NZBGet's address, such as `http://192.168.1.13:6789/`.
    pub base_url: String,
    /// The control user (`ControlUsername`), or one with the same rights.
    pub username: String,
    /// The category Tangible adds NZBs under. It must exist in NZBGet:
    /// NZBGet files an unknown one under that name, outside the folder a
    /// category of its own would have.
    #[serde(default = "default_category")]
    pub category: String,
    /// Where NZBGet puts that category's finished downloads, as NZBGet sees
    /// it: an absolute path, such as `/downloads/completed/tangible`.
    pub client_path: String,
    /// The watched root on this server where the same folder is mounted,
    /// read-only, by its id in `TANGIBLE_WATCH_ROOTS`.
    pub local_root: String,
}

impl NzbgetSettings {
    /// Check settings an administrator sent, answering them as they are
    /// stored.
    ///
    /// # Errors
    ///
    /// As [`QbittorrentSettings::checked`].
    pub fn checked(
        value: &serde_json::Value,
        known_roots: &[String],
        policy: &OutboundPolicy,
    ) -> Result<Self, SettingsError> {
        let settings: Self = parsed(value)?;
        Ok(Self {
            base_url: checked_base_url(&settings.base_url, policy)?,
            username: checked_username(&settings.username)?,
            category: checked_category(settings.category)?,
            client_path: checked_client_path(&settings.client_path, "NZBGet")?,
            local_root: checked_local_root(settings.local_root, known_roots)?,
        })
    }
}

/// How an S3 integration reaches its endpoint, and which buckets operators
/// may import from. Its access key id and secret are secrets, not settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3Settings {
    /// The endpoint, such as `https://s3.eu-west-1.amazonaws.com/` or a
    /// self-hosted server's `http://192.168.1.13:8333/`.
    pub endpoint: String,
    /// The region signatures are made for.
    #[serde(default = "default_region")]
    pub region: String,
    /// Buckets in the path (`host/bucket/key`), as self-hosted servers
    /// usually want, rather than as a subdomain (`bucket.host/key`).
    #[serde(default = "default_path_style")]
    pub path_style: bool,
    /// The buckets operators may import from; no other is ever asked for.
    pub buckets: Vec<String>,
    /// What becomes of an object once its import is in the library and
    /// checked: kept (the default), or the version imported deleted.
    #[serde(default)]
    pub after_import: AfterImport,
}

fn default_region() -> String {
    "us-east-1".to_owned()
}

const fn default_path_style() -> bool {
    true
}

/// Most buckets one S3 integration lists.
const MAX_BUCKETS: usize = 50;

impl S3Settings {
    /// Check settings an administrator sent, answering them as they are
    /// stored.
    ///
    /// # Errors
    ///
    /// [`SettingsError`] saying what will not do.
    pub fn checked(
        value: &serde_json::Value,
        policy: &OutboundPolicy,
    ) -> Result<Self, SettingsError> {
        let settings: Self = parsed(value)?;
        let endpoint = checked_base_url(&settings.endpoint, policy)
            .map_err(|error| SettingsError(error.0.replace("base_url", "endpoint")))?;
        let region = settings.region.trim().to_owned();
        let region_ok = (1..=64).contains(&region.len())
            && region
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !region_ok {
            return Err(refuse(
                "region must be 1 to 64 lowercase letters, digits and '-', such as us-east-1",
            ));
        }
        if !settings.path_style
            && Url::parse(&endpoint)
                .ok()
                .is_some_and(|url| !matches!(url.host(), Some(url::Host::Domain(_))))
        {
            return Err(refuse(
                "an endpoint given as an address needs path_style: a bucket cannot be a \
                 subdomain of an address",
            ));
        }
        if settings.buckets.is_empty() || settings.buckets.len() > MAX_BUCKETS {
            return Err(SettingsError(format!(
                "buckets must list 1 to {MAX_BUCKETS} buckets operators may import from"
            )));
        }
        let mut buckets = Vec::with_capacity(settings.buckets.len());
        for bucket in settings.buckets {
            let bucket = bucket.trim().to_owned();
            crate::s3::check_bucket(&bucket)
                .map_err(|error| SettingsError(format!("{bucket}: {error}")))?;
            if !buckets.contains(&bucket) {
                buckets.push(bucket);
            }
        }
        Ok(Self {
            endpoint,
            region,
            path_style: settings.path_style,
            buckets,
            after_import: settings.after_import,
        })
    }
}

/// How the server reaches RomM's API, and how a browser reaches RomM. Its
/// client token is a secret, not a setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RommApiSettings {
    /// RomM's address as this server reaches it, such as
    /// `http://romm:8080/` on the same Compose network.
    pub base_url: String,
    /// RomM's address as a browser reaches it, for links to its pages; the
    /// same as `base_url` when absent. Never fetched by the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_url: Option<String>,
    /// Whether to ask RomM to scan the platforms an export changed.
    #[serde(default = "default_scan_after_export")]
    pub scan_after_export: bool,
}

const fn default_scan_after_export() -> bool {
    true
}

impl RommApiSettings {
    /// Check settings an administrator sent, answering them as they are
    /// stored.
    ///
    /// # Errors
    ///
    /// [`SettingsError`] saying what will not do.
    pub fn checked(
        value: &serde_json::Value,
        policy: &OutboundPolicy,
    ) -> Result<Self, SettingsError> {
        let settings: Self = parsed(value)?;
        let web_url = match settings.web_url.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(text) => Some(checked_web_url(text)?),
        };
        Ok(Self {
            base_url: checked_base_url(&settings.base_url, policy)?,
            web_url,
            scan_after_export: settings.scan_after_export,
        })
    }

    /// The address links to RomM's pages start from.
    #[must_use]
    pub fn link_base(&self) -> &str {
        self.web_url.as_deref().unwrap_or(&self.base_url)
    }
}

/// An address only a browser follows: `http` or `https`, with no
/// credentials, query or fragment. The server never fetches it, so the
/// outbound policy does not apply.
fn checked_web_url(text: &str) -> Result<String, SettingsError> {
    let url = Url::parse(text).map_err(|_| refuse("web_url is not a URL"))?;
    let ok = matches!(url.scheme(), "http" | "https")
        && url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none();
    if !ok {
        return Err(refuse(
            "web_url must be an http or https address without credentials, a query or a fragment",
        ));
    }
    Ok(url.to_string())
}

fn parsed<T: serde::de::DeserializeOwned>(value: &serde_json::Value) -> Result<T, SettingsError> {
    serde_json::from_value(value.clone())
        .map_err(|error| SettingsError(format!("the settings do not fit: {error}")))
}

fn refuse(what: &str) -> SettingsError {
    SettingsError(what.to_owned())
}

/// A base URL the policy reaches, without a query or fragment, as stored.
fn checked_base_url(text: &str, policy: &OutboundPolicy) -> Result<String, SettingsError> {
    let url = Url::parse(text.trim()).map_err(|_| refuse("base_url is not a URL"))?;
    policy
        .check_url(&url)
        .map_err(|refusal| SettingsError(format!("base_url will not be reached: {refusal}")))?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(refuse("base_url must not have a query or fragment"));
    }
    Ok(url.to_string())
}

fn checked_username(name: &str) -> Result<String, SettingsError> {
    if name.trim().is_empty() || name.len() > 200 {
        return Err(refuse("username must be 1 to 200 characters"));
    }
    if name.chars().any(char::is_control) {
        return Err(refuse("username must not contain control characters"));
    }
    Ok(name.trim().to_owned())
}

fn checked_category(category: String) -> Result<String, SettingsError> {
    let ok = (1..=64).contains(&category.len())
        && category
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '));
    if ok {
        Ok(category)
    } else {
        Err(refuse(
            "category must be 1 to 64 letters, digits, spaces, '-', '_' or '.'",
        ))
    }
}

/// An absolute path as the client reports it, without `.` or `..`, stored
/// without a trailing slash.
fn checked_client_path(path: &str, client: &str) -> Result<String, SettingsError> {
    let path = path.trim_end_matches('/');
    let ok = path.starts_with('/')
        && path.len() <= 1024
        && !path
            .split('/')
            .any(|segment| segment == ".." || segment == ".")
        && !path.chars().any(char::is_control);
    if !ok {
        return Err(SettingsError(format!(
            "client_path must be an absolute path as {client} reports it, without . or .."
        )));
    }
    Ok(if path.is_empty() {
        "/".to_owned()
    } else {
        path.to_owned()
    })
}

fn checked_local_root(root: String, known_roots: &[String]) -> Result<String, SettingsError> {
    if known_roots.contains(&root) {
        return Ok(root);
    }
    Err(SettingsError(format!(
        "local_root must be one of this server's watched roots ({})",
        if known_roots.is_empty() {
            "none are configured; set TANGIBLE_WATCH_ROOTS".to_owned()
        } else {
            known_roots.join(", ")
        }
    )))
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
            stored(&QbittorrentSettings::checked(value, known_roots, policy)?)
        }
        ConfigurableKind::Sabnzbd => stored(&SabnzbdSettings::checked(value, known_roots, policy)?),
        ConfigurableKind::Nzbget => stored(&NzbgetSettings::checked(value, known_roots, policy)?),
        ConfigurableKind::S3 => stored(&S3Settings::checked(value, policy)?),
        ConfigurableKind::RommApi => stored(&RommApiSettings::checked(value, policy)?),
    }
}

fn stored(settings: &impl Serialize) -> Result<serde_json::Value, SettingsError> {
    serde_json::to_value(settings)
        .map_err(|error| SettingsError(format!("the settings could not be stored: {error}")))
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
                "after_import": "keep",
            })
        );
    }

    #[test]
    fn removing_after_import_is_asked_for_by_name_and_off_unless_asked() {
        let mut removing = good();
        removing["after_import"] = "remove".into();
        let settings = QbittorrentSettings::checked(&removing, &roots(), &policy()).unwrap();
        assert_eq!(settings.after_import, AfterImport::Remove);
        assert_eq!(settings.after_import.as_str(), "remove");
        assert_eq!(
            QbittorrentSettings::checked(&good(), &roots(), &policy())
                .unwrap()
                .after_import,
            AfterImport::Keep
        );
        for wrong in [
            serde_json::json!("delete"),
            serde_json::json!(true),
            serde_json::json!(null),
        ] {
            let mut settings = good();
            settings["after_import"] = wrong.clone();
            assert!(
                QbittorrentSettings::checked(&settings, &roots(), &policy()).is_err(),
                "{wrong}"
            );
        }
        let mut s3 = serde_json::json!({
            "endpoint": "http://192.168.1.13:8333",
            "buckets": ["discs"],
            "after_import": "remove",
        });
        assert_eq!(
            S3Settings::checked(&s3, &policy()).unwrap().after_import,
            AfterImport::Remove
        );
        s3["after_import"] = "sometimes".into();
        assert!(S3Settings::checked(&s3, &policy()).is_err());
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
    fn sabnzbd_settings_take_no_user_and_are_checked_alike() {
        let good = serde_json::json!({
            "base_url": "http://192.168.1.13:8080/sabnzbd",
            "client_path": "/downloads/complete/tangible/",
            "local_root": "downloads",
        });
        assert_eq!(
            checked_settings(ConfigurableKind::Sabnzbd, &good, &roots(), &policy()).unwrap(),
            serde_json::json!({
                "base_url": "http://192.168.1.13:8080/sabnzbd",
                "category": "tangible",
                "client_path": "/downloads/complete/tangible",
                "local_root": "downloads",
            })
        );
        let mut with_user = good.clone();
        with_user["username"] = "admin".into();
        let error = checked_settings(ConfigurableKind::Sabnzbd, &with_user, &roots(), &policy())
            .expect_err("refused");
        assert!(error.0.contains("do not fit"), "{error}");
        let mut relative = good;
        relative["client_path"] = "complete".into();
        let error = checked_settings(ConfigurableKind::Sabnzbd, &relative, &roots(), &policy())
            .expect_err("refused");
        assert!(error.0.contains("as SABnzbd reports it"), "{error}");
    }

    #[test]
    fn s3_settings_are_checked_and_stored_tidied() {
        let good = serde_json::json!({
            "endpoint": "http://192.168.1.13:8333",
            "buckets": [" discs ", "discs", "archive.2026"],
        });
        assert_eq!(
            checked_settings(ConfigurableKind::S3, &good, &roots(), &policy()).unwrap(),
            serde_json::json!({
                "endpoint": "http://192.168.1.13:8333/",
                "region": "us-east-1",
                "path_style": true,
                "buckets": ["discs", "archive.2026"],
                "after_import": "keep",
            })
        );
        let with = |field: &str, value: serde_json::Value| {
            let mut settings = good.clone();
            settings[field] = value;
            settings
        };
        for (settings, says) in [
            (with("endpoint", "http://127.0.0.1:8333".into()), "endpoint"),
            (with("region", "US East".into()), "region"),
            (with("buckets", serde_json::json!([])), "buckets"),
            (
                with("buckets", serde_json::json!(["Bad_Bucket"])),
                "Bad_Bucket",
            ),
            (with("path_style", false.into()), "path_style"),
            (with("client_path", "/x".into()), "do not fit"),
        ] {
            let error = checked_settings(ConfigurableKind::S3, &settings, &roots(), &policy())
                .expect_err("refused");
            assert!(error.0.contains(says), "{settings}: {error}");
        }
        let mut aws = good;
        aws["endpoint"] = "https://s3.eu-west-1.amazonaws.com".into();
        aws["path_style"] = false.into();
        aws["region"] = "eu-west-1".into();
        assert!(checked_settings(ConfigurableKind::S3, &aws, &roots(), &policy()).is_ok());
    }

    #[test]
    fn romm_api_settings_are_checked_and_stored_tidied() {
        let good = serde_json::json!({ "base_url": "http://192.168.1.13:8080" });
        assert_eq!(
            checked_settings(ConfigurableKind::RommApi, &good, &roots(), &policy()).unwrap(),
            serde_json::json!({
                "base_url": "http://192.168.1.13:8080/",
                "scan_after_export": true,
            })
        );
        let mut linked = good.clone();
        linked["web_url"] = "https://romm.example.net".into();
        linked["scan_after_export"] = false.into();
        let settings = RommApiSettings::checked(&linked, &policy()).unwrap();
        assert_eq!(settings.link_base(), "https://romm.example.net/");
        assert!(!settings.scan_after_export);
        let mut blank = good.clone();
        blank["web_url"] = " ".into();
        let settings = RommApiSettings::checked(&blank, &policy()).unwrap();
        assert_eq!(settings.web_url, None);
        assert_eq!(settings.link_base(), "http://192.168.1.13:8080/");
        let with = |field: &str, value: serde_json::Value| {
            let mut settings = good.clone();
            settings[field] = value;
            settings
        };
        for (settings, says) in [
            (with("base_url", "http://127.0.0.1:8080".into()), "base_url"),
            (with("web_url", "javascript:alert(1)".into()), "web_url"),
            (with("web_url", "https://u:p@romm/".into()), "web_url"),
            (with("web_url", "https://romm/?x=1".into()), "web_url"),
            (with("token", "rmm_x".into()), "do not fit"),
        ] {
            let error = checked_settings(ConfigurableKind::RommApi, &settings, &roots(), &policy())
                .expect_err("refused");
            assert!(error.0.contains(says), "{settings}: {error}");
        }
    }

    #[test]
    fn only_known_kinds_are_configured_and_each_names_its_secrets() {
        assert_eq!(
            ConfigurableKind::parse("qbittorrent"),
            Some(ConfigurableKind::Qbittorrent)
        );
        assert_eq!(
            ConfigurableKind::parse("sabnzbd"),
            Some(ConfigurableKind::Sabnzbd)
        );
        assert_eq!(ConfigurableKind::Sabnzbd.secret_fields(), ["api_key"]);
        assert_eq!(
            ConfigurableKind::parse("nzbget"),
            Some(ConfigurableKind::Nzbget)
        );
        assert_eq!(ConfigurableKind::Nzbget.secret_fields(), ["password"]);
        assert_eq!(ConfigurableKind::parse("s3"), Some(ConfigurableKind::S3));
        assert_eq!(
            ConfigurableKind::S3.secret_fields(),
            ["access_key_id", "secret_access_key"]
        );
        assert_eq!(
            ConfigurableKind::parse("romm_api"),
            Some(ConfigurableKind::RommApi)
        );
        assert_eq!(ConfigurableKind::RommApi.secret_fields(), ["token"]);
        for other in ["romm", "webhook", "peer_instance", ""] {
            assert_eq!(ConfigurableKind::parse(other), None, "{other}");
        }
        assert_eq!(ConfigurableKind::Qbittorrent.secret_fields(), ["password"]);
    }
}
