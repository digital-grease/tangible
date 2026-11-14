// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Searching Prowlarr for an approved request's releases, and holding what
//! was found while a person picks one (ADR 0021, docs/plans/requests.md Q3).
//!
//! What a request is searched for is its title, cleaned of what an indexer
//! would not match (a DAT's region in parentheses, punctuation), in the
//! categories its kind suits; a film with an IMDb ID is searched for by it
//! first. A person may search other words instead. Nothing is grabbed here:
//! a person picks, and the grab route fetches it.
//!
//! The results are ranked by how well they match what was asked for, and
//! each says why. They are held in memory for an hour, one search per
//! request, at most [`MAX_HELD_SEARCHES`] of them; each release's links,
//! which carry Prowlarr's API key, are held sealed under the server's key
//! with the result's id as their additional data, and are never answered.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tangible_db::requests::RequestRecord;
use tangible_domain::{RequestKind, SecretString};
use tangible_integrations::outbound::Url;
use tangible_integrations::prowlarr::{MAX_RESULTS, Protocol, Release, Search};
use time::OffsetDateTime;

use crate::secrets::{Sealed, SecretError, SecretKey};

/// How long a search's results are held.
pub const HOLD_FOR: Duration = Duration::from_hours(1);

/// The most searches held at once; past it, the oldest is let go.
pub const MAX_HELD_SEARCHES: usize = 256;

/// The longest words a person may search instead, in characters.
pub const MAX_WORDS_CHARS: usize = 200;

/// The field a release's link is sealed under.
const LINK_FIELD: &str = "release_link";

/// The Newznab categories a request's kind is searched in: consoles and PC
/// for a game, PC for software, audio, films, television; every one for
/// anything else.
#[must_use]
pub const fn categories(kind: RequestKind) -> &'static [u32] {
    match kind {
        RequestKind::Game => &[1000, 4000],
        RequestKind::Software => &[4000],
        RequestKind::Music => &[3000],
        RequestKind::Movie => &[2000],
        RequestKind::Television => &[5000],
        RequestKind::Other => &[],
    }
}

/// A title as words to search for: what is in parentheses or brackets (a
/// DAT's region and flags) dropped, punctuation made spaces. The title
/// itself when nothing is left.
#[must_use]
pub fn words_of(title: &str) -> String {
    let mut depth = 0_u32;
    let mut out = String::with_capacity(title.len());
    for c in title.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            '\'' | '\u{2019}' if depth == 0 => {}
            c if depth == 0 && c.is_alphanumeric() => out.push(c),
            _ if depth == 0 => out.push(' '),
            _ => {}
        }
    }
    let words = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if words.is_empty() {
        title.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        words
    }
}

/// The request's IMDb ID, for a film that has one.
fn imdb_of(record: &RequestRecord) -> Option<String> {
    if record.kind != RequestKind::Movie {
        return None;
    }
    record.refs.as_array()?.iter().find_map(|reference| {
        (reference.get("provider")?.as_str()? == "imdb")
            .then(|| reference.get("external_id")?.as_str().map(str::to_owned))
            .flatten()
            .filter(|id| is_imdb_title(id))
    })
}

/// Whether `id` is an IMDb title's: `tt` and seven to ten digits.
fn is_imdb_title(id: &str) -> bool {
    id.strip_prefix("tt").is_some_and(|digits| {
        (7..=10).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
    })
}

/// What to search for, in order: a film by its IMDb ID, then its words if
/// that found nothing; otherwise the words alone. `words`, when a person
/// gave them, are searched instead of the title, and alone.
#[must_use]
pub fn plan(record: &RequestRecord, words: Option<&str>) -> Vec<Search> {
    let categories = categories(record.kind);
    if let Some(words) = words {
        return vec![Search::words(words, categories)];
    }
    let mut searches = Vec::new();
    if let Some(imdb) = imdb_of(record) {
        searches.push(Search::film(&imdb, categories));
    }
    searches.push(Search::words(&words_of(&record.title), categories));
    searches
}

