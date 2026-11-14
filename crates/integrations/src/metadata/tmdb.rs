// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! TMDB's search for films and television, and its lookup of an IMDb ID
//! (ADR 0021): IMDb is used only through its IDs, which TMDB resolves.
//!
//! Signed with an API Read Access Token, sent as a bearer token (marked
//! sensitive) rather than an `api_key` in the query, so it is never in a
//! URL. The answers' shapes are from TMDB's v3 reference
//! (fixtures/tool-output/metadata, written rather than recorded).

use serde::Deserialize;
use tangible_domain::{RequestKind, SecretString};

use super::{Candidate, MAX_CANDIDATES, Pace, query_of, year_of};
use crate::download::{ClientError, read_text_within};
use crate::outbound::{OutboundClient, OutboundError, Url};

/// TMDB's API.
pub const BASE: &str = "https://api.themoviedb.org/3/";

/// The provider's name, as references and answers carry it.
pub const PROVIDER: &str = "tmdb";

const CLIENT: &str = "TMDB";

/// Largest answer read.
const MAX_ANSWER_BYTES: usize = 1024 * 1024;

fn outbound(error: OutboundError) -> ClientError {
    ClientError::from_outbound(CLIENT, error)
}

/// A client for TMDB.
#[derive(Debug, Clone)]
pub struct TmdbClient {
    http: OutboundClient,
    base: Url,
    token: SecretString,
    pace: Pace,
}

/// An IMDb title ID, such as `tt0083658`: from an ID or an IMDb address.
#[must_use]
pub fn imdb_id(text: &str) -> Option<String> {
    let text = text.trim();
    let id = |candidate: &str| {
        let digits = candidate.strip_prefix("tt")?;
        ((7..=10).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit()))
            .then(|| candidate.to_owned())
    };
    if let Some(found) = id(text) {
        return Some(found);
    }
    let url = Url::parse(text).ok()?;
    if !matches!(url.scheme(), "https" | "http") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "imdb.com" && !host.ends_with(".imdb.com") {
        return None;
    }
    let mut segments = url.path_segments()?;
    (segments.next() == Some("title"))
        .then(|| segments.next())
        .flatten()
        .and_then(id)
}

impl TmdbClient {
    /// A client for the TMDB API at `base` ([`BASE`] but in tests), signed
    /// with the read access token `token`.
    #[must_use]
    pub fn new(http: OutboundClient, base: Url, token: SecretString, pace: Pace) -> Self {
        Self {
            http,
            base,
            token,
            pace,
        }
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<String, ClientError> {
        let mut url = self
            .base
            .join(path)
            .map_err(|_| ClientError::bad(CLIENT, "the address cannot be joined"))?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        self.pace.wait().await;
        let request = self
            .http
            .get(&url)
            .map_err(outbound)?
            // reqwest marks the header sensitive.
            .bearer_auth(self.token.expose())
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(super::ANSWER_TIMEOUT);
        let response = self.http.send(request).await.map_err(outbound)?;
        let status = response.status().as_u16();
        let text = read_text_within(CLIENT, response, MAX_ANSWER_BYTES).await?;
        match status {
            200 => Ok(text),
            401 => Err(ClientError::Auth {
                client: CLIENT,
                reason: "the read access token is wrong or revoked (it is the long API Read \
                         Access Token, not the short API key)",
            }),
            429 => Err(ClientError::bad(
                CLIENT,
                "it asked for requests to slow down (429)",
            )),
            code => Err(ClientError::bad(CLIENT, format!("{path} answered {code}"))),
        }
    }

    /// Films matching `query`.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn search_movies(&self, query: &str) -> Result<Vec<Candidate>, ClientError> {
        let Some(query) = query_of(query) else {
            return Ok(Vec::new());
        };
        let text = self
            .get(
                "search/movie",
                &[("query", &query), ("include_adult", "false")],
            )
            .await?;
        parse_movies(&text)
    }

