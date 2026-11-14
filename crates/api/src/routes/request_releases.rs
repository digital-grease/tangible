// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Searching for an approved request's releases through Prowlarr, and
//! grabbing the one a person picks (ADR 0021, docs/plans/requests.md Q3).
//!
//! For operators and administrators. A search asks Prowlarr, ranks what it
//! found, and holds it for an hour; nothing is grabbed until a person picks
//! one. A grab fetches the release's `.torrent`, NZB or magnet link through
//! Prowlarr, checks it as a submitted job is checked, and hands it to a
//! download client of its protocol as `POST /integrations/{id}/jobs` does;
//! the request then follows the import that brings it in. A release's link
//! carries Prowlarr's API key, so it is never answered, logged or audited.

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tangible_db::integrations::{IntegrationRecord, Submitted, list_integrations};
use tangible_db::requests::{
    Download, RequestRecord, RequestRefusal, get_request, record_search, start_download,
};
use tangible_domain::RequestId;
use tangible_integrations::prowlarr::{Fetched, Protocol, ProwlarrClient, Search};
use utoipa::ToSchema;

use crate::auth::SignedIn;
use crate::download_clients::{open_client, open_prowlarr};
use crate::problem::{ErrorCode, Problem};
use crate::releases::{
    HeldRelease, HeldSearch, IndexerState, MAX_WORDS_CHARS, plan, ranked, words_of,
};
use crate::routes::integration_jobs::{Submission, deliver};
use crate::routes::integrations::{client_problem, note_health, open_problem};
use crate::routes::requests::{
    RequestView, invalid, line, parse_id, rate_limited, refused, rfc3339, unavailable,
};
use crate::secrets::SecretKey;
use crate::state::ApiState;

/// What to search for: the request's own title, unless words are given.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchReleasesRequest {
    /// Words to search for instead of the request's title, up to 200
    /// characters.
    #[serde(default)]
    pub query: Option<String>,
}

/// A release found, as a person picks from it. Its link is never answered.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ReleaseView {
    /// The result's id, which a grab names.
    pub id: String,
    /// Its title, as the indexer gives it.
    pub title: String,
    /// The indexer that has it, as Prowlarr names it.
    pub indexer: String,
    /// `torrent` or `usenet`.
    pub protocol: String,
    /// Its size.
    pub size_bytes: Option<u64>,
    /// A torrent's seeders.
    pub seeders: Option<u32>,
    /// A torrent's leechers.
    pub leechers: Option<u32>,
    /// How often it was grabbed, where the indexer says.
    pub grabs: Option<u32>,
    /// When it was posted.
    pub published_at: Option<String>,
    /// Its categories, by name, such as `Movies/DVD`.
    pub categories: Vec<String>,
    /// How well it matches what was asked for: higher is better.
    pub score: i32,
    /// Why it scores as it does, in words.
    pub reasons: Vec<String>,
    /// Whether a download client for its protocol is configured and turned
    /// on, so it can be grabbed.
    pub can_grab: bool,
}

/// An indexer Prowlarr has paused after it failed.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PausedIndexerView {
    /// Its name, in Prowlarr.
    pub name: String,
    /// Until when.
    pub until: Option<String>,
}

/// A download client a release may be handed to.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DownloadClientChoiceView {
    /// The integration.
    pub id: String,
    /// Its name.
    pub name: String,
    /// `torrent` or `usenet`: what it takes.
    pub protocol: String,
}

/// A search for a request's releases, held for a person to pick from.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ReleaseSearchView {
    /// The request.
    pub request_id: String,
    /// `words`, or `imdb_id` for a film found by its IMDb ID.
    pub searched_by: String,
    /// The words searched for, or the IMDb ID.
    pub query: String,
    /// When.
    pub searched_at: String,
    /// Until when a release may be grabbed from it; search again after.
    pub held_until: String,
    /// What was found, best first, at most 100.
    pub results: Vec<ReleaseView>,
    /// How many indexers Prowlarr has turned on, if it said.
    pub indexers_enabled: Option<u32>,
    /// Those it has paused after they failed: their releases are missing.
    pub indexers_paused: Vec<PausedIndexerView>,
    /// The download clients a release may go to.
    pub clients: Vec<DownloadClientChoiceView>,
}