/// Lowercase words of letters and digits.
fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Words too common to tell releases apart.
const COMMON: [&str; 6] = ["the", "a", "an", "of", "and", "to"];

/// Words a platform's name carries that a release's rarely does: its
/// maker.
const MAKERS: [&str; 10] = [
    "sony",
    "nintendo",
    "sega",
    "microsoft",
    "nec",
    "snk",
    "atari",
    "bandai",
    "panasonic",
    "philips",
];

/// Short names releases use for platforms, by a phrase of the platform's
/// name. The longest phrase that is in it wins.
const PLATFORM_ALIASES: [(&str, &[&str]); 12] = [
    ("playstation portable", &["psp"]),
    ("playstation 3", &["ps3"]),
    ("playstation 2", &["ps2"]),
    ("playstation", &["psx", "ps1"]),
    ("gamecube", &["ngc", "gc"]),
    ("dreamcast", &["dc"]),
    ("saturn", &["ss"]),
    ("xbox 360", &["x360"]),
    ("xbox", &["xbox"]),
    ("wii u", &["wiiu"]),
    ("pc engine", &["pce", "tg16"]),
    ("mega cd", &["segacd", "scd"]),
];

/// Words that say a release is a disc image.
const DISC_WORDS: [&str; 18] = [
    "iso", "bin", "cue", "chd", "img", "mdf", "mds", "ccd", "toc", "nrg", "bdmv", "bluray", "blu",
    "dvd", "dvd5", "dvd9", "bd25", "bd50",
];

/// Whether `have` names the platform `platform`: every word of its name
/// but its maker's, or a short name releases use for it.
fn names_platform(platform: &str, have: &BTreeSet<String>) -> bool {
    let lowered = platform.to_lowercase();
    let words: Vec<String> = tokens(&lowered)
        .into_iter()
        .filter(|word| !MAKERS.contains(&word.as_str()) && !COMMON.contains(&word.as_str()))
        .collect();
    if !words.is_empty() && words.iter().all(|word| have.contains(word)) {
        return true;
    }
    let spaced = tokens(&lowered).join(" ");
    PLATFORM_ALIASES
        .iter()
        .filter(|(phrase, _)| spaced.contains(phrase))
        .max_by_key(|(phrase, _)| phrase.len())
        .is_some_and(|(_, aliases)| aliases.iter().any(|alias| have.contains(*alias)))
}

/// A four-digit year from 1900 to 2099.
fn is_year(word: &str) -> bool {
    word.len() == 4
        && word.bytes().all(|b| b.is_ascii_digit())
        && (word.starts_with("19") || word.starts_with("20"))
}

/// How well `release` matches what was asked for, and why: the share of
/// the words searched for in its title (up to 100), the year and platform
/// when the request names them, whether it names a disc image, and a
/// torrent nobody seeds.
#[must_use]
pub fn rank(record: &RequestRecord, words: &str, release: &Release) -> (i32, Vec<String>) {
    let mut score = 0_i32;
    let mut reasons = Vec::new();
    let have: BTreeSet<String> = tokens(&release.title).into_iter().collect();

    let all = tokens(words);
    let wanted: Vec<&String> = {
        let telling: Vec<&String> = all
            .iter()
            .filter(|word| !COMMON.contains(&word.as_str()))
            .collect();
        if telling.is_empty() {
            all.iter().collect()
        } else {
            telling
        }
    };
    if !wanted.is_empty() {
        let matched = wanted.iter().filter(|word| have.contains(**word)).count();
        let total = wanted.len();
        score += i32::try_from(matched * 100 / total).unwrap_or(0);
        reasons.push(if matched == total {
            "every word searched for is in its title".to_owned()
        } else {
            format!("{matched} of the {total} words searched for are in its title")
        });
    }

    let detail = |name: &str| record.details.get(name).and_then(|value| value.as_str());
    if let Some(year) = detail("year").filter(|year| is_year(year)) {
        if have.contains(year) {
            score += 20;
            reasons.push(format!("it names the year, {year}"));
        } else if let Some(other) = have.iter().find(|word| is_year(word)) {
            score -= 30;
            reasons.push(format!("it names another year, {other}"));
        }
    }
    if let Some(platform) = detail("platform")
        && names_platform(platform, &have)
    {
        score += 20;
        reasons.push(format!("it names the platform, {platform}"));
    }
    let video_ts = have.contains("video") && have.contains("ts");
    if video_ts || DISC_WORDS.iter().any(|word| have.contains(*word)) {
        score += 10;
        reasons.push("it says it is a disc image".to_owned());
    }
    if release.protocol == Protocol::Torrent && release.seeders == Some(0) {
        score -= 50;
        reasons.push("nobody is seeding it".to_owned());
    }
    (score, reasons)
}

