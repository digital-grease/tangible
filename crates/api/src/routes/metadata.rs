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

use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tangible_domain::RequestKind;
use tangible_integrations::metadata::{Candidate, MAX_QUERY_CHARS};
use utoipa::{IntoParams, ToSchema};

use tangible_domain::auth::Permission;

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
    /// Where Tangible serves its thumbnail (ADR 0027): fetched from the
    /// provider when it is first asked for, for an hour after the search.
    /// Null when it has no picture, or this server fetches none
    /// (`TANGIBLE_ARTWORK=false`). The provider may still have none, and
    /// the address then answers 404.
    pub thumbnail: Option<String>,
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
            thumbnail: None,
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
    let thumbnails: Vec<Option<String>> = match state.thumbnails() {
        Some(thumbnails) => thumbnails.hold(
            found
                .candidates
                .iter()
                .map(|candidate| candidate.picture.as_ref()),
        ),
        None => Vec::new(),
    };
    let mut view = MetadataSearchView::from(found);
    for (candidate, id) in view.candidates.iter_mut().zip(thumbnails) {
        candidate.thumbnail = id.map(|id| format!("/api/v1/metadata/thumbnails/{id}"));
    }
    if !user.role.allows(Permission::ManageIntegrations) {
        hide_resolved_addresses(&mut view.sources);
    }
    Ok(Json(view).into_response())
}

/// A provider's address refused names what its name resolved to, such as
/// an address on the server's own network: for administrators, who set the
/// providers up and see it on Test. Anyone else is told it was refused.
fn hide_resolved_addresses(sources: &mut [MetadataSourceView]) {
    for source in sources {
        if source.error_code.as_deref() == Some("INTEGRATION_ADDRESS_REFUSED") {
            source.detail = Some(
                "the provider's address was refused; an administrator can see why with Test \
                 on the Integrations page"
                    .to_owned(),
            );
        }
    }
}

/// A search result's thumbnail.
///
/// # Errors
///
/// `INVALID_PARAMETER` for an identifier that is not one; `NOT_FOUND`.
#[utoipa::path(
    get,
    path = "/api/v1/metadata/thumbnails/{thumbnail_id}",
    tag = "requests",
    description = "The thumbnail of a metadata search's result, at the address the search \
                   answered (ADR 0027). Tangible fetches it from the provider's image service \
                   the first time it is asked for, keeps it only if it is a JPEG, PNG or WebP, \
                   and holds it in memory for an hour after the last search that named it. \
                   Served as its checked type and never sniffed. Only a picture a search named \
                   can be fetched. Anybody who may search.",
    params(("thumbnail_id" = String, Path,
            description = "The thumbnail's identifier, from a search's answer")),
    responses(
        (status = 200, description = "The image", content_type = "image/jpeg"),
        (status = 400, description = "Not a thumbnail's identifier", body = Problem),
        (status = 404, description = "None now: not named by a search in the last hour, \
                                      none at the provider, or not fetched", body = Problem),
    ),
)]
pub async fn get_thumbnail(
    State(state): State<ApiState>,
    Path(thumbnail_id): Path<String>,
) -> Result<Response, Problem> {
    if thumbnail_id.len() != 32 || !thumbnail_id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Problem::invalid_parameter(
            "thumbnail_id",
            "not a thumbnail's identifier",
        ));
    }
    let not_found = || Problem::not_found("thumbnail", &thumbnail_id);
    let thumbnails = state.thumbnails().ok_or_else(not_found)?;
    let image = thumbnails
        .image(&thumbnail_id)
        .await
        .ok_or_else(not_found)?;
    let mut response = (StatusCode::OK, image.bytes.clone()).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(image.image_type.media_type()),
    );
    // For as long as it is held, and only by this browser.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=3600"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

/// The metadata routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/metadata/search", get(search_metadata))
        .route("/metadata/thumbnails/{thumbnail_id}", get(get_thumbnail))
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

    #[test]
    fn an_address_refused_is_told_without_what_it_resolved_to() {
        let source = |code: Option<&str>, detail: &str| MetadataSourceView {
            provider: "tmdb".into(),
            state: "failed".into(),
            count: 0,
            error_code: code.map(str::to_owned),
            detail: Some(detail.to_owned()),
        };
        let mut sources = [
            source(
                Some("INTEGRATION_ADDRESS_REFUSED"),
                "api.themoviedb.org has no address (10.0.0.5 is private network)",
            ),
            source(Some("INTEGRATION_RATE_LIMITED"), "TMDB asked to wait"),
        ];
        hide_resolved_addresses(&mut sources);
        let hidden = sources[0].detail.as_deref().unwrap();
        assert!(!hidden.contains("10.0.0.5"), "{hidden}");
        assert!(hidden.contains("refused"), "{hidden}");
        assert_eq!(sources[1].detail.as_deref(), Some("TMDB asked to wait"));
    }

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
