// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! A Prowlarr the operator already runs (ADR 0021, docs/plans/requests.md
//! Q3).
//!
//! Asked four things: its version, its indexers and which are failing, a
//! search, and, for the release a person picks, the `.torrent`, `.nzb` or
//! magnet link behind its download link. Never to add, change or remove an
//! indexer, and never to send anything to a download client itself: what
//! is fetched goes to one of Tangible's own (ADR 0021, decision 6).
//!
//! The API key goes in the `X-Api-Key` header, never in a URL Tangible
//! builds. Prowlarr's own download links carry it in their query, so a
//! [`Release`]'s links are never shown by its `Debug`, and the caller keeps
//! them sealed (ADR 0021, security). A fetch sends no key header: the link
//! carries what Prowlarr needs, and a redirect may lead to an indexer.
//!
//! Behaviour recorded from Prowlarr 2.7.0.5723 against a stand-in Torznab
//! and Newznab indexer, in fixtures/tool-output/prowlarr:
//!
//! - Without the right key, every route answers 401 with an empty body.
//! - `GET /api/v1/search?query&type&categories&limit` answers an array of
//!   releases. A film is found by IMDb ID with `type=movie` and
//!   `query={ImdbId:tt…}`, which Prowlarr sends an indexer as `imdbid`.
//! - An indexer that fails answers nothing: the search is a 200 with what
//!   the others found, and Prowlarr pauses the indexer for a while
//!   (`/api/v1/indexerstatus` names it, with `disabledTill`). A search that
//!   names only paused or unknown indexers is a 400 whose `message` says
//!   so.
//! - A release's `downloadUrl` is Prowlarr's own link, with its key in the
//!   query. For a `.torrent` it answers the file; for a magnet-only release
//!   a 301 to the `magnet:` link. A Usenet indexer must redirect (Prowlarr
//!   refuses otherwise), so an `.nzb` is a 301 to the indexer's own link,
//!   with the indexer's key in it.

use std::fmt;
use std::future::Future;
use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use tangible_domain::SecretString;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::download::{ClientError, read_bytes_within, read_text_within};
use crate::metadata::clean;
use crate::nzb::MAX_NZB_BYTES;
use crate::outbound::{MAX_REDIRECTS, OutboundClient, OutboundError, Refusal, Url};
use crate::torrent::{MAX_MAGNET_CHARS, MAX_TORRENT_BYTES};

/// The client, in words, for errors.
const CLIENT: &str = "Prowlarr";

/// The most results asked of each indexer, and the most kept from one
/// search.
pub const MAX_RESULTS: usize = 100;

/// The most of a search's answer read: a hundred results from each of a
/// few dozen indexers.
pub const MAX_SEARCH_BYTES: usize = 16 * 1024 * 1024;

/// The most of a release's file fetched: the larger of a `.torrent` and an
/// NZB at their limits. Which it is is checked by the caller's parsers.
pub const MAX_FETCH_BYTES: usize = if MAX_NZB_BYTES > MAX_TORRENT_BYTES {
    MAX_NZB_BYTES
} else {
    MAX_TORRENT_BYTES
};

/// The longest release title kept.
pub const MAX_TITLE_CHARS: usize = 300;

/// The longest indexer or category name kept.
pub const MAX_NAME_CHARS: usize = 100;

/// The most categories kept for one release.
pub const MAX_CATEGORIES: usize = 8;

/// The longest link to a release kept, Prowlarr's own or a magnet link.
pub const MAX_LINK_CHARS: usize = MAX_MAGNET_CHARS;

/// How long Prowlarr has to answer a search: it waits on every indexer.
pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(120);

/// How long it has to answer anything else.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(20);

/// How long fetching a release's file may take, every redirect included.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// `work`, or `Unreachable` once `limit` has passed.
async fn within<T>(
    limit: Duration,
    work: impl Future<Output = Result<T, ClientError>>,
) -> Result<T, ClientError> {
    tokio::time::timeout(limit, work).await.unwrap_or_else(|_| {
        Err(ClientError::Unreachable {
            client: CLIENT,
            detail: format!("it did not answer within {} seconds", limit.as_secs()),
        })
    })
}

fn outbound(error: OutboundError) -> ClientError {
    ClientError::from_outbound(CLIENT, error)
}

/// How a release is fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// BitTorrent: a `.torrent` or a magnet link.
    Torrent,
    /// Usenet: an NZB.
    Usenet,
}

