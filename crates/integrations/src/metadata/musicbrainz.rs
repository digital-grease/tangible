// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! MusicBrainz's release search (ADR 0021): no key, one request a second,
//! and a User-Agent naming the application and where to reach its makers,
//! as MusicBrainz asks of every client. No personal details go in it.
//!
//! Behaviour recorded from musicbrainz.org (fixtures/tool-output/metadata):
//! with `dismax=true` a plain query is searched across titles and artists
//! together and its syntax needs no escaping; releases come back on every
//! medium, cassettes and downloads among them, so those with no disc are
//! left out (a release with no media listed is kept: it may be one).

use serde::Deserialize;

use super::{Candidate, MAX_CANDIDATES, Pace, query_of, year_of};
use crate::download::{ClientError, read_text_within};
use crate::outbound::{OutboundClient, OutboundError, Url};
use tangible_domain::RequestKind;

/// MusicBrainz's own address.
pub const BASE: &str = "https://musicbrainz.org/";

/// The provider's name, as references and answers carry it.
pub const PROVIDER: &str = "musicbrainz";

const CLIENT: &str = "MusicBrainz";

/// Largest answer read: twenty-five releases run to about 50 KB.
const MAX_ANSWER_BYTES: usize = 1024 * 1024;

fn outbound(error: OutboundError) -> ClientError {
    ClientError::from_outbound(CLIENT, error)
}

/// A client for MusicBrainz's search.
#[derive(Debug, Clone)]
pub struct MusicBrainzClient {
    http: OutboundClient,
    base: Url,
    user_agent: String,
    pace: Pace,
}

impl MusicBrainzClient {
    /// A client for the MusicBrainz at `base` ([`BASE`] but in tests),
    /// naming itself `user_agent`, keeping to `pace`.
    #[must_use]
    pub fn new(http: OutboundClient, base: Url, user_agent: &str, pace: Pace) -> Self {
        Self {
            http,
            base,
            user_agent: user_agent.to_owned(),
            pace,
        }
    }

    fn search_url(&self, query: &str, limit: usize) -> Result<Url, ClientError> {
        let mut url = self
            .base
            .join("ws/2/release")
            .map_err(|_| ClientError::bad(CLIENT, "the address cannot be joined"))?;
        url.query_pairs_mut()
            .append_pair("query", query)
            .append_pair("limit", &limit.to_string())
            .append_pair("fmt", "json")
            .append_pair("dismax", "true");
        Ok(url)
    }

    async fn get(&self, url: &Url) -> Result<String, ClientError> {
        self.pace.wait().await;
        let request = self
            .http
            .get(url)
            .map_err(outbound)?
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(super::ANSWER_TIMEOUT);
        let response = self.http.send(request).await.map_err(outbound)?;
        let status = response.status().as_u16();
        let text = read_text_within(CLIENT, response, MAX_ANSWER_BYTES).await?;
        match status {
            200 => Ok(text),
            503 => Err(ClientError::bad(
                CLIENT,
                "it is busy and asked for requests to slow down (503); try again shortly",
            )),
            code => Err(ClientError::bad(
                CLIENT,
                format!("the search answered {code}"),
            )),
        }
    }

    /// Releases matching `query`, those on a disc.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn search(&self, query: &str) -> Result<Vec<Candidate>, ClientError> {
        let Some(query) = query_of(query) else {
            return Ok(Vec::new());
        };
        // More than are shown, since those with no disc are left out.
        let url = self.search_url(&query, MAX_CANDIDATES * 2)?;
        parse_releases(&self.get(&url).await?)
    }

    /// Whether MusicBrainz answers a search, for an administrator trying
    /// the integration.
    ///
    /// # Errors
    ///
    /// [`ClientError`].
    pub async fn check(&self) -> Result<(), ClientError> {
        let url = self.search_url("test", 1)?;
        parse_releases(&self.get(&url).await?).map(|_| ())
    }
}

#[derive(Deserialize)]
struct Answer {
    #[serde(default)]
    releases: Vec<Release>,
}

#[derive(Deserialize)]
struct Release {
    id: String,
    title: String,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    barcode: Option<String>,
    #[serde(default, rename = "artist-credit")]
    artist_credit: Vec<Credit>,
    #[serde(default)]
    media: Vec<Medium>,
    #[serde(default, rename = "label-info")]
    label_info: Vec<LabelInfo>,
}

#[derive(Deserialize)]
struct Credit {
    name: String,
    #[serde(default)]
    joinphrase: Option<String>,
}

#[derive(Deserialize)]
struct Medium {
    #[serde(default)]
    format: Option<String>,
}

#[derive(Deserialize)]
struct LabelInfo {
    #[serde(default, rename = "catalog-number")]
    catalog_number: Option<String>,
}

/// Whether a medium's format is a disc a drive reads: a CD, DVD, Blu-ray
/// or their kin. A format not stated may be one.
fn on_a_disc(format: Option<&str>) -> bool {
    let Some(format) = format else {
        return true;
    };
    let format = format.to_ascii_lowercase();
    ["cd", "dvd", "blu-ray", "sacd", "dualdisc", "umd", "disc"]
        .iter()
        .any(|word| format.contains(word))
}

