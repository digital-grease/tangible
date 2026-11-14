// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Metadata providers (ADR 0021, docs/plans/requests.md Q2): what a person
//! searches to find the item they ask for. MusicBrainz for music, TMDB for
//! films and television (and IMDb IDs, which it resolves), IGDB for games.
//! Imported DATs are searched in the database, not here.
//!
//! Each answers candidates: a title, details (platform, year, region and
//! the like), references to where it was found, and a link to its page.
//! Everything a provider says is untrusted: parsed into these fixed fields,
//! made one line of plain text, bounded to what a request may hold, so a
//! person can ask for a candidate as it is. Nothing is ever rendered as
//! markup. Every provider is reached through the outbound client, and kept
//! to its rate limit by a [`Pace`] shared by every search.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tangible_domain::RequestKind;
use tokio::sync::Mutex;
use tokio::time::Instant;

pub mod igdb;
pub mod musicbrainz;
pub mod tmdb;

/// Most candidates one provider answers with.
pub const MAX_CANDIDATES: usize = 25;

/// Longest title kept, in characters: as long as a request's.
pub const MAX_TITLE_CHARS: usize = 300;

/// Longest detail kept, in characters: as long as a request's.
pub const MAX_DETAIL_CHARS: usize = 200;

/// Longest external identifier kept, in characters: as long as a
/// request's. A candidate whose identifier is longer has no reference.
pub const MAX_EXTERNAL_ID_CHARS: usize = 100;

/// Most details a candidate carries: as many as a request may.
pub const MAX_DETAILS: usize = 8;

/// Most bytes a candidate's details take as compact JSON: as many as a
/// request's may. A detail that would take more is left out.
pub const MAX_DETAILS_BYTES: usize = 3584;

/// Longest query sent, in characters.
pub const MAX_QUERY_CHARS: usize = 200;

/// Longest one provider has to answer a search, waiting its turn included.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(20);

/// Where a candidate was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// `dat`, `musicbrainz`, `tmdb`, `igdb` or `imdb`.
    pub provider: &'static str,
    /// Its identifier there.
    pub external_id: String,
}

/// An item a provider knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Which provider answered it.
    pub provider: &'static str,
    /// What kind of item it is.
    pub kind: RequestKind,
    /// Its title, one line.
    pub title: String,
    /// Platform, year, region and the like, each one line, by the names a
    /// request stores them under.
    pub details: BTreeMap<String, String>,
    /// Where it was found.
    pub refs: Vec<Reference>,
    /// Its page at the provider: always `https`, always the provider's own
    /// site, built here rather than taken from the answer.
    pub link: Option<String>,
}

impl Candidate {
    /// A candidate with `title`, or none for a title with nothing in it.
    #[must_use]
    pub fn new(provider: &'static str, kind: RequestKind, title: &str) -> Option<Self> {
        Some(Self {
            provider,
            kind,
            title: clean(title, MAX_TITLE_CHARS)?,
            details: BTreeMap::new(),
            refs: Vec::new(),
            link: None,
        })
    }

    /// Add a detail, made one line and bounded; nothing for nothing, and
    /// nothing past what a request may hold.
    pub fn detail(&mut self, name: &str, value: &str) {
        let Some(value) = clean(value, MAX_DETAIL_CHARS) else {
            return;
        };
        if self.details.len() >= MAX_DETAILS && !self.details.contains_key(name) {
            return;
        }
        let previous = self.details.insert(name.to_owned(), value);
        let size = serde_json::to_string(&self.details).map_or(usize::MAX, |json| json.len());
        if size > MAX_DETAILS_BYTES {
            match previous {
                Some(previous) => self.details.insert(name.to_owned(), previous),
                None => self.details.remove(name),
            };
        }
    }

    /// Add a reference, if its identifier fits a request's.
    pub fn reference(&mut self, provider: &'static str, external_id: &str) {
        let Some(external_id) = clean(external_id, usize::MAX) else {
            return;
        };
        if external_id.chars().count() <= MAX_EXTERNAL_ID_CHARS {
            self.refs.push(Reference {
                provider,
                external_id,
            });
        }
    }
}

/// `text` as one line of plain text: control characters dropped, runs of
/// whitespace made one space, trimmed, and cut to `max` characters (the cut
/// marked with an ellipsis). `None` when nothing is left.
#[must_use]
pub fn clean(text: &str, max: usize) -> Option<String> {
    let mut out = String::with_capacity(text.len().min(max.saturating_mul(4)));
    let mut space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
        } else if !c.is_control() {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(c);
        }
    }
    if out.is_empty() {
        return None;
    }
    if out.chars().count() > max {
        let kept: String = out.chars().take(max.saturating_sub(1)).collect();
        out = format!("{}\u{2026}", kept.trim_end());
    }
    Some(out)
}