/// Which release to grab, and, when more than one client takes its
/// protocol, which client.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GrabReleaseRequest {
    /// A result's id, from the search held for the request.
    pub result_id: String,
    /// The download client to hand it to; needed only when more than one
    /// takes its protocol.
    #[serde(default)]
    pub integration_id: Option<String>,
}

/// A release grabbed: the request, now following the import.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct GrabView {
    /// The request, downloading (or fulfilled, when the library already
    /// had what the release holds).
    pub request: RequestView,
    /// `created`; `resubmitted` when an earlier import of the same job ended
    /// without an artifact; `already_active` when one is still going;
    /// `already_imported` when it is in the library.
    pub outcome: String,
    /// The download client it went to.
    pub integration_id: String,
    /// The import that brings it in.
    pub import_id: String,
}

/// What a download client kind takes.
fn takes(kind: &str) -> Option<Protocol> {
    match kind {
        "qbittorrent" => Some(Protocol::Torrent),
        "sabnzbd" | "nzbget" => Some(Protocol::Usenet),
        _ => None,
    }
}

const fn protocol_words(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Torrent => "torrents",
        Protocol::Usenet => "Usenet",
    }
}

async fn integrations(state: &ApiState) -> Result<Vec<IntegrationRecord>, Problem> {
    list_integrations(state.database().pool())
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "integrations could not be read");
            Problem::new(
                ErrorCode::StorageUnavailable,
                "integrations could not be read; see server logs",
            )
        })
}

/// The request, if a release may be searched for or grabbed for it now.
async fn grabbable(state: &ApiState, id: RequestId, doing: &str) -> Result<RequestRecord, Problem> {
    let record = get_request(state.database().pool(), id)
        .await
        .map_err(|error| unavailable(&error))?
        .ok_or_else(|| Problem::not_found("request", &id.to_string()))?;
    if record.state.may_grab() {
        Ok(record)
    } else {
        Err(refused(RequestRefusal::Conflict(record.state), id, doing))
    }
}

fn no_key() -> Problem {
    Problem::new(
        ErrorCode::SecretKeyMissing,
        "this server holds no key to seal search results with (TANGIBLE_SECRET_KEY_FILE)",
    )
}

/// The Prowlarr configured, opened.
async fn prowlarr<'a>(
    state: &ApiState,
    all: &'a [IntegrationRecord],
    key: &SecretKey,
    id: Option<uuid::Uuid>,
) -> Result<(&'a IntegrationRecord, ProwlarrClient), Problem> {
    let record = all
        .iter()
        .find(|record| record.kind == "prowlarr" && id.is_none_or(|id| record.id == id))
        .ok_or_else(|| {
            Problem::new(
                ErrorCode::IntegrationOff,
                if id.is_some() {
                    "the Prowlarr that was searched is no longer configured; search again"
                } else {
                    "no Prowlarr is configured; an administrator adds one on the Integrations \
                     page"
                },
            )
        })?;
    let client = open_prowlarr(
        state.database(),
        state.integration_client(),
        Some(key),
        record,
    )
    .await
    .map_err(|error| open_problem(&error))?;
    Ok((record, client))
}

/// The download clients a release may go to: those turned on.
fn clients(all: &[IntegrationRecord]) -> Vec<DownloadClientChoiceView> {
    all.iter()
        .filter(|record| record.enabled)
        .filter_map(|record| {
            Some(DownloadClientChoiceView {
                id: record.id.to_string(),
                name: record.name.clone(),
                protocol: takes(&record.kind)?.as_str().to_owned(),
            })
        })
        .collect()
}

