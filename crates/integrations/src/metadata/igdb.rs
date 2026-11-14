// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! IGDB's game search (ADR 0021), signed with a Twitch app access token.
//!
//! The token comes from Twitch's client-credentials grant, with the
//! client ID and secret an administrator entered, sent in the form body so
//! neither is ever in a URL. It is kept, in memory, until shortly before it
//! expires, and fetched again once if IGDB refuses it. IGDB allows four
//! requests a second. The answers' shapes are from IGDB's v4 reference and
//! Twitch's (fixtures/tool-output/metadata, written rather than recorded).

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tangible_domain::{RequestKind, SecretString};
use tokio::sync::Mutex;
use tokio::time::Instant;

use super::{Candidate, MAX_CANDIDATES, Pace, query_of};
use crate::download::{ClientError, read_text_within};
use crate::outbound::{OutboundClient, OutboundError, Url};

/// IGDB's API.
pub const API_BASE: &str = "https://api.igdb.com/v4/";

/// Where Twitch issues app access tokens.
pub const TOKEN_URL: &str = "https://id.twitch.tv/oauth2/token";

/// The provider's name, as references and answers carry it.
pub const PROVIDER: &str = "igdb";

const CLIENT: &str = "IGDB";

/// Largest answer read.
const MAX_ANSWER_BYTES: usize = 1024 * 1024;

/// How long before it expires a token is no longer used.
const EXPIRY_MARGIN: Duration = Duration::from_mins(10);

/// Most platforms named in a candidate's details.
const MAX_PLATFORMS: usize = 6;

fn outbound(error: OutboundError) -> ClientError {
    ClientError::from_outbound(CLIENT, error)
}

/// A token, and the credentials it was issued for.
struct Issued {
    client_id: SecretString,
    client_secret: SecretString,
    token: SecretString,
    until: Instant,
}

/// The app access token, kept between searches. Shared by every client
/// made from the same integration; a token issued for other credentials is
/// not used.
#[derive(Clone, Default)]
pub struct TokenCache(Arc<Mutex<Option<Issued>>>);

impl std::fmt::Debug for TokenCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokenCache(..)")
    }
}

/// Where the client sends things: IGDB's API and Twitch's token endpoint.
#[derive(Debug, Clone)]
pub struct Endpoints {
    /// IGDB's API ([`API_BASE`] but in tests).
    pub api: Url,
    /// Twitch's token endpoint ([`TOKEN_URL`] but in tests).
    pub token: Url,
}

/// A client for IGDB.
#[derive(Debug, Clone)]
pub struct IgdbClient {
    http: OutboundClient,
    endpoints: Endpoints,
    client_id: SecretString,
    client_secret: SecretString,
    pace: Pace,
    tokens: TokenCache,
}