/// A release held for a person to pick: what is shown of it, and its links
/// sealed.
#[derive(Clone)]
pub struct HeldRelease {
    /// The result's id, as the grab route takes it.
    pub id: uuid::Uuid,
    /// The release, its links taken out.
    pub release: Release,
    /// How well it matches.
    pub score: i32,
    /// Why.
    pub reasons: Vec<String>,
    download: Option<Sealed>,
    magnet: Option<Sealed>,
}

impl std::fmt::Debug for HeldRelease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeldRelease")
            .field("id", &self.id)
            .field("release", &self.release)
            .field("score", &self.score)
            .finish_non_exhaustive()
    }
}

impl HeldRelease {
    /// The release, held: its links sealed under `key`, and taken out of
    /// what is kept in the clear.
    ///
    /// # Errors
    ///
    /// [`SecretError`] when a link could not be sealed.
    pub fn seal(
        key: &SecretKey,
        mut release: Release,
        score: i32,
        reasons: Vec<String>,
    ) -> Result<Self, SecretError> {
        let id = uuid::Uuid::now_v7();
        let context = id.to_string();
        let seal = |link: Option<Url>| -> Result<Option<Sealed>, SecretError> {
            link.map(|link| key.seal(&context, LINK_FIELD, &SecretString::new(link.as_str())))
                .transpose()
        };
        let download = seal(release.download.take())?;
        let magnet = seal(release.magnet.take())?;
        Ok(Self {
            id,
            release,
            score,
            reasons,
            download,
            magnet,
        })
    }

    /// The link to fetch it by, opened: its file link, or, for a torrent
    /// without one, its magnet link.
    ///
    /// # Errors
    ///
    /// [`SecretError`] for a link that does not open, as under another key.
    pub fn link(&self, key: &SecretKey) -> Result<Option<Url>, SecretError> {
        let Some(sealed) = self.download.as_ref().or(self.magnet.as_ref()) else {
            return Ok(None);
        };
        let opened = key.open(&self.id.to_string(), LINK_FIELD, sealed)?;
        Ok(Url::parse(opened.expose()).ok())
    }
}

/// How Prowlarr's indexers stood when a search was made.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexerState {
    /// How many are turned on in Prowlarr, if Prowlarr said.
    pub enabled: Option<usize>,
    /// Those it has paused after they failed: name, and until when.
    pub paused: Vec<(String, Option<OffsetDateTime>)>,
}

/// One search, held.
#[derive(Debug, Clone)]
pub struct HeldSearch {
    /// The Prowlarr integration searched.
    pub prowlarr_id: uuid::Uuid,
    /// The words searched for, or the IMDb ID.
    pub query: String,
    /// When.
    pub searched_at: OffsetDateTime,
    /// What was found, best first.
    pub releases: Vec<HeldRelease>,
    /// How the indexers stood.
    pub indexers: IndexerState,
    held_at: Instant,
}