impl Protocol {
    /// As Prowlarr and the API say it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Torrent => "torrent",
            Self::Usenet => "usenet",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "torrent" => Some(Self::Torrent),
            "usenet" => Some(Self::Usenet),
            _ => None,
        }
    }
}

/// What Prowlarr is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// Its version, such as `2.7.0.5723`.
    pub version: String,
}

/// One of Prowlarr's indexers, as far as Tangible needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Indexer {
    /// Prowlarr's id for it.
    pub id: i64,
    /// Its name, in Prowlarr.
    pub name: String,
    /// Whether it is turned on in Prowlarr.
    pub enabled: bool,
    /// What it indexes.
    pub protocol: Option<Protocol>,
}

/// An indexer Prowlarr has paused after it failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paused {
    /// Prowlarr's id for the indexer.
    pub indexer_id: i64,
    /// Until when, if Prowlarr said.
    pub until: Option<OffsetDateTime>,
}

/// What kind of search to make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchType {
    /// Words, in the categories given.
    Search,
    /// A film by its IMDb ID.
    Movie,
}

impl SearchType {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Movie => "movie",
        }
    }
}

/// A search to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Search {
    /// The words, or for [`SearchType::Movie`] `{ImdbId:tt…}`.
    pub query: String,
    /// What kind.
    pub kind: SearchType,
    /// Newznab categories to search in; none for every one.
    pub categories: Vec<u32>,
}

impl Search {
    /// A search for `words` in `categories`.
    #[must_use]
    pub fn words(words: &str, categories: &[u32]) -> Self {
        Self {
            query: words.to_owned(),
            kind: SearchType::Search,
            categories: categories.to_vec(),
        }
    }

    /// A search for the film `imdb_id` (`tt` and its digits), in
    /// `categories`.
    #[must_use]
    pub fn film(imdb_id: &str, categories: &[u32]) -> Self {
        Self {
            query: format!("{{ImdbId:{imdb_id}}}"),
            kind: SearchType::Movie,
            categories: categories.to_vec(),
        }
    }
}

/// A category a release is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Category {
    /// Its Newznab number, such as 2000.
    pub id: u32,
    /// Its name, such as `Movies/DVD`.
    pub name: String,
}

/// A release an indexer has, as Prowlarr found it. Every text field is one
/// line of plain text, bounded; the links are never shown by `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct Release {
    /// The indexer's identifier for it, for telling results apart.
    pub guid: String,
    /// Prowlarr's id for the indexer that has it.
    pub indexer_id: i64,
    /// That indexer's name.
    pub indexer: String,
    /// Its title, as the indexer gives it.
    pub title: String,
    /// How it is fetched.
    pub protocol: Protocol,
    /// Its size, when given.
    pub size_bytes: Option<u64>,
    /// A torrent's seeders, when given.
    pub seeders: Option<u32>,
    /// A torrent's leechers, when given.
    pub leechers: Option<u32>,
    /// How often it was grabbed, when given.
    pub grabs: Option<u32>,
    /// When it was posted, when given.
    pub published_at: Option<OffsetDateTime>,
    /// Its categories that have a name.
    pub categories: Vec<Category>,
    /// A torrent's infohash, lowercase hex, when given.
    pub info_hash: Option<String>,
    /// Prowlarr's link to its file (a `.torrent` or an NZB), which may
    /// redirect to a magnet link. Carries Prowlarr's key.
    pub download: Option<Url>,
    /// Its magnet link, or Prowlarr's link to one. May carry the key.
    pub magnet: Option<Url>,
}

impl fmt::Debug for Release {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Release")
            .field("guid", &self.guid)
            .field("indexer", &self.indexer)
            .field("title", &self.title)
            .field("protocol", &self.protocol)
            .field("size_bytes", &self.size_bytes)
            .field("download", &self.download.as_ref().map(|_| "<sealed>"))
            .field("magnet", &self.magnet.as_ref().map(|_| "<sealed>"))
            .finish_non_exhaustive()
    }
}

/// What a release's link gave.
#[derive(Clone, PartialEq, Eq)]
pub enum Fetched {
    /// A magnet link, as it was redirected to.
    Magnet(String),
    /// A file: a `.torrent` or an NZB, for the caller to check.
    File(Vec<u8>),
}

impl fmt::Debug for Fetched {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Magnet(_) => f.write_str("Magnet(<link>)"),
            Self::File(bytes) => write!(f, "File({} bytes)", bytes.len()),
        }
    }
}

/// A Prowlarr's API.
pub struct ProwlarrClient {
    http: OutboundClient,
    base: Url,
    api_key: SecretString,
}