/// `text` as an Apicalypse string literal's contents.
fn quoted(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

impl IgdbClient {
    /// A client signing in with `client_id` and `client_secret`.
    #[must_use]
    pub fn new(
        http: OutboundClient,
        endpoints: Endpoints,
        client_id: SecretString,
        client_secret: SecretString,
        pace: Pace,
        tokens: TokenCache,
    ) -> Self {
        Self {
            http,
            endpoints,
            client_id,
            client_secret,
            pace,
            tokens,
        }
    }

    /// A token for these credentials: the one kept, or a new one.
    async fn token(&self, fresh: bool) -> Result<SecretString, ClientError> {
        let mut kept = self.tokens.0.lock().await;
        if !fresh
            && let Some(issued) = kept.as_ref()
            && issued.client_id.expose() == self.client_id.expose()
            && issued.client_secret.expose() == self.client_secret.expose()
            && Instant::now() < issued.until
        {
            return Ok(issued.token.clone());
        }
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", self.client_id.expose())
            .append_pair("client_secret", self.client_secret.expose())
            .append_pair("grant_type", "client_credentials")
            .finish();
        let request = self
            .http
            .request(reqwest::Method::POST, &self.endpoints.token)
            .map_err(outbound)?
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .timeout(super::ANSWER_TIMEOUT);
        let response = self.http.send(request).await.map_err(outbound)?;
        let status = response.status().as_u16();
        let text = read_text_within(CLIENT, response, 64 * 1024).await?;
        match status {
            200 => {}
            400 | 401 | 403 => {
                return Err(ClientError::Auth {
                    client: CLIENT,
                    reason: "Twitch refused the client ID or secret",
                });
            }
            code => {
                return Err(ClientError::bad(
                    CLIENT,
                    format!("Twitch's token endpoint answered {code}"),
                ));
            }
        }
        let (token, lifetime) = parse_token(&text)?;
        *kept = Some(Issued {
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.clone(),
            token: token.clone(),
            until: Instant::now() + lifetime.saturating_sub(EXPIRY_MARGIN),
        });
        Ok(token)
    }

    async fn post(
        &self,
        endpoint: &str,
        body: &str,
        token: &SecretString,
    ) -> Result<(u16, String), ClientError> {
        let url = self
            .endpoints
            .api
            .join(endpoint)
            .map_err(|_| ClientError::bad(CLIENT, "the address cannot be joined"))?;
        self.pace.wait().await;
        let request = self
            .http
            .request(reqwest::Method::POST, &url)
            .map_err(outbound)?
            .header("Client-ID", self.client_id.expose())
            // reqwest marks the header sensitive.
            .bearer_auth(token.expose())
            .header(reqwest::header::ACCEPT, "application/json")
            .body(body.to_owned())
            .timeout(super::ANSWER_TIMEOUT);
        let response = self.http.send(request).await.map_err(outbound)?;
        let status = response.status().as_u16();
        Ok((
            status,
            read_text_within(CLIENT, response, MAX_ANSWER_BYTES).await?,
        ))
    }

    /// An Apicalypse query to `endpoint`, signed, the token fetched again
    /// once if IGDB refuses the one kept.
    async fn query(&self, endpoint: &str, body: &str) -> Result<String, ClientError> {
        let token = self.token(false).await?;
        let (mut status, mut text) = self.post(endpoint, body, &token).await?;
        if status == 401 {
            let token = self.token(true).await?;
            (status, text) = self.post(endpoint, body, &token).await?;
        }
        match status {
            200 => Ok(text),
            401 | 403 => Err(ClientError::Auth {
                client: CLIENT,
                reason: "IGDB refused the token Twitch issued for this client ID",
            }),
            429 => Err(ClientError::bad(
                CLIENT,
                "it asked for requests to slow down (429)",
            )),
            code => Err(ClientError::bad(
                CLIENT,
                format!("{endpoint} answered {code}"),
            )),
        }
    }

    /// Games matching `query`.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn search(&self, query: &str) -> Result<Vec<Candidate>, ClientError> {
        let Some(query) = query_of(query) else {
            return Ok(Vec::new());
        };
        let body = format!(
            "search \"{}\"; fields name,first_release_date,platforms.name,slug; limit {MAX_CANDIDATES};",
            quoted(&query)
        );
        parse_games(&self.query("games", &body).await?)
    }

    /// Whether Twitch issues a token and IGDB takes it, for an
    /// administrator trying the integration.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn check(&self) -> Result<(), ClientError> {
        let text = self.query("games", "fields id; limit 1;").await?;
        serde_json::from_str::<Vec<serde_json::Value>>(&text)
            .map(|_| ())
            .map_err(|_| ClientError::bad(CLIENT, "the answer was not IGDB's"))
    }
}

#[derive(Deserialize)]
struct Token {
    access_token: String,
    expires_in: u64,
}

/// The token in Twitch's answer, and how long it lasts.
///
/// # Errors
///
/// [`ClientError::BadResponse`].
pub fn parse_token(text: &str) -> Result<(SecretString, Duration), ClientError> {
    let token: Token = serde_json::from_str(text)
        .map_err(|_| ClientError::bad(CLIENT, "Twitch's answer held no token"))?;
    if token.access_token.is_empty() || token.access_token.len() > 512 {
        return Err(ClientError::bad(CLIENT, "Twitch's token will not do"));
    }
    Ok((
        SecretString::new(token.access_token),
        Duration::from_secs(token.expires_in),
    ))
}

#[derive(Deserialize)]
struct Game {
    id: u64,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    first_release_date: Option<i64>,
    #[serde(default)]
    platforms: Vec<Platform>,
    #[serde(default)]
    slug: Option<String>,
}

#[derive(Deserialize)]
struct Platform {
    #[serde(default)]
    name: Option<String>,
}

/// The year a Unix time falls in, in UTC (Hinnant's days-to-civil), for
/// years a game may have come out in.
fn year_at(seconds: i64) -> Option<i64> {
    let z = seconds.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let year = yoe + era * 400 + i64::from(mp >= 10);
    (1950..=2200).contains(&year).then_some(year)
}

