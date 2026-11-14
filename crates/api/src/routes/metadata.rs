// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Searching metadata for an item to request (ADR 0021,
//! docs/plans/requests.md Q2): loaded DATs, and MusicBrainz, TMDB and IGDB
//! where an administrator configured them. Anybody who may ask for an item
//! may search. A candidate carries what a request takes, so one can be
//! asked for as it is.

use std::collections::BTreeMap;

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tangible_domain::RequestKind;
use tangible_integrations::metadata::{Candidate, MAX_QUERY_CHARS};
use utoipa::{IntoParams, ToSchema};

use crate::auth::SignedIn;
use crate::metadata::{Found, SourceOutcome};
use crate::problem::{ErrorCode, Problem};
use crate::routes::requests::RequestRefView;
use crate::state::ApiState;

/// What to search for.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct MetadataQuery {
    /// Words of a title (and, for music, an artist), or an IMDb ID or
    /// address. Up to 200 characters.
    pub q: String,
    /// `game`, `music`, `movie`, `television`, `software` or `other`; all
    /// sources when absent.
    pub kind: Option<String>,
}

/// An item a source knows.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MetadataCandidateView {
    /// `dat`, `musicbrainz`, `tmdb` or `igdb`.
    pub provider: String,
    /// What kind of item it is.
    pub kind: String,
    /// Its title.
    pub title: String,
    /// Platform, year, region and the like, as a request stores them.
    pub details: BTreeMap<String, String>,
    /// Where it was found, as a request stores it.
    pub refs: Vec<RequestRefView>,
    /// Its page at the provider, `https` and on the provider's own site.
    pub link: Option<String>,
}

impl From<Candidate> for MetadataCandidateView {
    fn from(candidate: Candidate) -> Self {
        Self {
            provider: candidate.provider.to_owned(),
            kind: candidate.kind.as_str().to_owned(),
            title: candidate.title,
            details: candidate.details,
            refs: candidate
                .refs
                .into_iter()
                .map(|reference| RequestRefView {
                    provider: reference.provider.to_owned(),
                    external_id: reference.external_id,
                })
                .collect(),
            link: candidate.link,
        }
    }
}

/// How one source answered.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MetadataSourceView {
    /// `dat`, `musicbrainz`, `tmdb` or `igdb`.
    pub provider: String,
    /// `answered`, `failed`, `unconfigured` or `off`.
    pub state: String,
    /// How many candidates it answered.
    pub count: usize,
    /// Why it failed, as a stable code.
    pub error_code: Option<String>,
    /// Why, or what to do, in words.
    pub detail: Option<String>,
}

impl From<SourceOutcome> for MetadataSourceView {
    fn from(outcome: SourceOutcome) -> Self {
        Self {
            provider: outcome.provider.to_owned(),
            state: outcome.state.as_str().to_owned(),
            count: outcome.count,
            error_code: outcome.error_code.map(str::to_owned),
            detail: outcome.detail,
        }
    }
}

/// What a search found.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MetadataSearchView {
    /// The candidates: DATs' first, then each provider's in its order.
    pub candidates: Vec<MetadataCandidateView>,
    /// How each source asked answered.
    pub sources: Vec<MetadataSourceView>,
}

impl From<Found> for MetadataSearchView {
    fn from(found: Found) -> Self {
        Self {
            candidates: found.candidates.into_iter().map(Into::into).collect(),
            sources: found.sources.into_iter().map(Into::into).collect(),
        }
    }
}

fn rate_limited(wait: std::time::Duration) -> Response {
    let mut response = Problem::new(
        ErrorCode::RateLimited,
        "too many searches from this account for now; try again later",
    )
    .into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(&wait.as_secs().max(1).to_string()) {
        response
            .headers_mut()
            .insert(axum::http::header::RETRY_AFTER, value);
    }
    response
}