impl HeldSearch {
    /// A search made now.
    #[must_use]
    pub fn new(
        prowlarr_id: uuid::Uuid,
        query: String,
        releases: Vec<HeldRelease>,
        indexers: IndexerState,
    ) -> Self {
        Self {
            prowlarr_id,
            query,
            searched_at: OffsetDateTime::now_utc(),
            releases,
            indexers,
            held_at: Instant::now(),
        }
    }

    /// Until when it is held.
    #[must_use]
    pub fn held_until(&self) -> OffsetDateTime {
        self.searched_at + HOLD_FOR
    }

    fn expired(&self, now: Instant) -> bool {
        now.duration_since(self.held_at) >= HOLD_FOR
    }
}

/// The searches held, by request. Shared by every clone of the state.
#[derive(Debug, Clone, Default)]
pub struct Held {
    searches: Arc<Mutex<HashMap<uuid::Uuid, HeldSearch>>>,
}

impl Held {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<uuid::Uuid, HeldSearch>> {
        // A panic while holding the lock leaves nothing half-written that
        // matters: at worst a search is lost, and searched again.
        self.searches.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Hold `search` for `request`, in place of any search held for it.
    pub fn hold(&self, request: uuid::Uuid, search: HeldSearch) {
        let now = Instant::now();
        let mut searches = self.lock();
        searches.retain(|_, held| !held.expired(now));
        if !searches.contains_key(&request) && searches.len() >= MAX_HELD_SEARCHES {
            let oldest = searches
                .iter()
                .min_by_key(|(_, held)| held.held_at)
                .map(|(id, _)| *id);
            if let Some(oldest) = oldest {
                searches.remove(&oldest);
            }
        }
        searches.insert(request, search);
    }