/// The year a date such as `1982-06-25` names, if it names one.
#[must_use]
pub fn year_of(date: &str) -> Option<&str> {
    let year = date.get(..4)?;
    (year.bytes().all(|b| b.is_ascii_digit()) && year != "0000").then_some(year)
}

/// A query as sent: trimmed, one line, bounded. `None` for nothing.
#[must_use]
pub fn query_of(text: &str) -> Option<String> {
    clean(text, MAX_QUERY_CHARS)
}

/// Keeps requests to one provider at least an interval apart, whoever
/// makes them: MusicBrainz asks for one a second, IGDB four.
#[derive(Debug, Clone)]
pub struct Pace {
    interval: Duration,
    next: Arc<Mutex<Option<Instant>>>,
}

impl Pace {
    /// At most one request each `interval`.
    #[must_use]
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            next: Arc::new(Mutex::new(None)),
        }
    }

    /// One request a second, as MusicBrainz asks.
    #[must_use]
    pub fn musicbrainz() -> Self {
        Self::new(Duration::from_secs(1))
    }

    /// Four requests a second, as IGDB allows.
    #[must_use]
    pub fn igdb() -> Self {
        Self::new(Duration::from_millis(250))
    }

    /// TMDB's own limit is about forty a second; twenty keeps well within.
    #[must_use]
    pub fn tmdb() -> Self {
        Self::new(Duration::from_millis(50))
    }

    /// Wait for this request's turn. Turns are taken in the order asked.
    pub async fn wait(&self) {
        let mut next = self.next.lock().await;
        let now = Instant::now();
        let at = next.map_or(now, |at| at.max(now));
        // The lock is held while waiting, so the next caller queues behind.
        tokio::time::sleep_until(at).await;
        *next = Some(at + self.interval);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn provider_text_is_made_one_bounded_line() {
        assert_eq!(
            clean("  Blade\n\tRunner  ", 300).as_deref(),
            Some("Blade Runner")
        );
        assert_eq!(clean("a\u{0}b\u{7}c", 300).as_deref(), Some("abc"));
        assert_eq!(clean(" \n ", 300), None);
        assert_eq!(clean("abcdef", 4).as_deref(), Some("abc\u{2026}"));
        assert_eq!(clean("abcd", 4).as_deref(), Some("abcd"));
        // Markup stays text: it is never rendered.
        assert_eq!(clean("<b>x</b>", 300).as_deref(), Some("<b>x</b>"));
    }

    #[test]
    fn a_candidate_keeps_only_what_a_request_may_hold() {
        let mut candidate = Candidate::new("tmdb", RequestKind::Movie, "A Film").unwrap();
        candidate.detail("year", "");
        candidate.detail("country", " US ");
        candidate.reference("tmdb", "movie/78");
        candidate.reference("imdb", &"x".repeat(MAX_EXTERNAL_ID_CHARS + 1));
        assert_eq!(candidate.details.len(), 1);
        assert_eq!(candidate.details["country"], "US");
        assert_eq!(
            candidate.refs.len(),
            1,
            "too long an identifier is left out"
        );
        assert!(Candidate::new("tmdb", RequestKind::Movie, "  ").is_none());
    }

    #[test]
    fn details_stop_where_a_request_would_refuse_them() {
        let mut candidate = Candidate::new("musicbrainz", RequestKind::Music, "A").unwrap();
        // Two hundred four-byte characters each: four fit, the fifth not.
        let long = "\u{1f4bf}".repeat(MAX_DETAIL_CHARS);
        for n in 0..6 {
            candidate.detail(&format!("d{n}"), &long);
        }
        assert_eq!(candidate.details.len(), 4);
        assert!(serde_json::to_string(&candidate.details).unwrap().len() <= MAX_DETAILS_BYTES);
        // Short ones still fit, up to the count.
        let mut many = Candidate::new("dat", RequestKind::Game, "B").unwrap();
        for n in 0..10 {
            many.detail(&format!("d{n}"), "x");
        }
        assert_eq!(many.details.len(), MAX_DETAILS);
    }

    #[test]
    fn a_year_is_four_digits_or_nothing() {
        assert_eq!(year_of("1982-06-25"), Some("1982"));
        assert_eq!(year_of("1982"), Some("1982"));
        assert_eq!(year_of(""), None);
        assert_eq!(year_of("19"), None);
        assert_eq!(year_of("0000-00-00"), None);
        assert_eq!(year_of("c. 1990"), None);
    }

    #[tokio::test(start_paused = true)]
    async fn requests_keep_their_distance() {
        let pace = Pace::new(Duration::from_secs(1));
        let start = Instant::now();
        pace.wait().await;
        pace.wait().await;
        pace.wait().await;
        assert_eq!(start.elapsed(), Duration::from_secs(2));
        // A gap longer than the interval costs nothing more.
        tokio::time::sleep(Duration::from_secs(5)).await;
        let later = Instant::now();
        pace.wait().await;
        assert_eq!(later.elapsed(), Duration::ZERO);
    }
}