/// An IGDB slug: lowercase letters, digits and hyphens.
fn slug(text: &str) -> Option<&str> {
    (!text.is_empty()
        && text.len() <= 200
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
    .then_some(text)
}

/// The games in a `games` answer.
///
/// # Errors
///
/// [`ClientError::BadResponse`].
pub fn parse_games(text: &str) -> Result<Vec<Candidate>, ClientError> {
    let games: Vec<Game> = serde_json::from_str(text)
        .map_err(|_| ClientError::bad(CLIENT, "the answer was not a game search's"))?;
    let mut candidates = Vec::new();
    for game in games.iter().take(MAX_CANDIDATES) {
        let Some(mut candidate) = Candidate::new(
            PROVIDER,
            RequestKind::Game,
            game.name.as_deref().unwrap_or(""),
        ) else {
            continue;
        };
        let platforms: Vec<&str> = game
            .platforms
            .iter()
            .filter_map(|platform| platform.name.as_deref())
            .take(MAX_PLATFORMS)
            .collect();
        candidate.detail("platform", &platforms.join(", "));
        if let Some(year) = game.first_release_date.and_then(year_at) {
            candidate.detail("year", &year.to_string());
        }
        candidate.reference(PROVIDER, &game.id.to_string());
        // Built from the slug, not taken from the answer's url.
        candidate.link = game
            .slug
            .as_deref()
            .and_then(slug)
            .map(|slug| format!("https://www.igdb.com/games/{slug}"));
        candidates.push(candidate);
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
    fn a_game_search_answers_games_with_their_platforms() {
        let games = parse_games(&fixture("igdb-games.json")).unwrap();
        assert_eq!(games.len(), 3);
        assert_eq!(games[0].title, "The Legend of Zelda: A Link to the Past");
        assert_eq!(games[0].kind, RequestKind::Game);
        assert_eq!(
            games[0].details["platform"],
            "Super Nintendo Entertainment System, Game Boy Advance"
        );
        assert_eq!(games[0].details["year"], "1991");
        assert_eq!(games[0].refs[0].external_id, "1026");
        assert_eq!(
            games[0].link.as_deref(),
            Some("https://www.igdb.com/games/the-legend-of-zelda-a-link-to-the-past")
        );
        // No date is no year; a url elsewhere is not followed: the link is
        // built from the slug.
        assert!(!games[2].details.contains_key("year"));
        assert_eq!(
            games[2].link.as_deref(),
            Some("https://www.igdb.com/games/the-legend-of-zelda-collectors-edition")
        );
    }

    #[test]
    fn a_token_is_read_and_bounded() {
        let (token, lifetime) = parse_token(&fixture("twitch-token.json")).unwrap();
        assert_eq!(token.expose(), "jostpf5q0uzmxmkba9iyug38kjtgh");
        assert_eq!(lifetime, Duration::from_secs(5_011_271));
        assert!(parse_token(&fixture("twitch-token-refused.json")).is_err());
        assert!(parse_token(r#"{"access_token": "", "expires_in": 5}"#).is_err());
    }

    #[test]
    fn a_release_date_is_its_year_in_utc() {
        assert_eq!(year_at(0), Some(1970));
        assert_eq!(year_at(690_249_600), Some(1991));
        // 2000-01-01T00:00:00Z, and the second before it.
        assert_eq!(year_at(946_684_800), Some(2000));
        assert_eq!(year_at(946_684_799), Some(1999));
        // 2024-02-29, a leap day; 2024-12-31T23:59:59Z.
        assert_eq!(year_at(1_709_164_800), Some(2024));
        assert_eq!(year_at(1_735_689_599), Some(2024));
        assert_eq!(year_at(-1_000_000_000_000), None);
    }

    #[test]
    fn a_query_cannot_break_out_of_its_string() {
        assert_eq!(quoted(r#"a "b" \c"#), r#"a \"b\" \\c"#);
    }

    #[test]
    fn a_slug_is_only_letters_digits_and_hyphens() {
        assert_eq!(slug("zelda-ii"), Some("zelda-ii"));
        assert_eq!(slug("../evil"), None);
        assert_eq!(slug("Zelda"), None);
        assert_eq!(slug(""), None);
    }

    #[test]
    fn an_answer_that_is_not_a_search_is_refused() {
        assert!(parse_games(&fixture("igdb-unauthorized.json")).is_err());
    }
}
