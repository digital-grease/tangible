// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! A qBittorrent WebUI (API v2) the operator already runs
//! (docs/plans/acquisition.md A4, ADR 0016).
//!
//! Three things are asked of it, and nothing else: sign in, add a torrent the
//! operator supplied to Tangible's category, and say how one torrent is
//! doing, by infohash. Tangible never searches, never removes a torrent or
//! its data, and never asks the client to run anything. Every request goes
//! through the outbound client, under the integrations policy (the
//! administrator's address, so the LAN is allowed).
//!
//! What it answers is untrusted: bodies are read up to a bound, the JSON is
//! parsed into the few fields used, and a path it reports is only ever
//! mapped through [`relative_to`] onto a watched root, never opened as given.
//!
//! Behaviour recorded from qBittorrent 5.1.2 (API 2.11.4), in
//! fixtures/tool-output/qbittorrent:
//!
//! - `auth/login` answers 200 `Ok.` with an `SID` cookie, or 200 `Fails.`
//!   for a wrong password. With host header validation on (the default), a
//!   `Host` whose port is not the WebUI's own is refused with 401 before the
//!   password is looked at.
//! - Without a session, the API answers 401; with a stale one, 403.
//! - `torrents/add` answers 200 `Ok.`, or 200 `Fails.` when it already has
//!   the torrent. It makes the category if there is none.
//! - A finished torrent reports `progress` 1, an `UP` state, and
//!   `content_path`, absolute as the client sees it: the file for a
//!   single-file torrent, its folder otherwise. A magnet still fetching
//!   metadata reports an empty `content_path`.

use std::fmt;

use serde::Deserialize;
use tangible_domain::SecretString;

use crate::download::{ClientError, Phase, multipart, read_text};
use crate::outbound::{OutboundClient, OutboundError, Url};
use crate::torrent::Infohash;

/// The client, in words, for errors.
const CLIENT: &str = "qBittorrent";

fn outbound(error: OutboundError) -> ClientError {
    ClientError::from_outbound(CLIENT, error)
}

/// What a torrent submission hands the client.
pub enum TorrentSource<'a> {
    /// A `.torrent` file's bytes.
    File(&'a [u8]),
    /// A magnet link.
    Magnet(&'a str),
}

/// What the client said to an add.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Added {
    /// It took the torrent.
    Added,
    /// It declined: usually because it already has it. The caller asks
    /// after it by hash to tell.
    Declined,
}

/// One torrent, as much of it as Tangible reads.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TorrentStatus {
    /// Its infohash, as the client names it.
    pub hash: String,
    /// Such as `downloading`, `stalledUP`, `error`.
    pub state: String,
    /// From 0 to 1.
    pub progress: f64,
    /// Where it is, absolute, as the client sees it. Empty until the
    /// client has its metadata.
    #[serde(default)]
    pub content_path: String,
    /// Bytes it will hold when done.
    #[serde(default)]
    pub size: i64,
    /// Bytes still to fetch.
    #[serde(default)]
    pub amount_left: i64,
    /// Whether it has its metadata yet (a magnet starts without).
    #[serde(default = "yes")]
    pub has_metadata: bool,
    /// Its category.
    #[serde(default)]
    pub category: String,
}

const fn yes() -> bool {
    true
}

/// States in which a torrent's data is complete and resting.
const FINISHED_STATES: &[&str] = &[
    "uploading",
    "stalledUP",
    "pausedUP",
    "stoppedUP",
    "queuedUP",
    "forcedUP",
];

/// States in which the client has given up on a torrent.
const FAILED_STATES: &[&str] = &["error", "missingFiles"];

impl TorrentStatus {
    /// Where it stands. Done means every byte present, the client resting
    /// on it (not checking or moving it), and a path to read it from.
    #[must_use]
    pub fn phase(&self) -> Phase {
        if FAILED_STATES.contains(&self.state.as_str()) {
            return Phase::Failed(self.state.chars().take(64).collect());
        }
        let finished = self.progress >= 1.0
            && self.amount_left == 0
            && FINISHED_STATES.contains(&self.state.as_str())
            && !self.content_path.is_empty();
        if finished {
            return Phase::Complete {
                content_path: self.content_path.clone(),
                size: self.size,
            };
        }
        let total = (self.has_metadata && self.size > 0).then_some(self.size);
        Phase::Downloading {
            done: total.map(|total| (total - self.amount_left).clamp(0, total)),
            total,
        }
    }
}