impl fmt::Debug for ProwlarrClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProwlarrClient")
            .field("base", &crate::outbound::redacted(&self.base))
            .finish_non_exhaustive()
    }
}

impl ProwlarrClient {
    /// A client for the Prowlarr at `base` (with any URL base it is served
    /// under), reached through `http`.
    #[must_use]
    pub fn new(http: OutboundClient, mut base: Url, api_key: SecretString) -> Self {
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        Self {
            http,
            base,
            api_key,
        }
    }

    fn url(&self, path: &str, query: &[(&str, String)]) -> Result<Url, ClientError> {
        let mut url = self
            .base
            .join(path)
            .map_err(|_| ClientError::bad(CLIENT, "the address cannot be joined"))?;
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(name, value)| (*name, value.as_str())));
        }
        Ok(url)
    }

    /// GET an API route with the key, answering its body as text, within
    /// `limit`.
    async fn get(&self, url: &Url, max: usize, limit: Duration) -> Result<String, ClientError> {
        within(limit, self.get_now(url, max)).await
    }

    async fn get_now(&self, url: &Url, max: usize) -> Result<String, ClientError> {
        let request = self
            .http
            .request(reqwest::Method::GET, url)
            .map_err(outbound)?
            .header("x-api-key", self.api_key.expose())
            .header("accept", "application/json");
        let response = self.http.send(request).await.map_err(outbound)?;
        let status = response.status();
        let text = read_text_within(CLIENT, response, max).await?;
        match status {
            StatusCode::OK => Ok(text),
            StatusCode::UNAUTHORIZED => Err(ClientError::Auth {
                client: CLIENT,
                reason: "the API key is wrong; Prowlarr shows it under Settings, General",
            }),
            StatusCode::BAD_REQUEST => Err(ClientError::bad(
                CLIENT,
                refused_message(&text).unwrap_or_else(|| "it refused the request".to_owned()),
            )),
            other => Err(ClientError::bad(
                CLIENT,
                format!("it answered HTTP {}", other.as_u16()),
            )),
        }
    }

    /// Its version.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn status(&self) -> Result<Status, ClientError> {
        let text = self
            .get(
                &self.url("api/v1/system/status", &[])?,
                64 * 1024,
                ANSWER_TIMEOUT,
            )
            .await?;
        parse_status(&text)
    }

    /// Its indexers.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn indexers(&self) -> Result<Vec<Indexer>, ClientError> {
        let text = self
            .get(
                &self.url("api/v1/indexer", &[])?,
                MAX_SEARCH_BYTES,
                ANSWER_TIMEOUT,
            )
            .await?;
        parse_indexers(&text)
    }

    /// The indexers it has paused after they failed.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn paused(&self) -> Result<Vec<Paused>, ClientError> {
        let text = self
            .get(
                &self.url("api/v1/indexerstatus", &[])?,
                1024 * 1024,
                ANSWER_TIMEOUT,
            )
            .await?;
        parse_paused(&text)
    }

    /// Search every indexer it has turned on.
    ///
    /// # Errors
    ///
    /// [`ClientError`]: a `BadResponse` saying why for one Prowlarr refused.
    pub async fn search(&self, search: &Search) -> Result<Vec<Release>, ClientError> {
        let mut query = vec![
            ("query", search.query.clone()),
            ("type", search.kind.as_str().to_owned()),
        ];
        query.extend(
            search
                .categories
                .iter()
                .map(|category| ("categories", category.to_string())),
        );
        query.push(("limit", MAX_RESULTS.to_string()));
        let text = self
            .get(
                &self.url("api/v1/search", &query)?,
                MAX_SEARCH_BYTES,
                SEARCH_TIMEOUT,
            )
            .await?;
        parse_releases(&text)
    }

    /// What a release's link gives: the file, or the magnet link it
    /// redirects to, within [`FETCH_TIMEOUT`]. Each hop is checked by the
    /// policy, as a redirect the client followed would be, at most
    /// [`MAX_REDIRECTS`] of them and none from https to http. No key header
    /// is sent.
    ///
    /// # Errors
    ///
    /// [`ClientError`]: `Refused` for a hop the policy refuses, `Auth` for a
    /// link refused, `BadResponse` for anything else unusable.
    pub async fn fetch(&self, link: &Url) -> Result<Fetched, ClientError> {
        within(FETCH_TIMEOUT, self.fetch_now(link)).await
    }

    async fn fetch_now(&self, link: &Url) -> Result<Fetched, ClientError> {
        let http = self.http.without_redirects().map_err(outbound)?;
        let mut url = link.clone();
        for _ in 0..=MAX_REDIRECTS {
            if url.scheme() == "magnet" {
                return magnet(url.as_str());
            }
            let request = http.get(&url).map_err(outbound)?;
            let response = http.send(request).await.map_err(outbound)?;
            let status = response.status();
            if status.is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| ClientError::bad(CLIENT, "the link redirected nowhere"))?
                    .trim()
                    .to_owned();
                if location.len() > MAX_LINK_CHARS {
                    return Err(ClientError::bad(CLIENT, "the link redirected too far"));
                }
                if location
                    .get(..7)
                    .is_some_and(|scheme| scheme.eq_ignore_ascii_case("magnet:"))
                {
                    return magnet(&location);
                }
                let next = url.join(&location).map_err(|_| {
                    ClientError::bad(CLIENT, "the link redirected to something that is not one")
                })?;
                if url.scheme() == "https" && next.scheme() == "http" {
                    return Err(ClientError::Refused(Refusal::Downgrade));
                }
                url = next;
                continue;
            }
            return match status {
                StatusCode::OK => Ok(Fetched::File(
                    read_bytes_within(CLIENT, response, MAX_FETCH_BYTES).await?,
                )),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(ClientError::Auth {
                    client: CLIENT,
                    reason: "the release's link was refused; search again, and if it is \
                             refused again, the indexer may be out of grabs",
                }),
                StatusCode::NOT_FOUND | StatusCode::GONE => Err(ClientError::bad(
                    CLIENT,
                    "the release is no longer there; search again",
                )),
                other => Err(ClientError::bad(
                    CLIENT,
                    format!("the release's link answered HTTP {}", other.as_u16()),
                )),
            };
        }
        Err(ClientError::Refused(Refusal::TooManyRedirects))
    }
}

