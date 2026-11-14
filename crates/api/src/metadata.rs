// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Searching metadata for an item to request (ADR 0021,
//! docs/plans/requests.md Q2).
//!
//! One search asks every source that suits the kind of item, at once:
//! loaded DATs (in the database), and MusicBrainz, TMDB and IGDB where an
//! administrator configured them. A source that fails is named in the
//! answer with why, and the others still answer. An IMDb ID or address is
//! looked up through TMDB alone. Each provider is kept to its rate limit
//! across every search, and its answers are kept for a day by query, so the
//! same search asked again does not reach it.
//!
//! Providers are reached through their own outbound client: `https`, at
//! public addresses only (plus any blocks an administrator allowed), never
//! the private network the download clients are on.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tangible_db::Database;
use tangible_db::integrations::{IntegrationRecord, list_integrations};
use tangible_domain::{RequestKind, SecretString};
use tangible_integrations::download::ClientError;
use tangible_integrations::metadata::igdb::{self, IgdbClient, TokenCache};
use tangible_integrations::metadata::musicbrainz::{self, MusicBrainzClient};
use tangible_integrations::metadata::tmdb::{self, TmdbClient, imdb_id};
use tangible_integrations::metadata::{
    ANSWER_TIMEOUT, Candidate, MAX_CANDIDATES, MAX_TITLE_CHARS, Pace, clean,
};
use tangible_integrations::outbound::{OutboundClient, Url};

use crate::download_clients::{OpenError, secret};
use crate::secrets::SecretKey;

/// The User-Agent providers are sent: MusicBrainz asks every client to
/// name itself and where its makers are found.
pub const USER_AGENT: &str = concat!(
    "Tangible/",
    env!("CARGO_PKG_VERSION"),
    " ( https://github.com/digital-grease/tangible )"
);

/// How long a provider's answer to a query is kept.
pub const CACHE_FOR: Duration = Duration::from_hours(24);

/// Most answers kept.
pub const CACHE_ENTRIES: usize = 512;

/// Where each provider is: their public addresses, but in tests.
#[derive(Debug, Clone)]
pub struct Endpoints {
    /// MusicBrainz.
    pub musicbrainz: Url,
    /// TMDB's API.
    pub tmdb: Url,
    /// IGDB's API and Twitch's token endpoint.
    pub igdb: igdb::Endpoints,
}

impl Endpoints {
    /// The providers' own addresses.
    ///
    /// # Panics
    ///
    /// Never: the addresses are constants that parse.
    #[must_use]
    #[allow(clippy::expect_used)]
    pub fn public() -> Self {
        let url = |text: &str| Url::parse(text).expect("a constant address parses");
        Self {
            musicbrainz: url(musicbrainz::BASE),
            tmdb: url(tmdb::BASE),
            igdb: igdb::Endpoints {
                api: url(igdb::API_BASE),
                token: url(igdb::TOKEN_URL),
            },
        }
    }
}

/// What reaches metadata providers, shared by every search.
#[derive(Clone)]
pub struct Metadata {
    inner: Arc<Inner>,
}

struct Inner {
    http: OutboundClient,
    endpoints: Endpoints,
    musicbrainz_pace: Pace,
    tmdb_pace: Pace,
    igdb_pace: Pace,
    igdb_tokens: TokenCache,
    cache: Mutex<HashMap<CacheKey, (Instant, Vec<Candidate>)>>,
}

impl std::fmt::Debug for Metadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Metadata")
            .field("endpoints", &self.inner.endpoints)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    source: &'static str,
    query: String,
}

/// How a source answered a search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceState {
    /// It answered, with however many candidates.
    Answered,
    /// It failed; the others still answered.
    Failed,
    /// It is not configured.
    Unconfigured,
    /// It is configured and turned off.
    Off,
}

impl SourceState {
    /// The wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::Failed => "failed",
            Self::Unconfigured => "unconfigured",
            Self::Off => "off",
        }
    }
}

/// One source's part in a search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceOutcome {
    /// `dat`, `musicbrainz`, `tmdb` or `igdb`.
    pub provider: &'static str,
    /// How it went.
    pub state: SourceState,
    /// How many candidates it answered.
    pub count: usize,
    /// Why it failed, as a stable code.
    pub error_code: Option<&'static str>,
    /// What to do about it, in words.
    pub detail: Option<String>,
}

impl SourceOutcome {
    fn answered(provider: &'static str, count: usize) -> Self {
        Self {
            provider,
            state: SourceState::Answered,
            count,
            error_code: None,
            detail: None,
        }
    }