fn search_view(
    id: RequestId,
    held: &HeldSearch,
    clients: Vec<DownloadClientChoiceView>,
) -> ReleaseSearchView {
    let (searched_by, query) = match held.query.strip_prefix("{ImdbId:") {
        Some(rest) => ("imdb_id", rest.trim_end_matches('}').to_owned()),
        None => ("words", held.query.clone()),
    };
    let results = held
        .releases
        .iter()
        .map(|held| {
            let release = &held.release;
            ReleaseView {
                id: held.id.to_string(),
                title: release.title.clone(),
                indexer: release.indexer.clone(),
                protocol: release.protocol.as_str().to_owned(),
                size_bytes: release.size_bytes,
                seeders: release.seeders,
                leechers: release.leechers,
                grabs: release.grabs,
                published_at: release.published_at.map(rfc3339),
                categories: release
                    .categories
                    .iter()
                    .map(|category| category.name.clone())
                    .collect(),
                score: held.score,
                reasons: held.reasons.clone(),
                can_grab: clients
                    .iter()
                    .any(|client| client.protocol == release.protocol.as_str()),
            }
        })
        .collect();
    ReleaseSearchView {
        request_id: id.to_string(),
        searched_by: searched_by.to_owned(),
        query,
        searched_at: rfc3339(held.searched_at),
        held_until: rfc3339(held.held_until()),
        results,
        indexers_enabled: held
            .indexers
            .enabled
            .and_then(|count| u32::try_from(count).ok()),
        indexers_paused: held
            .indexers
            .paused
            .iter()
            .map(|(name, until)| PausedIndexerView {
                name: name.clone(),
                until: until.map(rfc3339),
            })
            .collect(),
        clients,
    }
}

/// Ask Prowlarr each search the plan makes until one finds something.
async fn ask(
    state: &ApiState,
    prowlarr_id: uuid::Uuid,
    prowlarr: &ProwlarrClient,
    searches: Vec<Search>,
) -> Result<(Search, Vec<tangible_integrations::prowlarr::Release>), Problem> {
    let mut answered = None;
    for search in searches {
        match prowlarr.search(&search).await {
            Ok(found) => {
                let done = !found.is_empty();
                answered = Some((search, found));
                if done {
                    break;
                }
            }
            Err(error) => {
                note_health(state, prowlarr_id, Err(error.code())).await;
                return Err(client_problem(&error));
            }
        }
    }
    note_health(state, prowlarr_id, Ok(None)).await;
    answered.ok_or_else(|| Problem::new(ErrorCode::Internal, "nothing was searched for"))
}

/// How Prowlarr's indexers stand: as far as it says, since a search has
/// already answered.
async fn indexer_state(prowlarr: &ProwlarrClient) -> IndexerState {
    let indexers = match prowlarr.indexers().await {
        Ok(indexers) => indexers,
        Err(error) => {
            tracing::warn!(error = %error, "Prowlarr's indexers could not be read");
            return IndexerState::default();
        }
    };
    let now = time::OffsetDateTime::now_utc();
    let paused = prowlarr.paused().await.unwrap_or_default();
    IndexerState {
        enabled: Some(indexers.iter().filter(|indexer| indexer.enabled).count()),
        paused: paused
            .into_iter()
            .filter(|paused| paused.until.is_some_and(|until| until > now))
            .map(|paused| {
                let name = indexers
                    .iter()
                    .find(|indexer| indexer.id == paused.indexer_id)
                    .map_or_else(
                        || format!("indexer {}", paused.indexer_id),
                        |indexer| indexer.name.clone(),
                    );
                (name, paused.until)
            })
            .collect(),
    }
}