fn magnet(link: &str) -> Result<Fetched, ClientError> {
    if link.len() > MAX_MAGNET_CHARS || link.chars().any(char::is_control) {
        return Err(ClientError::bad(CLIENT, "the magnet link will not do"));
    }
    Ok(Fetched::Magnet(link.to_owned()))
}

/// Prowlarr's `message` in a refusal, made one line.
fn refused_message(text: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Refused {
        message: Option<String>,
    }
    serde_json::from_str::<Refused>(text)
        .ok()?
        .message
        .and_then(|message| clean(&message, 300))
}

fn parse_status(text: &str) -> Result<Status, ClientError> {
    #[derive(Deserialize)]
    struct Raw {
        version: Option<String>,
    }
    let raw: Raw = serde_json::from_str(text)
        .map_err(|_| ClientError::bad(CLIENT, "its status was not what Prowlarr answers"))?;
    let version = raw
        .version
        .and_then(|version| clean(&version, 40))
        .ok_or_else(|| ClientError::bad(CLIENT, "its status named no version"))?;
    Ok(Status { version })
}

/// Its answer as a JSON array, each item as it is: an item that will not
/// parse is left out rather than losing the rest.
fn items(text: &str, what: &str) -> Result<Vec<serde_json::Value>, ClientError> {
    serde_json::from_str(text)
        .map_err(|_| ClientError::bad(CLIENT, format!("its {what} were not a list")))
}

fn parse_indexers(text: &str) -> Result<Vec<Indexer>, ClientError> {
    #[derive(Deserialize)]
    struct Raw {
        id: i64,
        name: Option<String>,
        #[serde(default)]
        enable: bool,
        protocol: Option<String>,
    }
    Ok(items(text, "indexers")?
        .into_iter()
        .filter_map(|item| serde_json::from_value::<Raw>(item).ok())
        .map(|raw| Indexer {
            id: raw.id,
            name: raw
                .name
                .and_then(|name| clean(&name, MAX_NAME_CHARS))
                .unwrap_or_else(|| format!("indexer {}", raw.id)),
            enabled: raw.enable,
            protocol: raw.protocol.as_deref().and_then(Protocol::parse),
        })
        .collect())
}

fn parse_paused(text: &str) -> Result<Vec<Paused>, ClientError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Raw {
        indexer_id: i64,
        disabled_till: Option<String>,
    }
    Ok(items(text, "indexers' statuses")?
        .into_iter()
        .filter_map(|item| serde_json::from_value::<Raw>(item).ok())
        .map(|raw| Paused {
            indexer_id: raw.indexer_id,
            until: raw.disabled_till.as_deref().and_then(timestamp),
        })
        .collect())
}