    fn not_there(provider: &'static str, state: SourceState, detail: String) -> Self {
        Self {
            provider,
            state,
            count: 0,
            error_code: None,
            detail: Some(detail),
        }
    }

    fn failed(provider: &'static str, code: &'static str, detail: String) -> Self {
        Self {
            provider,
            state: SourceState::Failed,
            count: 0,
            error_code: Some(code),
            detail: Some(detail),
        }
    }
}

/// What a search found.
#[derive(Debug, Clone, Default)]
pub struct Found {
    /// The candidates, source by source: DATs first, as they are this
    /// library's own, then each provider in its own order.
    pub candidates: Vec<Candidate>,
    /// How each source asked went.
    pub sources: Vec<SourceOutcome>,
}

/// A source a search may ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Dat,
    Igdb,
    Musicbrainz,
    Movies,
    Tv,
}

/// Whether `source` suits `kind`: DATs list games and software; IGDB
/// games; MusicBrainz music; TMDB films and television. Any kind not given:
/// all of them.
fn suits(source: Source, kind: Option<RequestKind>) -> bool {
    let Some(kind) = kind else {
        return true;
    };
    match source {
        Source::Dat => matches!(
            kind,
            RequestKind::Game | RequestKind::Software | RequestKind::Other
        ),
        Source::Igdb => kind == RequestKind::Game,
        Source::Musicbrainz => kind == RequestKind::Music,
        Source::Movies => kind == RequestKind::Movie,
        Source::Tv => kind == RequestKind::Television,
    }
}

/// The provider's name in words, for what an administrator is told.
const fn named(provider: &str) -> &'static str {
    match provider.as_bytes() {
        b"musicbrainz" => "MusicBrainz",
        b"tmdb" => "TMDB",
        b"igdb" => "IGDB",
        _ => "This provider",
    }
}

fn open_failed(provider: &'static str, error: &OpenError) -> SourceOutcome {
    SourceOutcome::failed(provider, error.code, error.detail.clone())
}

fn client_failed(provider: &'static str, error: &ClientError) -> SourceOutcome {
    SourceOutcome::failed(provider, error.code(), error.to_string())
}

impl Metadata {
    /// Reach providers with `http` at `endpoints`.
    #[must_use]
    pub fn new(http: OutboundClient, endpoints: Endpoints) -> Self {
        Self {
            inner: Arc::new(Inner {
                http,
                endpoints,
                musicbrainz_pace: Pace::musicbrainz(),
                tmdb_pace: Pace::tmdb(),
                igdb_pace: Pace::igdb(),
                igdb_tokens: TokenCache::default(),
                cache: Mutex::new(HashMap::new()),
            }),
        }
    }

    fn cached(&self, key: &CacheKey) -> Option<Vec<Candidate>> {
        let cache = self.inner.cache.lock().ok()?;
        let (at, candidates) = cache.get(key)?;
        (at.elapsed() < CACHE_FOR).then(|| candidates.clone())
    }