    /// Television matching `query`.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn search_tv(&self, query: &str) -> Result<Vec<Candidate>, ClientError> {
        let Some(query) = query_of(query) else {
            return Ok(Vec::new());
        };
        let text = self
            .get(
                "search/tv",
                &[("query", &query), ("include_adult", "false")],
            )
            .await?;
        parse_tv(&text)
    }

    /// What an IMDb title ID names: films and television, each carrying
    /// the IMDb ID as a reference too.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn find_imdb(&self, imdb: &str) -> Result<Vec<Candidate>, ClientError> {
        let Some(imdb) = imdb_id(imdb) else {
            return Ok(Vec::new());
        };
        let text = self
            .get(&format!("find/{imdb}"), &[("external_source", "imdb_id")])
            .await?;
        parse_find(&text, &imdb)
    }

    /// Whether the token opens TMDB, for an administrator trying the
    /// integration.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn check(&self) -> Result<(), ClientError> {
        let text = self.get("configuration", &[]).await?;
        serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .filter(|value| value.get("images").is_some())
            .map(|_| ())
            .ok_or_else(|| ClientError::bad(CLIENT, "the configuration was not TMDB's"))
    }
}

#[derive(Deserialize)]
struct Page<T> {
    #[serde(default = "Vec::new")]
    results: Vec<T>,
}

#[derive(Deserialize)]
struct Movie {
    id: u64,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    original_title: Option<String>,
    #[serde(default)]
    release_date: Option<String>,
}

#[derive(Deserialize)]
struct Show {
    id: u64,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    original_name: Option<String>,
    #[serde(default)]
    first_air_date: Option<String>,
    #[serde(default)]
    origin_country: Vec<String>,
}

#[derive(Deserialize)]
struct Found {
    #[serde(default)]
    movie_results: Vec<Movie>,
    #[serde(default)]
    tv_results: Vec<Show>,
}

fn movie(found: &Movie) -> Option<Candidate> {
    let mut candidate = Candidate::new(
        PROVIDER,
        RequestKind::Movie,
        found.title.as_deref().unwrap_or(""),
    )?;
    if let Some(year) = found.release_date.as_deref().and_then(year_of) {
        candidate.detail("year", year);
    }
    if let Some(original) = found.original_title.as_deref()
        && found.title.as_deref() != Some(original)
    {
        candidate.detail("original_title", original);
    }
    candidate.reference(PROVIDER, &format!("movie/{}", found.id));
    candidate.link = Some(format!("https://www.themoviedb.org/movie/{}", found.id));
    Some(candidate)
}

fn show(found: &Show) -> Option<Candidate> {
    let mut candidate = Candidate::new(
        PROVIDER,
        RequestKind::Television,
        found.name.as_deref().unwrap_or(""),
    )?;
    if let Some(year) = found.first_air_date.as_deref().and_then(year_of) {
        candidate.detail("year", year);
    }
    if let Some(original) = found.original_name.as_deref()
        && found.name.as_deref() != Some(original)
    {
        candidate.detail("original_title", original);
    }
    candidate.detail("region", &found.origin_country.join(", "));
    candidate.reference(PROVIDER, &format!("tv/{}", found.id));
    candidate.link = Some(format!("https://www.themoviedb.org/tv/{}", found.id));
    Some(candidate)
}

fn unreadable() -> ClientError {
    ClientError::bad(CLIENT, "the answer was not a search's")
}

/// The films in a `search/movie` answer.
///
/// # Errors
///
/// [`ClientError::BadResponse`].
pub fn parse_movies(text: &str) -> Result<Vec<Candidate>, ClientError> {
    let page: Page<Movie> = serde_json::from_str(text).map_err(|_| unreadable())?;
    Ok(page
        .results
        .iter()
        .filter_map(movie)
        .take(MAX_CANDIDATES)
        .collect())
}

/// The television in a `search/tv` answer.
///
/// # Errors
///
/// [`ClientError::BadResponse`].
pub fn parse_tv(text: &str) -> Result<Vec<Candidate>, ClientError> {
    let page: Page<Show> = serde_json::from_str(text).map_err(|_| unreadable())?;
    Ok(page
        .results
        .iter()
        .filter_map(show)
        .take(MAX_CANDIDATES)
        .collect())
}