fn timestamp(text: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(text, &Rfc3339).ok()
}

/// A count Prowlarr gave, if it is one.
fn count(value: Option<i64>) -> Option<u32> {
    value.and_then(|value| u32::try_from(value).ok())
}

/// A link as given, if it is one Tangible would follow: http, https, or
/// (for a magnet) `magnet:`, and bounded.
fn link(text: Option<&str>, magnet_too: bool) -> Option<Url> {
    let text = text?.trim();
    if text.is_empty() || text.len() > MAX_LINK_CHARS {
        return None;
    }
    let url = Url::parse(text).ok()?;
    match url.scheme() {
        "http" | "https" => Some(url),
        "magnet" if magnet_too => Some(url),
        _ => None,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRelease {
    guid: Option<String>,
    indexer_id: Option<i64>,
    indexer: Option<String>,
    title: Option<String>,
    protocol: Option<String>,
    size: Option<i64>,
    seeders: Option<i64>,
    leechers: Option<i64>,
    grabs: Option<i64>,
    publish_date: Option<String>,
    #[serde(default)]
    categories: Vec<RawCategory>,
    info_hash: Option<String>,
    download_url: Option<String>,
    magnet_url: Option<String>,
}

#[derive(Deserialize)]
struct RawCategory {
    id: Option<i64>,
    name: Option<String>,
}

impl RawRelease {
    fn release(self) -> Option<Release> {
        let protocol = Protocol::parse(self.protocol.as_deref()?)?;
        let title = clean(self.title.as_deref()?, MAX_TITLE_CHARS)?;
        let guid = clean(self.guid.as_deref()?, MAX_LINK_CHARS)?;
        let download = link(self.download_url.as_deref(), true);
        let magnet = link(self.magnet_url.as_deref(), true);
        if download.is_none() && magnet.is_none() {
            return None;
        }
        let torrent = protocol == Protocol::Torrent;
        let indexer_id = self.indexer_id.unwrap_or(0);
        Some(Release {
            guid,
            indexer_id,
            indexer: self
                .indexer
                .and_then(|name| clean(&name, MAX_NAME_CHARS))
                .unwrap_or_else(|| format!("indexer {indexer_id}")),
            title,
            protocol,
            size_bytes: self
                .size
                .and_then(|size| u64::try_from(size).ok())
                .filter(|size| *size > 0),
            seeders: if torrent { count(self.seeders) } else { None },
            leechers: if torrent { count(self.leechers) } else { None },
            grabs: count(self.grabs),
            published_at: self.publish_date.as_deref().and_then(timestamp),
            categories: self
                .categories
                .into_iter()
                .filter_map(|category| {
                    Some(Category {
                        id: u32::try_from(category.id?).ok()?,
                        name: clean(category.name.as_deref()?, MAX_NAME_CHARS)?,
                    })
                })
                .take(MAX_CATEGORIES)
                .collect(),
            info_hash: self
                .info_hash
                .filter(|hash| hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
                .map(|hash| hash.to_ascii_lowercase()),
            download,
            magnet: if torrent { magnet } else { None },
        })
    }
}

/// A search's answer: each release Tangible can use, in Prowlarr's order;
/// one with no protocol, title or link it could follow is left out.
///
/// # Errors
///
/// [`ClientError::BadResponse`] for an answer that is not a list.
pub fn parse_releases(text: &str) -> Result<Vec<Release>, ClientError> {
    Ok(items(text, "results")?
        .into_iter()
        .filter_map(|item| serde_json::from_value::<RawRelease>(item).ok())
        .filter_map(RawRelease::release)
        .collect())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    const MOVIES: &str =
        include_str!("../../../fixtures/tool-output/prowlarr/search-movies-2.7.0.json");
    const IMDB: &str =
        include_str!("../../../fixtures/tool-output/prowlarr/search-imdb-2.7.0.json");
    const MAGNET_ONLY: &str =
        include_str!("../../../fixtures/tool-output/prowlarr/search-magnet-only-2.7.0.json");
    const USENET: &str =
        include_str!("../../../fixtures/tool-output/prowlarr/search-usenet-2.7.0.json");
    const REFUSED: &str =
        include_str!("../../../fixtures/tool-output/prowlarr/search-refused-2.7.0.json");
    const STATUS: &str =
        include_str!("../../../fixtures/tool-output/prowlarr/system-status-2.7.0.json");
    const INDEXERS: &str =
        include_str!("../../../fixtures/tool-output/prowlarr/indexers-2.7.0.json");
    const PAUSED: &str =
        include_str!("../../../fixtures/tool-output/prowlarr/indexer-status-failing-2.7.0.json");

    #[test]
    fn a_recorded_search_is_read_into_releases() {
        let releases = parse_releases(MOVIES).unwrap();
        assert_eq!(releases.len(), 3);
        let bluray = &releases[0];
        assert_eq!(bluray.title, "Big Buck Bunny (2008) [Blu-ray ISO]");
        assert_eq!(bluray.indexer, "Fake Torznab");
        assert_eq!(bluray.indexer_id, 1);
        assert_eq!(bluray.protocol, Protocol::Torrent);
        assert_eq!(bluray.size_bytes, Some(23_622_320_128));
        assert_eq!((bluray.seeders, bluray.leechers), (Some(48), Some(4)));
        assert_eq!(
            bluray.published_at,
            Some(timestamp("2026-11-10T12:00:00Z").unwrap())
        );
        // The indexer's own category number (102000) has no name, and is
        // left out.
        assert_eq!(
            bluray.categories,
            vec![Category {
                id: 2000,
                name: "Movies".to_owned()
            }]
        );
        assert_eq!(
            bluray.info_hash.as_deref(),
            Some("15458c4d7d033b448e97218564b703e7791003c0")
        );
        let download = bluray.download.as_ref().unwrap();
        assert_eq!(download.path(), "/1/download");
        assert!(bluray.magnet.is_some());

        let usenet = &releases[2];
        assert_eq!(usenet.protocol, Protocol::Usenet);
        assert_eq!(usenet.title, "Big.Buck.Bunny.2008.DVD.ISO");
        assert_eq!((usenet.seeders, usenet.grabs), (None, Some(7)));
        assert!(usenet.magnet.is_none());
    }

    #[test]
    fn a_release_never_shows_its_links() {
        let releases = parse_releases(MOVIES).unwrap();
        let shown = format!("{releases:?}");
        assert!(
            !shown.contains("apikey") && !shown.contains("download?"),
            "{shown}"
        );
        assert!(shown.contains("<sealed>"));
        let fetched = format!("{:?}", Fetched::Magnet("magnet:?xt=urn:btih:a".to_owned()));
        assert!(!fetched.contains("btih"), "{fetched}");
    }

    #[test]
    fn a_film_found_by_imdb_id_and_a_magnet_only_release_are_read() {
        assert_eq!(parse_releases(IMDB).unwrap().len(), 3);
        let only = parse_releases(MAGNET_ONLY).unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(
            only[0].title,
            "Big Buck Bunny Soundtrack (2008) [FLAC CD image]"
        );
        assert_eq!(only[0].categories[0].name, "Audio/Lossless");
        let usenet = parse_releases(USENET).unwrap();
        assert_eq!(usenet[0].categories[0].id, 1000);
        // Only the named categories, and at most eight.
        assert!(usenet[0].categories.len() <= MAX_CATEGORIES);
    }

    #[test]
    fn a_film_search_names_its_imdb_id_as_prowlarr_takes_it() {
        let search = Search::film("tt1254207", &[2000]);
        assert_eq!(search.query, "{ImdbId:tt1254207}");
        assert_eq!(search.kind.as_str(), "movie");
        assert_eq!(Search::words("a", &[]).kind.as_str(), "search");
    }

    #[test]
    fn what_cannot_be_used_is_left_out_and_text_is_one_line() {
        let text = serde_json::json!([
            // No protocol.
            { "guid": "a", "title": "A", "downloadUrl": "http://p/1/download" },
            // A protocol Tangible does not fetch.
            { "guid": "b", "title": "B", "protocol": "unknown", "downloadUrl": "http://p/1" },
            // No link it could follow.
            { "guid": "c", "title": "C", "protocol": "torrent", "downloadUrl": "javascript:alert(1)" },
            // Not an object.
            "d",
            // Usable, with a title on several lines and a size that is not.
            {
                "guid": "e", "title": "E\n\u{7}two\tlines", "protocol": "torrent",
                "size": -1, "seeders": -3, "infoHash": "not-a-hash",
                "magnetUrl": "magnet:?xt=urn:btih:15458c4d7d033b448e97218564b703e7791003c0",
                "categories": [{ "id": 2000 }, { "name": "x" }, { "id": 2010, "name": "Movies/Foreign" }]
            }
        ])
        .to_string();
        let releases = parse_releases(&text).unwrap();
        assert_eq!(releases.len(), 1);
        let release = &releases[0];
        assert_eq!(release.title, "E two lines");
        assert_eq!(release.size_bytes, None);
        assert_eq!(release.seeders, None);
        assert_eq!(release.info_hash, None);
        assert_eq!(release.indexer, "indexer 0");
        assert_eq!(release.categories.len(), 1);
        assert!(release.download.is_none() && release.magnet.is_some());
        assert!(parse_releases("{}").is_err());
        assert!(parse_releases("[]").unwrap().is_empty());
    }

    #[test]
    fn a_refusal_says_what_prowlarr_said_without_its_stack() {
        assert_eq!(
            refused_message(REFUSED).as_deref(),
            Some("Search failed due to all selected indexers being unavailable")
        );
        assert_eq!(refused_message("not json"), None);
    }

    #[test]
    fn status_indexers_and_paused_indexers_are_read() {
        assert_eq!(parse_status(STATUS).unwrap().version, "2.7.0.5723");
        assert!(parse_status("{}").is_err());
        let indexers = parse_indexers(INDEXERS).unwrap();
        assert_eq!(indexers.len(), 2);
        let newznab = indexers.iter().find(|i| i.id == 2).unwrap();
        assert_eq!(newznab.name, "Fake Newznab");
        assert!(newznab.enabled);
        assert_eq!(newznab.protocol, Some(Protocol::Usenet));
        let paused = parse_paused(PAUSED).unwrap();
        assert_eq!(paused.len(), 2);
        assert_eq!(
            paused[0].until,
            Some(timestamp("2026-11-14T01:59:30Z").unwrap())
        );
    }

    /// A server answering each path with a raw response, keeping every
    /// request's head (its request line and headers).
    async fn server(
        answers: Vec<(&'static str, String)>,
    ) -> (
        std::net::SocketAddr,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let heads = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = std::sync::Arc::clone(&heads);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let answers = answers.clone();
                let seen = std::sync::Arc::clone(&seen);
                tokio::spawn(async move {
                    let mut buffer = [0_u8; 8192];
                    let read = stream.read(&mut buffer).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buffer[..read]).into_owned();
                    let path = head.split(' ').nth(1).unwrap_or_default().to_owned();
                    seen.lock().unwrap().push(head);
                    let response = answers
                        .iter()
                        .find(|(prefix, _)| path.starts_with(prefix))
                        .map_or_else(
                            || "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n".to_owned(),
                            |(_, response)| response.clone(),
                        );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        (address, heads)
    }

    fn redirect(to: &str) -> String {
        format!(
            "HTTP/1.1 301 Moved Permanently\r\nlocation: {to}\r\ncontent-length: 0\r\n\
             connection: close\r\n\r\n"
        )
    }

    fn file(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/x-bittorrent\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn prowlarr_at(address: std::net::SocketAddr) -> ProwlarrClient {
        let policy = crate::outbound::OutboundPolicy::for_integrations(
            vec!["127.0.0.1/32".parse().unwrap()],
            None,
        );
        let http = OutboundClient::new(policy, "tangible-test").unwrap();
        ProwlarrClient::new(
            http,
            Url::parse(&format!("http://127.0.0.1:{}/prowlarr", address.port())).unwrap(),
            SecretString::new("the-key"),
        )
    }

    #[tokio::test]
    async fn a_link_gives_its_file_or_the_magnet_it_redirects_to() {
        let magnet = "magnet:?xt=urn:btih:ed0d831bcefc077847bc1fa808bd2ae5fd32ae9b&dn=Big%20Buck";
        let (address, heads) = server(vec![
            ("/1/download?file=torrent", file("d8:announce0:e")),
            ("/1/download?file=magnet", redirect(magnet)),
            // An NZB: Prowlarr redirects to the indexer, which answers it.
            (
                "/2/download",
                redirect("/indexer/u1.nzb?apikey=indexer-key"),
            ),
            ("/indexer/u1.nzb", file("<nzb/>")),
            ("/loop", redirect("/loop")),
            (
                "/refused",
                "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n".to_owned(),
            ),
            (
                "/gone",
                "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n".to_owned(),
            ),
            ("/elsewhere", redirect("http://127.0.0.2:9/x")),
        ])
        .await;
        let prowlarr = prowlarr_at(address);
        let at =
            |path: &str| Url::parse(&format!("http://127.0.0.1:{}{path}", address.port())).unwrap();

        let fetched = prowlarr
            .fetch(&at("/1/download?file=torrent&apikey=the-key"))
            .await
            .unwrap();
        assert_eq!(fetched, Fetched::File(b"d8:announce0:e".to_vec()));
        assert_eq!(
            prowlarr
                .fetch(&at("/1/download?file=magnet"))
                .await
                .unwrap(),
            Fetched::Magnet(magnet.to_owned())
        );
        assert_eq!(
            prowlarr
                .fetch(&at("/2/download?apikey=the-key"))
                .await
                .unwrap(),
            Fetched::File(b"<nzb/>".to_vec())
        );
        // A magnet link given as the link is answered as it is.
        assert_eq!(
            prowlarr.fetch(&Url::parse(magnet).unwrap()).await.unwrap(),
            Fetched::Magnet(Url::parse(magnet).unwrap().to_string())
        );
        // No hop is sent the key in a header: the link carries it, and a
        // redirect may lead to an indexer.
        for head in heads.lock().unwrap().iter() {
            assert!(!head.to_ascii_lowercase().contains("x-api-key"), "{head}");
        }

        let error = prowlarr.fetch(&at("/loop")).await.unwrap_err();
        assert!(
            matches!(error, ClientError::Refused(Refusal::TooManyRedirects)),
            "{error:?}"
        );
        let error = prowlarr.fetch(&at("/refused")).await.unwrap_err();
        assert_eq!(error.code(), "INTEGRATION_AUTH_FAILED");
        let error = prowlarr.fetch(&at("/gone")).await.unwrap_err();
        assert!(error.to_string().contains("search again"), "{error}");
        // A redirect to an address the policy refuses is not followed.
        let error = prowlarr.fetch(&at("/elsewhere")).await.unwrap_err();
        assert_eq!(error.code(), "INTEGRATION_ADDRESS_REFUSED");
    }

    #[tokio::test]
    async fn the_api_is_asked_with_the_key_in_a_header_and_never_in_the_url() {
        let ok = |body: &str| {
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            )
        };
        let (address, heads) = server(vec![
            ("/prowlarr/api/v1/system/status", ok(STATUS)),
            ("/prowlarr/api/v1/search", ok(MOVIES)),
        ])
        .await;
        let prowlarr = prowlarr_at(address);
        assert_eq!(prowlarr.status().await.unwrap().version, "2.7.0.5723");
        let found = prowlarr
            .search(&Search::words("Big Buck Bunny", &[2000, 5000]))
            .await
            .unwrap();
        assert_eq!(found.len(), 3);
        let heads = heads.lock().unwrap();
        let search = heads.iter().find(|head| head.contains("/search")).unwrap();
        let line = search.lines().next().unwrap();
        assert_eq!(
            line,
            "GET /prowlarr/api/v1/search?query=Big+Buck+Bunny&type=search&categories=2000&\
             categories=5000&limit=100 HTTP/1.1"
        );
        assert!(search.to_ascii_lowercase().contains("x-api-key: the-key"));
        assert!(
            heads
                .iter()
                .all(|head| !head.lines().next().unwrap().contains("the-key"))
        );
    }

    #[tokio::test]
    async fn a_prowlarr_that_never_answers_is_given_up_on() {
        tokio::time::pause();
        let error = within(
            ANSWER_TIMEOUT,
            std::future::pending::<Result<(), ClientError>>(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), "INTEGRATION_UNREACHABLE");
        assert!(error.to_string().contains("20 seconds"), "{error}");
    }

    #[tokio::test]
    async fn a_wrong_key_and_a_refused_search_say_so() {
        let (address, _) = server(vec![
            (
                "/prowlarr/api/v1/system/status",
                "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n".to_owned(),
            ),
            (
                "/prowlarr/api/v1/search",
                format!(
                    "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\n\r\n{REFUSED}",
                    REFUSED.len()
                ),
            ),
        ])
        .await;
        let prowlarr = prowlarr_at(address);
        let error = prowlarr.status().await.unwrap_err();
        assert_eq!(error.code(), "INTEGRATION_AUTH_FAILED");
        let error = prowlarr.search(&Search::words("x", &[])).await.unwrap_err();
        assert_eq!(error.code(), "INTEGRATION_BAD_RESPONSE");
        assert!(
            error.to_string().contains("all selected indexers"),
            "{error}"
        );
        assert!(!error.to_string().contains("NzbDrone"), "{error}");
    }

    #[test]
    fn a_magnet_link_is_bounded() {
        assert!(magnet("magnet:?xt=urn:btih:a").is_ok());
        assert!(magnet(&format!("magnet:?{}", "a".repeat(MAX_MAGNET_CHARS))).is_err());
        assert!(magnet("magnet:?xt=a\nb").is_err());
    }
}