    fn keep(&self, key: CacheKey, candidates: &[Candidate]) {
        let Ok(mut cache) = self.inner.cache.lock() else {
            return;
        };
        if cache.len() >= CACHE_ENTRIES {
            cache.retain(|_, (at, _)| at.elapsed() < CACHE_FOR);
        }
        if cache.len() >= CACHE_ENTRIES
            && let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(key, _)| key.clone())
        {
            cache.remove(&oldest);
        }
        cache.insert(key, (Instant::now(), candidates.to_vec()));
    }

    /// `ask`'s answer for `source` and `query`, from the cache where it is
    /// there, kept there when it answers.
    async fn through_cache<F>(
        &self,
        source: &'static str,
        query: &str,
        ask: F,
    ) -> Result<Vec<Candidate>, ClientError>
    where
        F: Future<Output = Result<Vec<Candidate>, ClientError>>,
    {
        let key = CacheKey {
            source,
            query: query.to_lowercase(),
        };
        if let Some(candidates) = self.cached(&key) {
            return Ok(candidates);
        }
        let candidates = match tokio::time::timeout(ANSWER_TIMEOUT, ask).await {
            Ok(answered) => answered?,
            Err(_) => {
                return Err(ClientError::Unreachable {
                    client: named(source),
                    detail: format!("no answer within {} seconds", ANSWER_TIMEOUT.as_secs()),
                });
            }
        };
        self.keep(key, &candidates);
        Ok(candidates)
    }

    /// Search every source suited to `kind` for `query`.
    ///
    /// # Errors
    ///
    /// None: a source that fails, the database included, is an outcome.
    pub async fn search(
        &self,
        database: &Database,
        key: Option<&SecretKey>,
        query: &str,
        kind: Option<RequestKind>,
    ) -> Found {
        let integrations = match list_integrations(database.pool()).await {
            Ok(integrations) => integrations,
            Err(error) => {
                tracing::error!(error = ?error, "integrations could not be read for a search");
                Vec::new()
            }
        };
        let configured = |kind: &str| integrations.iter().find(|record| record.kind == kind);
        let context = Context {
            database,
            key,
            query,
        };

        // An IMDb ID names one title: TMDB looks it up, and nothing else is
        // asked.
        if let Some(imdb) = imdb_id(query) {
            let (outcome, candidates) = self.tmdb_find(&context, configured("tmdb"), &imdb).await;
            return Found {
                candidates,
                sources: vec![outcome],
            };
        }

        let wants = |source| suits(source, kind);
        let (dat, igdb, musicbrainz, tmdb) = tokio::join!(
            async {
                if wants(Source::Dat) {
                    Some(dats(&context, kind).await)
                } else {
                    None
                }
            },
            async {
                if wants(Source::Igdb) {
                    Some(self.igdb(&context, configured("igdb")).await)
                } else {
                    None
                }
            },
            async {
                if wants(Source::Musicbrainz) {
                    Some(self.musicbrainz(&context, configured("musicbrainz")).await)
                } else {
                    None
                }
            },
            async {
                if wants(Source::Movies) || wants(Source::Tv) {
                    Some(
                        self.tmdb(
                            &context,
                            configured("tmdb"),
                            wants(Source::Movies),
                            wants(Source::Tv),
                        )
                        .await,
                    )
                } else {
                    None
                }
            },
        );
        let mut found = Found::default();
        for (outcome, candidates) in [dat, igdb, musicbrainz, tmdb].into_iter().flatten() {
            found.sources.push(outcome);
            found.candidates.extend(candidates);
        }
        found
    }

    /// The integration for `provider`, ready to use, or why not.
    fn usable<'a>(
        provider: &'static str,
        record: Option<&'a IntegrationRecord>,
    ) -> Result<&'a IntegrationRecord, SourceOutcome> {
        let Some(record) = record else {
            return Err(SourceOutcome::not_there(
                provider,
                SourceState::Unconfigured,
                format!(
                    "{} is not configured; an administrator can add it on the Integrations page",
                    named(provider)
                ),
            ));
        };
        if !record.enabled {
            return Err(SourceOutcome::not_there(
                provider,
                SourceState::Off,
                format!(
                    "{} is turned off; an administrator can turn it on",
                    named(provider)
                ),
            ));
        }
        Ok(record)
    }

    async fn musicbrainz(
        &self,
        context: &Context<'_>,
        record: Option<&IntegrationRecord>,
    ) -> (SourceOutcome, Vec<Candidate>) {
        let provider = musicbrainz::PROVIDER;
        if let Err(outcome) = Self::usable(provider, record) {
            return (outcome, Vec::new());
        }
        let client = MusicBrainzClient::new(
            self.inner.http.clone(),
            self.inner.endpoints.musicbrainz.clone(),
            USER_AGENT,
            self.inner.musicbrainz_pace.clone(),
        );
        answered(
            provider,
            self.through_cache(provider, context.query, client.search(context.query))
                .await,
        )
    }

    async fn tmdb_client(
        &self,
        context: &Context<'_>,
        record: Option<&IntegrationRecord>,
    ) -> Result<TmdbClient, SourceOutcome> {
        let provider = tmdb::PROVIDER;
        let record = Self::usable(provider, record)?;
        let token = context
            .required(record, "read_access_token", "read access token")
            .await
            .map_err(|error| open_failed(provider, &error))?;
        Ok(TmdbClient::new(
            self.inner.http.clone(),
            self.inner.endpoints.tmdb.clone(),
            token,
            self.inner.tmdb_pace.clone(),
        ))
    }

    async fn tmdb(
        &self,
        context: &Context<'_>,
        record: Option<&IntegrationRecord>,
        movies: bool,
        tv: bool,
    ) -> (SourceOutcome, Vec<Candidate>) {
        let provider = tmdb::PROVIDER;
        let client = match self.tmdb_client(context, record).await {
            Ok(client) => client,
            Err(outcome) => return (outcome, Vec::new()),
        };
        let query = context.query;
        let (films, shows) = tokio::join!(
            async {
                if movies {
                    self.through_cache("tmdb:movie", query, client.search_movies(query))
                        .await
                } else {
                    Ok(Vec::new())
                }
            },
            async {
                if tv {
                    self.through_cache("tmdb:tv", query, client.search_tv(query))
                        .await
                } else {
                    Ok(Vec::new())
                }
            },
        );
        match (films, shows) {
            (Ok(mut films), Ok(shows)) => {
                films.extend(shows);
                films.truncate(MAX_CANDIDATES * 2);
                (SourceOutcome::answered(provider, films.len()), films)
            }
            (Err(error), _) | (_, Err(error)) => (client_failed(provider, &error), Vec::new()),
        }
    }

    async fn tmdb_find(
        &self,
        context: &Context<'_>,
        record: Option<&IntegrationRecord>,
        imdb: &str,
    ) -> (SourceOutcome, Vec<Candidate>) {
        let provider = tmdb::PROVIDER;
        let client = match self.tmdb_client(context, record).await {
            Ok(client) => client,
            Err(mut outcome) => {
                if outcome.state == SourceState::Unconfigured {
                    outcome.detail = Some(
                        "an IMDb ID is looked up through TMDB, which is not configured; an \
                         administrator can add it on the Integrations page"
                            .to_owned(),
                    );
                }
                return (outcome, Vec::new());
            }
        };
        answered(
            provider,
            self.through_cache("tmdb:find", imdb, client.find_imdb(imdb))
                .await,
        )
    }

    async fn igdb(
        &self,
        context: &Context<'_>,
        record: Option<&IntegrationRecord>,
    ) -> (SourceOutcome, Vec<Candidate>) {
        let provider = igdb::PROVIDER;
        let client = match self.igdb_client(context, record).await {
            Ok(client) => client,
            Err(outcome) => return (outcome, Vec::new()),
        };
        answered(
            provider,
            self.through_cache(provider, context.query, client.search(context.query))
                .await,
        )
    }

    async fn igdb_client(
        &self,
        context: &Context<'_>,
        record: Option<&IntegrationRecord>,
    ) -> Result<IgdbClient, SourceOutcome> {
        let provider = igdb::PROVIDER;
        let record = Self::usable(provider, record)?;
        let client_id = context
            .required(record, "client_id", "Twitch client ID")
            .await
            .map_err(|error| open_failed(provider, &error))?;
        let client_secret = context
            .required(record, "client_secret", "Twitch client secret")
            .await
            .map_err(|error| open_failed(provider, &error))?;
        Ok(IgdbClient::new(
            self.inner.http.clone(),
            self.inner.endpoints.igdb.clone(),
            client_id,
            client_secret,
            self.inner.igdb_pace.clone(),
            self.inner.igdb_tokens.clone(),
        ))
    }

    /// Try a provider's integration: whether it answers, and takes its key.
    ///
    /// # Errors
    ///
    /// [`OpenError`] when its client cannot be made (no key, a key that
    /// does not open); a provider that fails is a [`ClientError`].
    pub async fn check(
        &self,
        database: &Database,
        key: Option<&SecretKey>,
        record: &IntegrationRecord,
    ) -> Result<Result<(), ClientError>, OpenError> {
        let context = Context {
            database,
            key,
            query: "",
        };
        let opened = |outcome: SourceOutcome| OpenError {
            code: outcome.error_code.unwrap_or("INTEGRATION_OFF"),
            detail: outcome.detail.unwrap_or_default(),
        };
        match record.kind.as_str() {
            "musicbrainz" => {
                Self::usable(musicbrainz::PROVIDER, Some(record)).map_err(opened)?;
                let client = MusicBrainzClient::new(
                    self.inner.http.clone(),
                    self.inner.endpoints.musicbrainz.clone(),
                    USER_AGENT,
                    self.inner.musicbrainz_pace.clone(),
                );
                Ok(client.check().await)
            }
            "tmdb" => {
                let client = self
                    .tmdb_client(&context, Some(record))
                    .await
                    .map_err(opened)?;
                Ok(client.check().await)
            }
            "igdb" => {
                let client = self
                    .igdb_client(&context, Some(record))
                    .await
                    .map_err(opened)?;
                Ok(client.check().await)
            }
            _ => Err(OpenError {
                code: "INTEGRATION_OFF",
                detail: format!("{} is not a metadata provider", record.name),
            }),
        }
    }
}