/// What a `find` answer names for the IMDb ID `imdb`, each with it as a
/// reference.
///
/// # Errors
///
/// [`ClientError::BadResponse`].
pub fn parse_find(text: &str, imdb: &str) -> Result<Vec<Candidate>, ClientError> {
    let found: Found = serde_json::from_str(text).map_err(|_| unreadable())?;
    let mut candidates: Vec<Candidate> = found
        .movie_results
        .iter()
        .filter_map(movie)
        .chain(found.tv_results.iter().filter_map(show))
        .take(MAX_CANDIDATES)
        .collect();
    for candidate in &mut candidates {
        candidate.reference("imdb", imdb);
    }
    Ok(candidates)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/../../fixtures/tool-output/metadata/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn a_film_search_answers_films() {
        let films = parse_movies(&fixture("tmdb-search-movie.json")).unwrap();
        assert_eq!(films.len(), 3);
        let first = &films[0];
        assert_eq!(first.title, "Blade Runner");
        assert_eq!(first.kind, RequestKind::Movie);
        assert_eq!(first.details["year"], "1982");
        assert!(
            !first.details.contains_key("original_title"),
            "the same as the title"
        );
        assert_eq!(first.refs[0].external_id, "movie/78");
        assert_eq!(
            first.link.as_deref(),
            Some("https://www.themoviedb.org/movie/78")
        );
        // Markup is kept as text, and an empty date is no year.
        assert_eq!(films[2].title, "Dangerous Days: Making <b>Blade Runner</b>");
        assert!(!films[2].details.contains_key("year"));
    }

    #[test]
    fn a_television_search_answers_programmes() {
        let shows = parse_tv(&fixture("tmdb-search-tv.json")).unwrap();
        assert_eq!(shows[0].title, "Game of Thrones");
        assert_eq!(shows[0].kind, RequestKind::Television);
        assert_eq!(shows[0].details["year"], "2011");
        assert_eq!(shows[0].details["region"], "US");
        assert_eq!(shows[0].refs[0].external_id, "tv/1399");
        assert_eq!(
            shows[1].details["original_title"],
            "ゲーム・オブ・スローンズ"
        );
        assert_eq!(shows[1].details["region"], "JP, GB");
        assert!(
            !shows[1].details.contains_key("year"),
            "a null date is no year"
        );
    }

    #[test]
    fn an_imdb_id_is_found_and_kept_as_a_reference() {
        let found = parse_find(&fixture("tmdb-find-imdb.json"), "tt0083658").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "Blade Runner");
        let refs: Vec<_> = found[0]
            .refs
            .iter()
            .map(|r| (r.provider, r.external_id.as_str()))
            .collect();
        assert_eq!(refs, [("tmdb", "movie/78"), ("imdb", "tt0083658")]);
    }

    #[test]
    fn an_imdb_id_comes_from_an_id_or_an_imdb_address_only() {
        assert_eq!(imdb_id("tt0083658").as_deref(), Some("tt0083658"));
        assert_eq!(
            imdb_id("https://www.imdb.com/title/tt0083658/?ref_=x").as_deref(),
            Some("tt0083658")
        );
        assert_eq!(
            imdb_id("https://m.imdb.com/title/tt12345678").as_deref(),
            Some("tt12345678")
        );
        assert_eq!(imdb_id("tt12"), None);
        assert_eq!(imdb_id("https://example.com/title/tt0083658/"), None);
        assert_eq!(
            imdb_id("https://imdb.com.example.com/title/tt0083658/"),
            None
        );
        assert_eq!(imdb_id("https://www.imdb.com/name/nm0000148/"), None);
        assert_eq!(imdb_id("ftp://imdb.com/title/tt0083658/"), None);
        assert_eq!(imdb_id("Blade Runner"), None);
    }

    #[test]
    fn an_answer_that_is_not_a_search_is_refused() {
        assert!(parse_movies("<html>").is_err());
        assert!(
            parse_movies(&fixture("tmdb-unauthorized.json"))
                .unwrap()
                .is_empty()
        );
    }
}