/// A qBittorrent WebUI, signed in to on first use.
pub struct QbittorrentClient {
    http: OutboundClient,
    base: Url,
    username: String,
    password: Option<SecretString>,
    session: Option<SecretString>,
}

impl fmt::Debug for QbittorrentClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QbittorrentClient")
            .field("base", &crate::outbound::redacted(&self.base))
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

impl QbittorrentClient {
    /// A client for the WebUI at `base`. Without a password, no sign-in is
    /// attempted, for a WebUI that lets this address in without one.
    #[must_use]
    pub fn new(
        http: OutboundClient,
        mut base: Url,
        username: &str,
        password: Option<SecretString>,
    ) -> Self {
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        Self {
            http,
            base,
            username: username.to_owned(),
            password,
            session: None,
        }
    }

    fn endpoint(&self, path: &str) -> Result<Url, ClientError> {
        self.base
            .join(path)
            .map_err(|_| ClientError::bad(CLIENT, "the address cannot be joined".to_owned()))
    }

    /// Sign in, if there is a password and no session yet.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn sign_in(&mut self) -> Result<(), ClientError> {
        if self.session.is_some() {
            return Ok(());
        }
        let Some(password) = &self.password else {
            return Ok(());
        };
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("username", &self.username)
            .append_pair("password", password.expose())
            .finish();
        let request = self
            .http
            .request(reqwest::Method::POST, &self.endpoint("api/v2/auth/login")?)
            .map_err(outbound)?
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body);
        let response = self.http.send(request).await.map_err(outbound)?;
        let status = response.status();
        let session = session_cookie(response.headers());
        let text = read_text(CLIENT, response).await?;
        match (status.as_u16(), text.trim()) {
            (200, "Ok.") => {
                self.session = Some(session.ok_or_else(|| {
                    ClientError::bad(CLIENT, "signed in without a session cookie".to_owned())
                })?);
                Ok(())
            }
            (200, "Fails.") => Err(ClientError::Auth {
                client: CLIENT,
                reason: "the user name or password is wrong",
            }),
            (401, _) => Err(ClientError::Auth {
                client: CLIENT,
                reason: "it turned the request away before signing in; with host header \
                         validation on, the WebUI address must use the port qBittorrent \
                         itself listens on",
            }),
            (403, _) => Err(ClientError::Auth {
                client: CLIENT,
                reason: "it is refusing this address, possibly after too many failed sign-ins",
            }),
            (code, _) => Err(ClientError::bad(
                CLIENT,
                format!("signing in answered {code}"),
            )),
        }
    }

    async fn call(
        &mut self,
        request: impl Fn(&Self) -> Result<reqwest::RequestBuilder, ClientError>,
    ) -> Result<(u16, String), ClientError> {
        self.sign_in().await?;
        for attempt in 0..2 {
            let mut builder = request(self)?;
            if let Some(session) = &self.session {
                builder = builder.header("cookie", format!("SID={}", session.expose()));
            }
            let response = self.http.send(builder).await.map_err(outbound)?;
            let status = response.status().as_u16();
            if matches!(status, 401 | 403) {
                // No session, or a stale one: sign in afresh, once.
                if attempt == 0 && self.password.is_some() {
                    self.session = None;
                    self.sign_in().await?;
                    continue;
                }
                return Err(ClientError::Auth {
                    client: CLIENT,
                    reason: if self.password.is_some() {
                        "it refused the session it gave"
                    } else {
                        "it needs a password, and none is set"
                    },
                });
            }
            let text = read_text(CLIENT, response).await?;
            return Ok((status, text));
        }
        Err(ClientError::Auth {
            client: CLIENT,
            reason: "it refused the session it gave",
        })
    }

    /// The WebUI API's version, such as `2.11.4`.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn api_version(&mut self) -> Result<String, ClientError> {
        let url = self.endpoint("api/v2/app/webapiVersion")?;
        let (status, text) = self
            .call(|this| this.http.get(&url).map_err(outbound))
            .await?;
        let version = text.trim();
        let plausible = status == 200
            && !version.is_empty()
            && version.len() <= 32
            && version.bytes().all(|b| b.is_ascii_digit() || b == b'.');
        if plausible {
            Ok(version.to_owned())
        } else {
            Err(ClientError::bad(
                CLIENT,
                format!("the API version request answered {status}; is this a qBittorrent WebUI?"),
            ))
        }
    }

    /// Add a torrent to `category`, started, with automatic torrent
    /// management so the category's folder is used. Nothing else is set:
    /// no rename, no script, no save path of Tangible's choosing.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn add(
        &mut self,
        source: &TorrentSource<'_>,
        category: &str,
    ) -> Result<Added, ClientError> {
        let url = self.endpoint("api/v2/torrents/add")?;
        let (content_type, body) = add_body(source, category);
        let (status, text) = self
            .call(|this| {
                Ok(this
                    .http
                    .request(reqwest::Method::POST, &url)
                    .map_err(outbound)?
                    .header("content-type", content_type.clone())
                    .body(body.clone()))
            })
            .await?;
        match (status, text.trim()) {
            (200, "Fails.") | (415, _) => Ok(Added::Declined),
            (200, _) => Ok(Added::Added),
            (code, _) => Err(ClientError::bad(
                CLIENT,
                format!("adding the torrent answered {code}"),
            )),
        }
    }

    /// The torrent with `hash`, if the client has it.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn torrent(&mut self, hash: &Infohash) -> Result<Option<TorrentStatus>, ClientError> {
        let mut url = self.endpoint("api/v2/torrents/info")?;
        url.query_pairs_mut().append_pair("hashes", hash.as_str());
        let (status, text) = self
            .call(|this| this.http.get(&url).map_err(outbound))
            .await?;
        if status != 200 {
            return Err(ClientError::bad(
                CLIENT,
                format!("asking after the torrent answered {status}"),
            ));
        }
        parse_torrents(&text).map(|torrents| {
            torrents
                .into_iter()
                .find(|torrent| torrent.hash.eq_ignore_ascii_case(hash.as_str()))
        })
    }
}