/// What every source of one search shares.
struct Context<'a> {
    database: &'a Database,
    key: Option<&'a SecretKey>,
    query: &'a str,
}

impl Context<'_> {
    /// A secret the provider cannot work without.
    async fn required(
        &self,
        record: &IntegrationRecord,
        field: &str,
        what: &str,
    ) -> Result<SecretString, OpenError> {
        secret(self.database, self.key, record, field)
            .await?
            .ok_or_else(|| OpenError {
                code: "INTEGRATION_MISCONFIGURED",
                detail: format!(
                    "{} has no {what}; an administrator can set one",
                    record.name
                ),
            })
    }
}

fn answered(
    provider: &'static str,
    result: Result<Vec<Candidate>, ClientError>,
) -> (SourceOutcome, Vec<Candidate>) {
    match result {
        Ok(candidates) => (
            SourceOutcome::answered(provider, candidates.len()),
            candidates,
        ),
        Err(error) => {
            tracing::warn!(provider, code = error.code(), error = %error, "a metadata provider failed");
            (client_failed(provider, &error), Vec::new())
        }
    }
}

/// A search on a server that reaches no providers: its DATs alone.
pub async fn search_dats_only(
    database: &Database,
    query: &str,
    kind: Option<RequestKind>,
) -> Found {
    if imdb_id(query).is_some() {
        return Found {
            candidates: Vec::new(),
            sources: vec![SourceOutcome::failed(
                tmdb::PROVIDER,
                "INTEGRATION_OFF",
                "this server reaches no metadata providers".to_owned(),
            )],
        };
    }
    if !suits(Source::Dat, kind) {
        return Found::default();
    }
    let context = Context {
        database,
        key: None,
        query,
    };
    let (outcome, candidates) = dats(&context, kind).await;
    Found {
        candidates,
        sources: vec![outcome],
    }
}