/// Search Prowlarr for a request's releases, ranked, and hold them.
///
/// # Errors
///
/// `NOT_FOUND`; `CONFLICT` for a request not approved, or downloading or
/// ended; `VALIDATION_FAILED` for words that will not do; `INTEGRATION_OFF`
/// with no Prowlarr; Prowlarr's failures as `INTEGRATION_*`;
/// `RATE_LIMITED` past 60 an hour.
#[utoipa::path(
    post,
    path = "/api/v1/requests/{request_id}/search",
    tag = "requests",
    description = "Search Prowlarr for an approved request's releases: by the request's title, \\
                   cleaned, in the categories its kind suits (a film by its IMDb ID first, \\
                   where it has one), or by the words given. The results are ranked by how well \\
                   they match, each saying why, and held for an hour for a person to pick one \\
                   to grab; nothing is grabbed here. A release's link is never answered. \\
                   Operators and administrators; at most 60 an hour per account.",
    params(("request_id" = String, Path, description = "Request identifier")),
    request_body = SearchReleasesRequest,
    responses(
        (status = 200, description = "What was found, held", body = ReleaseSearchView),
        (status = 404, description = "No such request", body = Problem),
        (status = 409, description = "Not approved, or downloading or ended; or no Prowlarr", body = Problem),
        (status = 422, description = "Words that will not do", body = Problem),
        (status = 429, description = "Too many searches from this account for now", body = Problem),
        (status = 502, description = "Prowlarr failed", body = Problem),
    ),
)]
pub async fn search_releases(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(request_id): Path<String>,
    Json(request): Json<SearchReleasesRequest>,
) -> Result<Response, Problem> {
    let id = parse_id(&request_id)?;
    let words = request
        .query
        .as_deref()
        .map(|words| line("query", words, MAX_WORDS_CHARS))
        .transpose()?;
    let record = grabbable(&state, id, "searched for").await?;
    let key = state.secret_key().ok_or_else(no_key)?;
    let all = integrations(&state).await?;
    let (prowlarr_record, prowlarr) = prowlarr(&state, &all, key, None).await?;
    if let Err(wait) = state.release_limiter().try_acquire(&user.username) {
        return Ok(rate_limited(wait));
    }

    let (search, found) = ask(
        &state,
        prowlarr_record.id,
        &prowlarr,
        plan(&record, words.as_deref()),
    )
    .await?;
    let indexers = indexer_state(&prowlarr).await;
    let ranking_words = words.unwrap_or_else(|| words_of(&record.title));
    let releases = ranked(&record, &ranking_words, found)
        .into_iter()
        .map(|(release, score, reasons)| HeldRelease::seal(key, release, score, reasons))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| Problem::new(ErrorCode::Internal, "the results could not be sealed"))?;
    let held = HeldSearch::new(prowlarr_record.id, search.query, releases, indexers);

    record_search(
        state.database().pool(),
        id,
        &user.username,
        &held.query,
        held.releases.len(),
    )
    .await
    .map_err(|error| unavailable(&error))?
    .map_err(|refusal| refused(refusal, id, "searched for"))?;
    tracing::info!(
        request_id = %id,
        found = held.releases.len(),
        by = %user.username,
        "request searched for"
    );
    let view = search_view(id, &held, clients(&all));
    state.releases().hold(*id.as_uuid(), held);
    Ok(Json(view).into_response())
}

/// The search held for a request.
///
/// # Errors
///
/// `NOT_FOUND` for no such request, or none held for it.
#[utoipa::path(
    get,
    path = "/api/v1/requests/{request_id}/search",
    tag = "requests",
    description = "The search held for a request, for an hour after it was made. Operators and \\
                   administrators.",
    params(("request_id" = String, Path, description = "Request identifier")),
    responses(
        (status = 200, description = "What was found, held", body = ReleaseSearchView),
        (status = 404, description = "No such request, or no search held for it", body = Problem),
    ),
)]
pub async fn held_search(
    State(state): State<ApiState>,
    Path(request_id): Path<String>,
) -> Result<Json<ReleaseSearchView>, Problem> {
    let id = parse_id(&request_id)?;
    get_request(state.database().pool(), id)
        .await
        .map_err(|error| unavailable(&error))?
        .ok_or_else(|| Problem::not_found("request", &id.to_string()))?;
    let held = state.releases().get(*id.as_uuid()).ok_or_else(|| {
        Problem::new(
            ErrorCode::NotFound,
            "no search is held for this request; search for it",
        )
    })?;
    let all = integrations(&state).await?;
    Ok(Json(search_view(id, &held, clients(&all))))
}