/// The torrents in a `torrents/info` answer.
///
/// # Errors
///
/// [`ClientError::BadResponse`] for an answer that is not a list of
/// torrents.
pub fn parse_torrents(text: &str) -> Result<Vec<TorrentStatus>, ClientError> {
    serde_json::from_str(text)
        .map_err(|_| ClientError::bad(CLIENT, "the torrent list did not parse".to_owned()))
}

fn add_body(source: &TorrentSource<'_>, category: &str) -> (String, Vec<u8>) {
    let fields = [
        ("category", category),
        ("autoTMM", "true"),
        // `paused` until API 2.11, `stopped` since; each ignores the other.
        ("paused", "false"),
        ("stopped", "false"),
    ];
    match source {
        TorrentSource::Magnet(link) => {
            let mut form = url::form_urlencoded::Serializer::new(String::new());
            form.append_pair("urls", link);
            for (name, value) in fields {
                form.append_pair(name, value);
            }
            (
                "application/x-www-form-urlencoded".to_owned(),
                form.finish().into_bytes(),
            )
        }
        TorrentSource::File(bytes) => multipart(
            &fields,
            "torrents",
            "submitted.torrent",
            "application/x-bittorrent",
            bytes,
        ),
    }
}

fn session_cookie(headers: &reqwest::header::HeaderMap) -> Option<SecretString> {
    headers
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|cookie| {
            let first = cookie.split(';').next()?.trim();
            let value = first.strip_prefix("SID=")?;
            let usable = !value.is_empty()
                && value.len() <= 256
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"+/=-_.".contains(&b));
            usable.then(|| SecretString::from(value.to_owned()))
        })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn recorded() -> Vec<TorrentStatus> {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/tool-output/qbittorrent/torrents-info-5.1.2.json"
        ))
        .unwrap();
        parse_torrents(&text).unwrap()
    }

    #[test]
    fn a_recorded_answer_reads_as_finished_or_fetching() {
        let torrents = recorded();
        let by_hash = |prefix: &str| {
            torrents
                .iter()
                .find(|t| t.hash.starts_with(prefix))
                .unwrap()
                .phase()
        };
        assert_eq!(
            by_hash("2376"),
            Phase::Complete {
                content_path: "/downloads/tangible/probe-disc.iso".to_owned(),
                size: 300_000,
            }
        );
        assert_eq!(
            by_hash("ad98"),
            Phase::Complete {
                content_path: "/downloads/tangible/Probe Disc (CUE)".to_owned(),
                size: 100_067,
            }
        );
        // A magnet with no peers: no metadata, no path, no size yet.
        assert_eq!(
            by_hash("0123"),
            Phase::Downloading {
                done: None,
                total: None
            }
        );
    }

    fn status(state: &str, progress: f64, amount_left: i64) -> TorrentStatus {
        TorrentStatus {
            hash: "a".repeat(40),
            state: state.to_owned(),
            progress,
            content_path: "/downloads/tangible/x.iso".to_owned(),
            size: 1000,
            amount_left,
            has_metadata: true,
            category: "tangible".to_owned(),
        }
    }

    #[test]
    fn only_a_resting_complete_torrent_is_done() {
        assert_eq!(
            status("downloading", 0.25, 750).phase(),
            Phase::Downloading {
                done: Some(250),
                total: Some(1000)
            }
        );
        for resting in FINISHED_STATES {
            assert!(
                matches!(status(resting, 1.0, 0).phase(), Phase::Complete { .. }),
                "{resting}"
            );
        }
        for busy in ["checkingUP", "moving", "checkingResumeData", "metaDL"] {
            assert!(
                matches!(status(busy, 1.0, 0).phase(), Phase::Downloading { .. }),
                "{busy}"
            );
        }
        assert!(matches!(
            status("stalledUP", 0.999, 1).phase(),
            Phase::Downloading { .. }
        ));
        for failed in FAILED_STATES {
            assert_eq!(
                status(failed, 1.0, 0).phase(),
                Phase::Failed((*failed).to_owned())
            );
        }
        let mut no_path = status("stalledUP", 1.0, 0);
        no_path.content_path.clear();
        assert!(matches!(no_path.phase(), Phase::Downloading { .. }));
    }

    #[test]
    fn junk_is_not_a_torrent_list() {
        for junk in ["", "Forbidden", "{}", "[{\"hash\": 1}]"] {
            assert!(parse_torrents(junk).is_err(), "{junk}");
        }
        assert_eq!(parse_torrents("[]").unwrap(), Vec::new());
    }

    #[test]
    fn a_torrent_goes_as_a_file_part_and_a_magnet_as_a_form() {
        let torrent = b"d4:infod6:pieces20:aaaaaaaaaaaaaaaaaaaaee";
        let (content_type, body) = add_body(&TorrentSource::File(torrent), "tangible");
        let boundary = content_type
            .strip_prefix("multipart/form-data; boundary=")
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"category\"\r\n\r\ntangible\r\n"));
        assert!(text.contains("name=\"autoTMM\"\r\n\r\ntrue\r\n"));
        assert!(text.contains("filename=\"submitted.torrent\""));
        assert!(text.ends_with(&format!("--{boundary}--\r\n")));
        assert!(body.windows(torrent.len()).any(|w| w == torrent));

        let (content_type, body) = add_body(
            &TorrentSource::Magnet("magnet:?xt=urn:btih:abc&dn=a b"),
            "my cat",
        );
        assert_eq!(content_type, "application/x-www-form-urlencoded");
        let text = String::from_utf8(body).unwrap();
        assert!(text.starts_with("urls=magnet%3A%3Fxt%3Durn%3Abtih%3Aabc%26dn%3Da+b&"));
        assert!(text.contains("category=my+cat"));
        assert!(text.contains("stopped=false"));
    }

    #[test]
    fn the_session_cookie_is_found_and_kept_secret() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.append("set-cookie", "other=1; path=/".parse().unwrap());
        headers.append(
            "set-cookie",
            "SID=AbC123+/=; HttpOnly; SameSite=Strict; path=/"
                .parse()
                .unwrap(),
        );
        let session = session_cookie(&headers).unwrap();
        assert_eq!(session.expose(), "AbC123+/=");
        assert!(!format!("{session:?}").contains("AbC123"));
        let mut bad = reqwest::header::HeaderMap::new();
        bad.append("set-cookie", "SID=a\"b; path=/".parse().unwrap());
        assert!(session_cookie(&bad).is_none());
    }
}