    /// The search held for `request`, while it is held.
    #[must_use]
    pub fn get(&self, request: uuid::Uuid) -> Option<HeldSearch> {
        let now = Instant::now();
        let mut searches = self.lock();
        if searches.get(&request).is_some_and(|held| held.expired(now)) {
            searches.remove(&request);
        }
        searches.get(&request).cloned()
    }
}

/// The releases found, best first, at most [`MAX_RESULTS`]: one per
/// indexer and identifier, ranked, then by seeders, grabs and how recent.
#[must_use]
pub fn ranked(
    record: &RequestRecord,
    words: &str,
    found: Vec<Release>,
) -> Vec<(Release, i32, Vec<String>)> {
    let mut seen = BTreeSet::new();
    let mut ranked: Vec<(Release, i32, Vec<String>)> = found
        .into_iter()
        .filter(|release| seen.insert((release.indexer_id, release.guid.clone())))
        .map(|release| {
            let (score, reasons) = rank(record, words, &release);
            (release, score, reasons)
        })
        .collect();
    ranked.sort_by(|(a, a_score, _), (b, b_score, _)| {
        b_score
            .cmp(a_score)
            .then(b.seeders.unwrap_or(0).cmp(&a.seeders.unwrap_or(0)))
            .then(b.grabs.unwrap_or(0).cmp(&a.grabs.unwrap_or(0)))
            .then(b.published_at.cmp(&a.published_at))
    });
    ranked.truncate(MAX_RESULTS);
    ranked
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use tangible_domain::{RequestId, RequestState};
    use tangible_integrations::prowlarr::parse_releases;

    const MOVIES: &str =
        include_str!("../../../fixtures/tool-output/prowlarr/search-movies-2.7.0.json");

    fn request(kind: RequestKind, title: &str, details: serde_json::Value) -> RequestRecord {
        let now = OffsetDateTime::now_utc();
        RequestRecord {
            id: RequestId::generate(),
            requester_id: uuid::Uuid::now_v7(),
            requested_by: "someone".to_owned(),
            kind,
            title: title.to_owned(),
            details,
            refs: serde_json::json!([]),
            note: None,
            state: RequestState::Approved,
            decided_by: Some("operator".to_owned()),
            decided_at: Some(now),
            decline_reason: None,
            integration_id: None,
            import_id: None,
            artifact_id: None,
            error_code: None,
            error_detail: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn release(title: &str, seeders: Option<u32>) -> Release {
        let mut release = parse_releases(MOVIES).unwrap().remove(0);
        release.title = title.to_owned();
        release.guid = title.to_owned();
        release.seeders = seeders;
        release
    }

    #[test]
    fn a_title_is_searched_for_without_what_an_indexer_would_not_match() {
        assert_eq!(words_of("Crash Bandicoot (USA)"), "Crash Bandicoot");
        assert_eq!(
            words_of("The Legend of Zelda: Ocarina of Time [!] (Europe) (En,Fr)"),
            "The Legend of Zelda Ocarina of Time"
        );
        assert_eq!(
            words_of("Tony Hawk's Pro Skater 2"),
            "Tony Hawks Pro Skater 2"
        );
        assert_eq!(words_of("(Unl)"), "(Unl)");
        assert_eq!(words_of("Amélie"), "Amélie");
    }

    #[test]
    fn a_film_with_an_imdb_id_is_searched_for_by_it_first() {
        let mut film = request(RequestKind::Movie, "Big Buck Bunny", serde_json::json!({}));
        film.refs = serde_json::json!([
            { "provider": "tmdb", "external_id": "movie/10378" },
            { "provider": "imdb", "external_id": "tt1254207" }
        ]);
        let searches = plan(&film, None);
        assert_eq!(searches.len(), 2);
        assert_eq!(searches[0], Search::film("tt1254207", &[2000]));
        assert_eq!(searches[1], Search::words("Big Buck Bunny", &[2000]));
        // Words a person gave are searched alone.
        assert_eq!(
            plan(&film, Some("bbb 2008")),
            vec![Search::words("bbb 2008", &[2000])]
        );
        // A reference that is not an IMDb title's is not sent.
        let mut odd = film.clone();
        odd.refs = serde_json::json!([{ "provider": "imdb", "external_id": "tt12}{x" }]);
        assert_eq!(
            plan(&odd, None),
            vec![Search::words("Big Buck Bunny", &[2000])]
        );
        // Only a film is searched by IMDb ID.
        let mut series = film.clone();
        series.kind = RequestKind::Television;
        assert_eq!(
            plan(&series, None),
            vec![Search::words("Big Buck Bunny", &[5000])]
        );
        let game = request(
            RequestKind::Game,
            "Crash Bandicoot (USA)",
            serde_json::json!({}),
        );
        assert_eq!(
            plan(&game, None),
            vec![Search::words("Crash Bandicoot", &[1000, 4000])]
        );
        assert!(categories(RequestKind::Other).is_empty());
    }

    #[test]
    fn releases_rank_by_how_well_they_match_and_say_why() {
        let game = request(
            RequestKind::Game,
            "Crash Bandicoot (USA)",
            serde_json::json!({ "platform": "Sony - PlayStation", "year": "1996" }),
        );
        let words = "Crash Bandicoot";
        let (best, why) = rank(
            &game,
            words,
            &release("Crash Bandicoot (1996) (USA) [PSX] [BIN/CUE]", Some(5)),
        );
        assert_eq!(best, 100 + 20 + 20 + 10, "{why:?}");
        assert_eq!(
            why,
            [
                "every word searched for is in its title",
                "it names the year, 1996",
                "it names the platform, Sony - PlayStation",
                "it says it is a disc image",
            ]
        );
        let (half, why) = rank(&game, words, &release("Crash Team Racing 1999", Some(3)));
        assert_eq!(half, 50 - 30, "{why:?}");
        assert!(why.contains(&"1 of the 2 words searched for are in its title".to_owned()));
        assert!(why.contains(&"it names another year, 1999".to_owned()));
        let (dead, why) = rank(&game, words, &release("Crash Bandicoot", Some(0)));
        assert_eq!(dead, 100 - 50, "{why:?}");
        assert!(why.contains(&"nobody is seeding it".to_owned()));

        // The platform by every word of its name but its maker's.
        let saturn = request(
            RequestKind::Game,
            "Nights",
            serde_json::json!({ "platform": "Sega Saturn" }),
        );
        assert!(
            rank(
                &saturn,
                "Nights",
                &release("NiGHTS into Dreams Saturn", None)
            )
            .1
            .iter()
            .any(|r| r.contains("platform"))
        );
        // A PlayStation 2 game is not taken for a PlayStation one.
        let two = request(
            RequestKind::Game,
            "Okami",
            serde_json::json!({ "platform": "Sony - PlayStation 2" }),
        );
        assert!(
            rank(&two, "Okami", &release("Okami [PS2]", None))
                .1
                .iter()
                .any(|r| r.contains("platform"))
        );
        assert!(
            !rank(&two, "Okami", &release("Okami [PSX]", None))
                .1
                .iter()
                .any(|r| r.contains("platform"))
        );
    }

    #[test]
    fn the_best_come_first_and_each_release_once() {
        let film = request(
            RequestKind::Movie,
            "Big Buck Bunny",
            serde_json::json!({ "year": "2008" }),
        );
        let mut found = parse_releases(MOVIES).unwrap();
        found.push(found[0].clone());
        let ranked = ranked(&film, "Big Buck Bunny", found);
        assert_eq!(ranked.len(), 3);
        // All three match every word, the year and a disc; the most seeded
        // first, the Usenet one (no seeders) last.
        assert_eq!(ranked[0].0.title, "Big Buck Bunny (2008) [Blu-ray ISO]");
        assert_eq!(ranked[1].0.title, "Big Buck Bunny (2008) [DVD ISO]");
        assert_eq!(ranked[2].0.protocol, Protocol::Usenet);
    }

    #[test]
    fn a_held_release_keeps_its_links_sealed_and_opens_them_with_the_key() {
        let key = SecretKey::from_bytes(&[7; 32]).unwrap();
        let found = parse_releases(MOVIES).unwrap().remove(0);
        let link = found.download.clone().unwrap();
        let held = HeldRelease::seal(&key, found, 1, Vec::new()).unwrap();
        assert!(held.release.download.is_none() && held.release.magnet.is_none());
        assert!(!format!("{held:?}").contains("apikey"));
        assert_eq!(held.link(&key).unwrap(), Some(link));
        let other = SecretKey::from_bytes(&[8; 32]).unwrap();
        assert!(held.link(&other).is_err());
    }

    #[test]
    fn a_search_is_held_for_an_hour_and_the_oldest_let_go_past_the_most() {
        let held = Held::default();
        let request = uuid::Uuid::now_v7();
        held.hold(
            request,
            HeldSearch::new(
                uuid::Uuid::now_v7(),
                "x".to_owned(),
                Vec::new(),
                IndexerState::default(),
            ),
        );
        assert!(held.get(request).is_some());
        let mut old = HeldSearch::new(
            uuid::Uuid::now_v7(),
            "y".to_owned(),
            Vec::new(),
            IndexerState::default(),
        );
        old.held_at = Instant::now().checked_sub(HOLD_FOR).unwrap();
        held.hold(request, old);
        assert!(held.get(request).is_none(), "an hour on, it is let go");

        let first = uuid::Uuid::now_v7();
        held.hold(
            first,
            HeldSearch::new(
                uuid::Uuid::now_v7(),
                "a".to_owned(),
                Vec::new(),
                IndexerState::default(),
            ),
        );
        for _ in 0..MAX_HELD_SEARCHES {
            held.hold(
                uuid::Uuid::now_v7(),
                HeldSearch::new(
                    uuid::Uuid::now_v7(),
                    "b".to_owned(),
                    Vec::new(),
                    IndexerState::default(),
                ),
            );
        }
        assert!(held.get(first).is_none(), "the oldest went");
        assert_eq!(held.lock().len(), MAX_HELD_SEARCHES);
    }
}