/// Search metadata for an item.
///
/// # Errors
///
/// `INVALID_PARAMETER` for an empty or too long query, or an unknown kind;
/// `RATE_LIMITED`.
#[utoipa::path(
    get,
    path = "/api/v1/metadata/search",
    tag = "requests",
    description = "Search metadata for an item to request: loaded DATs, and MusicBrainz, TMDB \
                   and IGDB where an administrator configured them, each where it suits the \
                   kind. An IMDb ID or address is looked up through TMDB alone. A source that \
                   fails is named, with why, and the others still answer. At most 300 an hour \
                   per account.",
    params(MetadataQuery),
    responses(
        (status = 200, description = "What was found, and how each source answered",
         body = MetadataSearchView),
        (status = 400, description = "No query, too long a one, or an unknown kind",
         body = Problem),
        (status = 429, description = "Too many from this account for now", body = Problem),
    ),
)]
pub async fn search_metadata(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Query(query): Query<MetadataQuery>,
) -> Result<Response, Problem> {
    let q = query.q.trim();
    if q.is_empty() || q.chars().any(char::is_control) {
        return Err(Problem::invalid_parameter(
            "q",
            "say what to search for, on one line",
        ));
    }
    if q.chars().count() > MAX_QUERY_CHARS {
        return Err(Problem::invalid_parameter(
            "q",
            &format!("at most {MAX_QUERY_CHARS} characters"),
        ));
    }
    let kind = match query.kind.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => Some(raw.parse::<RequestKind>().map_err(|_| {
            Problem::invalid_parameter("kind", "game, music, movie, television, software or other")
        })?),
    };
    if let Err(wait) = state.metadata_limiter().try_acquire(&user.username) {
        return Ok(rate_limited(wait));
    }
    let found = match state.metadata() {
        Some(metadata) => {
            metadata
                .search(state.database(), state.secret_key(), q, kind)
                .await
        }
        // A server that reaches no providers still searches its DATs.
        None => crate::metadata::search_dats_only(state.database(), q, kind).await,
    };
    Ok(Json(MetadataSearchView::from(found)).into_response())
}

/// The metadata routes.
pub fn router() -> Router<ApiState> {
    Router::new().route("/metadata/search", get(search_metadata))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::routes::requests::{
        CreateItemRequest, MAX_DETAIL_VALUE_CHARS, MAX_DETAILS, MAX_EXTERNAL_ID_CHARS,
        MAX_JSON_BYTES, MAX_TITLE_CHARS, check,
    };
    use tangible_integrations::metadata::{igdb, musicbrainz, tmdb};

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/../../fixtures/tool-output/metadata/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn a_candidate_is_bounded_as_a_request_is() {
        use tangible_integrations::metadata as bounds;
        assert_eq!(bounds::MAX_TITLE_CHARS, MAX_TITLE_CHARS);
        assert_eq!(bounds::MAX_DETAIL_CHARS, MAX_DETAIL_VALUE_CHARS);
        assert_eq!(bounds::MAX_EXTERNAL_ID_CHARS, MAX_EXTERNAL_ID_CHARS);
        assert_eq!(bounds::MAX_DETAILS, MAX_DETAILS);
        assert_eq!(bounds::MAX_DETAILS_BYTES, MAX_JSON_BYTES);
    }

    #[test]
    fn every_candidate_a_provider_answers_can_be_asked_for_as_it_is() {
        let mut candidates =
            musicbrainz::parse_releases(&fixture("musicbrainz-release-search.json")).unwrap();
        candidates.extend(tmdb::parse_movies(&fixture("tmdb-search-movie.json")).unwrap());
        candidates.extend(tmdb::parse_tv(&fixture("tmdb-search-tv.json")).unwrap());
        candidates.extend(tmdb::parse_find(&fixture("tmdb-find-imdb.json"), "tt0083658").unwrap());
        candidates.extend(igdb::parse_games(&fixture("igdb-games.json")).unwrap());
        assert!(candidates.len() > 10);
        for candidate in candidates {
            let view = MetadataCandidateView::from(candidate);
            let request = CreateItemRequest {
                kind: view.kind.clone(),
                title: view.title.clone(),
                details: view.details.clone(),
                refs: view.refs.clone(),
                note: None,
                attests_ownership: true,
            };
            assert!(check(&request).is_ok(), "{view:?}");
        }
    }
}