/// A MusicBrainz identifier: a UUID, or nothing.
fn mbid(text: &str) -> Option<&str> {
    let ok = text.len() == 36
        && text.bytes().enumerate().all(|(at, b)| match at {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        });
    ok.then_some(text)
}

/// The candidates in a release search's answer, those on a disc.
///
/// # Errors
///
/// [`ClientError::BadResponse`] for an answer that is not a search's.
pub fn parse_releases(text: &str) -> Result<Vec<Candidate>, ClientError> {
    let answer: Answer = serde_json::from_str(text)
        .map_err(|_| ClientError::bad(CLIENT, "the answer was not a release search"))?;
    let mut candidates = Vec::new();
    for release in answer.releases {
        if candidates.len() == MAX_CANDIDATES {
            break;
        }
        let formats: Vec<&str> = release
            .media
            .iter()
            .filter_map(|medium| medium.format.as_deref())
            .collect();
        if !release.media.is_empty()
            && !release
                .media
                .iter()
                .any(|medium| on_a_disc(medium.format.as_deref()))
        {
            continue;
        }
        let Some(id) = mbid(&release.id) else {
            continue;
        };
        let Some(mut candidate) = Candidate::new(PROVIDER, RequestKind::Music, &release.title)
        else {
            continue;
        };
        let mut artist = String::new();
        for credit in &release.artist_credit {
            artist.push_str(&credit.name);
            artist.push_str(credit.joinphrase.as_deref().unwrap_or(""));
        }
        candidate.detail("artist", &artist);
        if let Some(year) = release.date.as_deref().and_then(year_of) {
            candidate.detail("year", year);
        }
        if let Some(country) = release.country.as_deref() {
            candidate.detail("region", country);
        }
        let mut shown: Vec<&str> = Vec::new();
        for format in formats {
            if !shown.contains(&format) {
                shown.push(format);
            }
        }
        candidate.detail("format", &shown.join(", "));
        if let Some(catalog) = release
            .label_info
            .iter()
            .find_map(|info| info.catalog_number.as_deref())
        {
            candidate.detail("catalog_number", catalog);
        }
        if let Some(barcode) = release.barcode.as_deref() {
            candidate.detail("barcode", barcode);
        }
        candidate.reference(PROVIDER, id);
        candidate.link = Some(format!("https://musicbrainz.org/release/{id}"));
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
    fn a_recorded_search_answers_the_releases_on_a_disc() {
        let candidates = parse_releases(&fixture("musicbrainz-release-search.json")).unwrap();
        // Eight releases: a download and a cassette left out.
        let formats: Vec<_> = candidates
            .iter()
            .map(|c| c.details.get("format").map(String::as_str))
            .collect();
        assert!(!formats.contains(&Some("Digital Media")));
        assert!(!formats.contains(&Some("Cassette")));
        assert!(formats.contains(&Some("DVD-Audio")), "a DVD is a disc");
        assert!(
            formats.contains(&None),
            "a release with no media stated is kept"
        );

        let first = &candidates[0];
        assert_eq!(
            first.title,
            "A Tribute to Pink Floyd\u{2019}s Dark Side of the Moon"
        );
        assert_eq!(first.kind, RequestKind::Music);
        assert_eq!(first.details["artist"], "E.S.P");
        assert_eq!(first.details["year"], "2003");
        assert_eq!(first.details["region"], "GB");
        assert_eq!(first.details["catalog_number"], "PLATCV 8309");
        assert_eq!(first.details["barcode"], "5014293830928");
        assert_eq!(first.refs[0].provider, "musicbrainz");
        assert_eq!(
            first.link.as_deref(),
            Some(
                format!(
                    "https://musicbrainz.org/release/{}",
                    first.refs[0].external_id
                )
                .as_str()
            )
        );
        assert!(
            candidates
                .iter()
                .any(|c| c.details.get("artist").map(String::as_str) == Some("Pink Floyd"))
        );
    }

    #[test]
    fn an_answer_that_is_not_a_search_is_refused() {
        assert!(parse_releases("<html>").is_err());
        assert!(parse_releases("{}").unwrap().is_empty());
    }

    #[test]
    fn an_identifier_that_is_not_one_is_left_out() {
        let text = r#"{"releases": [
            {"id": "../../evil", "title": "Bad"},
            {"id": "93c671d8-7765-4709-bbf0-fa899f5293a5", "title": "Good", "media": [{"format": "CD"}]}
        ]}"#;
        let candidates = parse_releases(text).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].title, "Good");
    }

    #[test]
    fn formats_on_a_disc() {
        for disc in [
            "CD",
            "Enhanced CD",
            "SACD",
            "Hybrid SACD",
            "DVD-Video",
            "Blu-ray",
            "HDCD",
        ] {
            assert!(on_a_disc(Some(disc)), "{disc}");
        }
        for other in ["Digital Media", "Cassette", "12\" Vinyl", "Reel-to-reel"] {
            assert!(!on_a_disc(Some(other)), "{other}");
        }
    }
}