fn expired() -> Problem {
    Problem::new(
        ErrorCode::SearchExpired,
        "that result is not among those held for this request: the search was more than an \
         hour ago, or before the server restarted, or it was searched again since; search again",
    )
}

/// The download client a release of `protocol` goes to: the one chosen, or
/// the only one there is.
fn choose(
    all: &[IntegrationRecord],
    protocol: Protocol,
    chosen: Option<uuid::Uuid>,
) -> Result<&IntegrationRecord, Problem> {
    let fit: Vec<&IntegrationRecord> = all
        .iter()
        .filter(|record| record.enabled && takes(&record.kind) == Some(protocol))
        .collect();
    let what = protocol_words(protocol);
    if let Some(chosen) = chosen {
        return fit
            .into_iter()
            .find(|record| record.id == chosen)
            .ok_or_else(|| {
                invalid(format!(
                    "integration_id is not a download client for {what} that is turned on"
                ))
            });
    }
    match fit.as_slice() {
        [one] => Ok(one),
        [] => Err(Problem::new(
            ErrorCode::IntegrationOff,
            format!(
                "no download client for {what} is configured and turned on; an administrator \
                 adds one on the Integrations page"
            ),
        )),
        many => Err(invalid(format!(
            "more than one download client takes {what}; choose one by integration_id: {}",
            many.iter()
                .map(|record| record.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// What a release's link gave, as a job for a client of its protocol,
/// checked as a submitted one is.
fn submission_of(protocol: Protocol, fetched: Fetched) -> Result<Submission, Problem> {
    let bad = |detail: String| {
        Problem::new(
            ErrorCode::IntegrationBadResponse,
            format!("the release's link gave {detail}"),
        )
    };
    match (protocol, fetched) {
        (Protocol::Torrent, Fetched::Magnet(link)) => Submission::magnet(&link)
            .map_err(|error| bad(format!("a magnet link that will not do: {error}"))),
        (Protocol::Torrent, Fetched::File(bytes)) => Submission::torrent(bytes)
            .map_err(|error| bad(format!("something that is not a usable .torrent: {error}"))),
        (Protocol::Usenet, Fetched::File(bytes)) => Submission::nzb(bytes)
            .map_err(|error| bad(format!("something that is not a usable NZB: {error}"))),
        (Protocol::Usenet, Fetched::Magnet(_)) => {
            Err(bad("a magnet link, for a Usenet release".to_owned()))
        }
    }
}

/// The import a handover made or found, and how.
const fn outcome_of(submitted: &Submitted) -> (&'static str, uuid::Uuid) {
    match submitted {
        Submitted::Created { import_id } => ("created", *import_id),
        Submitted::Resubmitted { import_id } => ("resubmitted", *import_id),
        Submitted::AlreadyActive { import_id } => ("already_active", *import_id),
        Submitted::AlreadyImported { import_id, .. } => ("already_imported", *import_id),
    }
}

/// Fetch a held release through Prowlarr, as a job its client can take.
async fn fetch(
    state: &ApiState,
    all: &[IntegrationRecord],
    key: &SecretKey,
    held: &HeldSearch,
    release: &HeldRelease,
) -> Result<Submission, Problem> {
    let (prowlarr_record, prowlarr) = prowlarr(state, all, key, Some(held.prowlarr_id)).await?;
    let link = release
        .link(key)
        .map_err(|_| {
            Problem::new(
                ErrorCode::IntegrationSecretUnreadable,
                "the result's link does not open under this server's key; search again",
            )
        })?
        .ok_or_else(|| {
            Problem::new(
                ErrorCode::IntegrationBadResponse,
                "Prowlarr gave this release no link to fetch it by",
            )
        })?;
    let fetched = match prowlarr.fetch(&link).await {
        Ok(fetched) => fetched,
        Err(error) => {
            note_health(state, prowlarr_record.id, Err(error.code())).await;
            return Err(client_problem(&error));
        }
    };
    submission_of(release.release.protocol, fetched)
}

/// Grab a release from the search held for a request.
///
/// # Errors
///
/// `VALIDATION_FAILED` for ids that will not do, or more than one client
/// and none chosen; `NOT_FOUND`; `CONFLICT` for a request not approved, or
/// downloading or ended; `SEARCH_EXPIRED` for a result no longer held;
/// `INTEGRATION_OFF` with no client for its protocol; Prowlarr's and the
/// client's failures as `INTEGRATION_*`.
#[utoipa::path(
    post,
    path = "/api/v1/requests/{request_id}/grab",
    tag = "requests",
    description = "Grab a release a person picked from the search held for an approved \\
                   request: its .torrent, NZB or magnet link is fetched through Prowlarr, \\
                   checked as a submitted job is, and handed to a download client of its \\
                   protocol (the one named, or the only one there is) in Tangible's category. \\
                   The request then follows the import that brings it in, and is fulfilled \\
                   when that completes. Operators and administrators.",
    params(("request_id" = String, Path, description = "Request identifier")),
    request_body = GrabReleaseRequest,
    responses(
        (status = 200, description = "Handed over; the request follows its import", body = GrabView),
        (status = 404, description = "No such request", body = Problem),
        (status = 409, description = "Not approved, or downloading or ended; the search \\
                                      expired (SEARCH_EXPIRED); or no client for it", body = Problem),
        (status = 422, description = "Ids that will not do, or a client to choose", body = Problem),
        (status = 502, description = "Prowlarr or the client failed, or gave what will not do", body = Problem),
    ),
)]
pub async fn grab_release(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(request_id): Path<String>,
    Json(request): Json<GrabReleaseRequest>,
) -> Result<Json<GrabView>, Problem> {
    let id = parse_id(&request_id)?;
    let result_id: uuid::Uuid = request
        .result_id
        .trim()
        .parse()
        .map_err(|_| invalid("result_id is not a result's id"))?;
    let chosen = request
        .integration_id
        .as_deref()
        .map(|raw| raw.trim().parse::<uuid::Uuid>())
        .transpose()
        .map_err(|_| invalid("integration_id is not an integration's id"))?;
    grabbable(&state, id, "grabbed").await?;
    let held = state.releases().get(*id.as_uuid()).ok_or_else(expired)?;
    let release = held
        .releases
        .iter()
        .find(|release| release.id == result_id)
        .ok_or_else(expired)?;
    let key = state.secret_key().ok_or_else(no_key)?;
    let all = integrations(&state).await?;
    let client_record = choose(&all, release.release.protocol, chosen)?;
    let submission = fetch(&state, &all, key, &held, release).await?;

    let (mut client, folders) = open_client(
        state.database(),
        state.integration_client(),
        Some(key),
        client_record,
    )
    .await
    .map_err(|error| open_problem(&error))?;
    let delivered = deliver(
        &state,
        client_record.id,
        &mut client,
        &folders,
        &submission,
        &user.username,
    )
    .await?;
    let (outcome, import_id) = outcome_of(&delivered.submitted);
    let record = start_download(
        state.database().pool(),
        id,
        &Download {
            actor: &user.username,
            integration_id: client_record.id,
            import_id,
            release: serde_json::json!({
                "title": release.release.title,
                "indexer": release.release.indexer,
                "protocol": release.release.protocol.as_str(),
                "size_bytes": release.release.size_bytes,
                "integration": client_record.name,
                "outcome": outcome,
            }),
        },
    )
    .await
    .map_err(|error| unavailable(&error))?
    .map_err(|refusal| refused(refusal, id, "grabbed"))?;
    tracing::info!(
        request_id = %id,
        integration_id = %client_record.id,
        import_id = %import_id,
        outcome,
        by = %user.username,
        "release grabbed for a request"
    );
    Ok(Json(GrabView {
        request: RequestView::of(record, &user),
        outcome: outcome.to_owned(),
        integration_id: client_record.id.to_string(),
        import_id: import_id.to_string(),
    }))
}

/// The routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route(
            "/requests/{request_id}/search",
            get(held_search).post(search_releases),
        )
        .route("/requests/{request_id}/grab", post(grab_release))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn record(kind: &str, name: &str, enabled: bool) -> IntegrationRecord {
        let now = time::OffsetDateTime::now_utc();
        IntegrationRecord {
            id: uuid::Uuid::now_v7(),
            kind: kind.to_owned(),
            name: name.to_owned(),
            enabled,
            configuration: serde_json::json!({}),
            status: "ok".to_owned(),
            last_success_at: None,
            last_error_code: None,
            client_version: None,
            secret_fields: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn a_release_goes_to_the_one_client_for_its_protocol_or_the_one_chosen() {
        let qbittorrent = record("qbittorrent", "qBittorrent", true);
        let sabnzbd = record("sabnzbd", "SABnzbd", true);
        let off = record("nzbget", "NZBGet", false);
        let all = vec![qbittorrent.clone(), sabnzbd.clone(), off.clone()];
        assert_eq!(
            choose(&all, Protocol::Torrent, None).unwrap().id,
            qbittorrent.id
        );
        assert_eq!(choose(&all, Protocol::Usenet, None).unwrap().id, sabnzbd.id);
        // One turned off is not chosen, even by name.
        let error = choose(&all, Protocol::Usenet, Some(off.id)).unwrap_err();
        assert_eq!(error.code, "VALIDATION_FAILED");
        // A client for the other protocol is not chosen either.
        assert!(choose(&all, Protocol::Torrent, Some(sabnzbd.id)).is_err());

        let both = vec![sabnzbd.clone(), record("nzbget", "NZBGet", true)];
        let error = choose(&both, Protocol::Usenet, None).unwrap_err();
        assert!(error.detail.contains("SABnzbd, NZBGet"), "{}", error.detail);
        let none = choose(&[sabnzbd], Protocol::Torrent, None).unwrap_err();
        assert_eq!(none.code, "INTEGRATION_OFF");
        assert_eq!(clients(&all).len(), 2);
    }

    #[test]
    fn what_a_link_gives_must_suit_the_releases_protocol() {
        let error = submission_of(Protocol::Usenet, Fetched::Magnet("magnet:?x".to_owned()))
            .err()
            .unwrap();
        assert_eq!(error.code, "INTEGRATION_BAD_RESPONSE");
        let error = submission_of(Protocol::Torrent, Fetched::File(b"<html>".to_vec()))
            .err()
            .unwrap();
        assert!(error.detail.contains(".torrent"), "{}", error.detail);
        let error = submission_of(Protocol::Usenet, Fetched::File(b"d4:infoe".to_vec()))
            .err()
            .unwrap();
        assert!(error.detail.contains("NZB"), "{}", error.detail);
        assert!(
            submission_of(
                Protocol::Torrent,
                Fetched::Magnet(
                    "magnet:?xt=urn:btih:ed0d831bcefc077847bc1fa808bd2ae5fd32ae9b".to_owned()
                )
            )
            .is_ok()
        );
    }
}