/// The loaded DATs' games matching the query.
async fn dats(context: &Context<'_>, kind: Option<RequestKind>) -> (SourceOutcome, Vec<Candidate>) {
    let provider = "dat";
    let hits = match tangible_db::dats::search_games(
        context.database.pool(),
        context.query,
        i64::try_from(MAX_CANDIDATES).unwrap_or(25),
    )
    .await
    {
        Ok(hits) => hits,
        Err(error) => {
            tracing::error!(error = ?error, "the DATs could not be searched");
            return (
                SourceOutcome::failed(
                    provider,
                    "STORAGE_UNAVAILABLE",
                    "the DATs could not be searched; see server logs".to_owned(),
                ),
                Vec::new(),
            );
        }
    };
    let kind = match kind {
        Some(RequestKind::Software) => RequestKind::Software,
        Some(RequestKind::Other) => RequestKind::Other,
        _ => RequestKind::Game,
    };
    let candidates: Vec<Candidate> = hits
        .iter()
        .filter_map(|hit| {
            let mut candidate = Candidate::new(provider, kind, &hit.name)?;
            candidate.detail("platform", &hit.dat_name);
            if let Some(description) = hit.description.as_deref()
                && clean(description, MAX_TITLE_CHARS).as_deref() != Some(candidate.title.as_str())
            {
                candidate.detail("description", description);
            }
            candidate.reference(provider, &hit.name);
            Some(candidate)
        })
        .collect();
    (
        SourceOutcome::answered(provider, candidates.len()),
        candidates,
    )
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_asks_the_sources_that_know_it() {
        let asked = |kind| {
            [
                Source::Dat,
                Source::Igdb,
                Source::Musicbrainz,
                Source::Movies,
                Source::Tv,
            ]
            .into_iter()
            .filter(|source| suits(*source, kind))
            .collect::<Vec<_>>()
        };
        assert_eq!(asked(Some(RequestKind::Game)), [Source::Dat, Source::Igdb]);
        assert_eq!(asked(Some(RequestKind::Music)), [Source::Musicbrainz]);
        assert_eq!(asked(Some(RequestKind::Movie)), [Source::Movies]);
        assert_eq!(asked(Some(RequestKind::Television)), [Source::Tv]);
        assert_eq!(asked(Some(RequestKind::Software)), [Source::Dat]);
        assert_eq!(asked(Some(RequestKind::Other)), [Source::Dat]);
        assert_eq!(asked(None).len(), 5);
    }

    #[test]
    fn the_user_agent_names_tangible_and_where_it_is_made() {
        assert!(USER_AGENT.starts_with("Tangible/"));
        assert!(USER_AGENT.ends_with("( https://github.com/digital-grease/tangible )"));
    }
}
