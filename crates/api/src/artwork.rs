// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Cover art (ADR 0023): which cover an item wants, the runner that fetches
//! covers, and the route that serves them.
//!
//! A cover is fetched once, by this runner, through the metadata providers
//! a request was found with, checked to be a JPEG, PNG or WebP, and kept in
//! PostgreSQL. Pages ask Tangible for it, never a provider: the web UI's
//! policy loads images from Tangible alone.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tangible_db::artwork::{
    DueArtwork, Picked, ReadyArtwork, artwork_image, artwork_not_fetched, artwork_ready,
    due_artwork, pick_artwork, ready_artwork, requests_references, unpick_artwork, want_artwork,
};
use tangible_integrations::artwork::{ArtworkError, fetch_image};
use tokio::sync::Notify;
use utoipa::ToSchema;

use crate::auth::SignedIn;
use crate::metadata::CoverLookup;
use crate::problem::{ErrorCode, Problem};
use crate::state::ApiState;

/// The sources a request's references are taken from for its cover, best
/// first: a TMDB title names its poster exactly; an IMDb id is looked up in
/// TMDB; then a MusicBrainz release, then an IGDB game.
const PREFERRED: [&str; 4] = ["tmdb", "imdb", "musicbrainz", "igdb"];

/// The longest external id kept, as the table allows.
const MAX_EXTERNAL_ID: usize = 500;

/// How long the runner waits between passes when nothing wakes it.
const INTERVAL: Duration = Duration::from_mins(10);

/// How many covers one pass fetches, so a large backlog is fetched a little
/// at a time and the providers are not hurried.
const BATCH: i64 = 20;

/// A cover a provider has not got is asked for again after this.
const MISSING_AGAIN: time::Duration = time::Duration::days(30);

/// A provider not configured, or off, is tried again after this.
const UNAVAILABLE_AGAIN: time::Duration = time::Duration::hours(1);

/// The cover a request's references want: the first of [`PREFERRED`] it
/// has, as (source, external id).
#[must_use]
pub fn preferred(refs: &serde_json::Value) -> Option<(String, String)> {
    let pairs: Vec<(&str, &str)> = refs
        .as_array()?
        .iter()
        .filter_map(|reference| {
            Some((
                reference.get("provider")?.as_str()?,
                reference.get("external_id")?.as_str()?,
            ))
        })
        .collect();
    preferred_of(&pairs)
}

/// The same, from (provider, external id) pairs.
#[must_use]
pub fn preferred_of(refs: &[(&str, &str)]) -> Option<(String, String)> {
    PREFERRED.iter().find_map(|source| {
        refs.iter().find_map(|&(provider, external_id)| {
            let external_id = external_id.trim();
            (provider == *source && !external_id.is_empty() && external_id.len() <= MAX_EXTERNAL_ID)
                .then(|| ((*source).to_owned(), external_id.to_owned()))
        })
    })
}

/// Fill in each request's cover, where its best reference's is ready.
pub async fn with_request_covers(
    state: &ApiState,
    views: &mut [crate::routes::requests::RequestView],
) {
    let wanted: Vec<Option<(String, String)>> = views
        .iter()
        .map(|view| {
            let pairs: Vec<(&str, &str)> = view
                .refs
                .iter()
                .map(|reference| (reference.provider.as_str(), reference.external_id.as_str()))
                .collect();
            preferred_of(&pairs)
        })
        .collect();
    let keys: Vec<(String, String)> = wanted.iter().flatten().cloned().collect();
    let covers = covers_for(state, &keys).await;
    for (view, key) in views.iter_mut().zip(wanted) {
        view.cover = key.and_then(|key| covers.get(&key).cloned());
    }
}

/// A cover, as a page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct CoverView {
    /// Where Tangible serves it: `/api/v1/artwork/{id}?v=` and the start of
    /// its digest, so a cover fetched again is a new address.
    pub url: String,
    /// The provider it came from: `tmdb`, `imdb`, `musicbrainz`, `igdb` or
    /// `libretro`.
    pub source: String,
}

impl From<&ReadyArtwork> for CoverView {
    fn from(ready: &ReadyArtwork) -> Self {
        Self {
            url: format!(
                "/api/v1/artwork/{}?v={}",
                ready.id,
                ready.sha256.get(..12).unwrap_or(&ready.sha256)
            ),
            source: ready.source.clone(),
        }
    }
}

/// The ready covers for `keys`, by key. A database that cannot answer
/// leaves them out: a page without covers is still a page.
pub async fn covers_for(
    state: &ApiState,
    keys: &[(String, String)],
) -> HashMap<(String, String), CoverView> {
    match ready_artwork(state.database().pool(), keys).await {
        Ok(found) => found
            .iter()
            .map(|(key, ready)| (key.clone(), CoverView::from(ready)))
            .collect(),
        Err(error) => {
            tracing::warn!(error = ?error, "covers could not be read");
            HashMap::new()
        }
    }
}

/// How long to wait before trying a cover again after `attempts` failures:
/// five minutes, doubling, at most a day.
fn backoff(attempts: i32) -> time::Duration {
    let doublings = u32::try_from(attempts.clamp(0, 9)).unwrap_or(0);
    let minutes = 5_i64.saturating_mul(1_i64 << doublings);
    time::Duration::minutes(minutes.min(24 * 60))
}

/// What one pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArtworkPass {
    /// Covers newly wanted.
    pub wanted: u64,
    /// Covers fetched and kept.
    pub fetched: u64,
    /// Covers the provider has not got.
    pub missing: u64,
    /// Covers that could not be fetched now.
    pub failed: u64,
}

/// Fetches the covers wanted, a batch at a time.
#[derive(Clone)]
pub struct ArtworkRunner {
    state: ApiState,
    nudge: Arc<Notify>,
}

impl ArtworkRunner {
    /// A runner over `state`, which holds the database, the metadata
    /// providers and the key their secrets are sealed with, and wakes on
    /// the state's nudge as well as its own clock.
    #[must_use]
    pub fn new(state: ApiState) -> Self {
        let nudge = state.artwork_nudge();
        Self { state, nudge }
    }

    /// Ask the runner to look now.
    pub fn nudge(&self) {
        self.nudge.notify_one();
    }

    /// Run until `shutdown` resolves.
    pub async fn run<S>(self, shutdown: S)
    where
        S: Future<Output = ()> + Send,
    {
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            match self.pass().await {
                Ok(pass) if pass != ArtworkPass::default() => tracing::info!(
                    wanted = pass.wanted,
                    fetched = pass.fetched,
                    missing = pass.missing,
                    failed = pass.failed,
                    "artwork pass"
                ),
                Ok(_) => {}
                Err(error) => tracing::error!(error = ?error, "an artwork pass failed"),
            }
            tokio::select! {
                () = &mut shutdown => return,
                () = self.nudge.notified() => {}
                () = tokio::time::sleep(INTERVAL) => {}
            }
        }
    }

    /// One pass: want the covers requests name, then fetch a batch of those
    /// due.
    ///
    /// # Errors
    ///
    /// A database failure.
    pub async fn pass(&self) -> Result<ArtworkPass, tangible_db::DbError> {
        let pool = self.state.database().pool();
        let mut pass = ArtworkPass::default();
        let wanted: Vec<(String, String)> = requests_references(pool)
            .await?
            .iter()
            .filter_map(preferred)
            .collect();
        pass.wanted = want_artwork(pool, &wanted).await?;
        let Some(metadata) = self.state.metadata() else {
            return Ok(pass);
        };
        for due in due_artwork(pool, BATCH).await? {
            self.fetch(metadata, &due, &mut pass).await?;
        }
        Ok(pass)
    }

    async fn fetch(
        &self,
        metadata: &crate::metadata::Metadata,
        due: &DueArtwork,
        pass: &mut ArtworkPass,
    ) -> Result<(), tangible_db::DbError> {
        let pool = self.state.database().pool();
        let url = match metadata
            .cover_url(
                self.state.database(),
                self.state.secret_key(),
                &due.source,
                &due.external_id,
            )
            .await
        {
            Ok(Some(url)) => url,
            Ok(None) => {
                pass.missing += 1;
                return artwork_not_fetched(
                    pool,
                    due.id,
                    true,
                    "the provider has no cover for it",
                    MISSING_AGAIN,
                )
                .await;
            }
            Err(CoverLookup::Unavailable(why)) => {
                pass.failed += 1;
                return artwork_not_fetched(pool, due.id, false, &why, UNAVAILABLE_AGAIN).await;
            }
            Err(CoverLookup::Failed(why)) => {
                pass.failed += 1;
                return artwork_not_fetched(pool, due.id, false, &why, backoff(due.attempts)).await;
            }
        };
        match fetch_image(metadata.http(), &url).await {
            Ok(image) => {
                let sha256 = hex::encode(Sha256::digest(&image.bytes));
                artwork_ready(
                    pool,
                    due.id,
                    image.image_type.media_type(),
                    &image.bytes,
                    &sha256,
                )
                .await?;
                pass.fetched += 1;
            }
            Err(ArtworkError::Missing) => {
                pass.missing += 1;
                artwork_not_fetched(
                    pool,
                    due.id,
                    true,
                    "the provider has no cover for it",
                    MISSING_AGAIN,
                )
                .await?;
            }
            Err(ArtworkError::Failed(why)) => {
                pass.failed += 1;
                tracing::info!(source = %due.source, %why, "a cover could not be fetched");
                artwork_not_fetched(pool, due.id, false, &why, backoff(due.attempts)).await?;
            }
        }
        Ok(())
    }
}

/// A cover, as fetched and checked.
///
/// # Errors
///
/// `NOT_FOUND` for no such cover, or one not fetched.
#[utoipa::path(
    get,
    path = "/api/v1/artwork/{artwork_id}",
    tag = "library",
    description = "A cover, as Tangible fetched it from a provider and checked it to be a \\
                   JPEG, PNG or WebP (ADR 0023). Served as its checked type and never sniffed; \\
                   its address changes when it is fetched again, so it is kept as long as a \\
                   browser likes. Anyone signed in.",
    params(("artwork_id" = String, Path, description = "Artwork identifier")),
    responses(
        (status = 200, description = "The image", content_type = "image/jpeg"),
        (status = 404, description = "No such cover", body = Problem),
    ),
)]
pub async fn get_artwork(
    State(state): State<ApiState>,
    Path(artwork_id): Path<String>,
) -> Result<Response, Problem> {
    let id = uuid::Uuid::parse_str(&artwork_id)
        .map_err(|_| Problem::invalid_parameter("artwork_id", "not a valid identifier"))?;
    let image = artwork_image(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "a cover could not be read");
            Problem::new(ErrorCode::StorageUnavailable, "the cover could not be read")
        })?
        .ok_or_else(|| Problem::not_found("artwork", &artwork_id))?;
    // Only the three types the table allows are ever served.
    let content_type = match image.content_type.as_str() {
        "image/jpeg" | "image/png" | "image/webp" => image.content_type.clone(),
        _ => return Err(Problem::not_found("artwork", &artwork_id)),
    };
    let mut response = (StatusCode::OK, image.bytes).into_response();
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&content_type) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if let Ok(value) = HeaderValue::from_str(&format!("\"{}\"", image.sha256)) {
        headers.insert(header::ETAG, value);
    }
    Ok(response)
}

/// One reference to an item at a provider, as a metadata search's candidate
/// carries it.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtworkRef {
    /// `tmdb`, `imdb`, `musicbrainz`, `igdb`, or another the search named.
    pub provider: String,
    /// The provider's id for the item.
    pub external_id: String,
}

/// A cover to pick for an artifact: a metadata search candidate's
/// references, from which Tangible takes the one it fetches covers by.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PickArtworkRequest {
    /// The candidate's references, at most 16.
    pub refs: Vec<ArtworkRef>,
}

/// The cover picked: the reference it is fetched by.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PickedArtworkView {
    /// `tmdb`, `imdb`, `musicbrainz` or `igdb`.
    pub source: String,
    /// The provider's id for the item.
    pub external_id: String,
}

fn artifact_id(raw: &str) -> Result<uuid::Uuid, Problem> {
    uuid::Uuid::parse_str(raw)
        .map_err(|_| Problem::invalid_parameter("artifact_id", "not a valid identifier"))
}

/// Pick an artifact's cover from a metadata search candidate.
///
/// # Errors
///
/// `NOT_FOUND`; `VALIDATION_FAILED` for references none of which has a
/// cover Tangible fetches.
#[utoipa::path(
    put,
    path = "/api/v1/artifacts/{artifact_id}/artwork",
    tag = "library",
    description = "Pick an artifact's cover from a metadata search candidate (ADR 0023): \
                   Tangible takes the candidate's best reference for a cover (TMDB, then \
                   IMDb, then MusicBrainz, then IGDB), fetches it in the background, and shows \
                   it before the cover of the request the artifact fulfilled. Picking again \
                   replaces it. Needs the catalog permission; audited.",
    params(("artifact_id" = String, Path, description = "Artifact identifier")),
    request_body = PickArtworkRequest,
    responses(
        (status = 202, description = "Picked; fetched shortly", body = PickedArtworkView),
        (status = 404, description = "No such artifact", body = Problem),
        (status = 422, description = "No reference Tangible fetches covers by", body = Problem),
    ),
)]
pub async fn pick_artifact_artwork(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(raw): Path<String>,
    Json(request): Json<PickArtworkRequest>,
) -> Result<(StatusCode, Json<PickedArtworkView>), Problem> {
    let id = artifact_id(&raw)?;
    if request.refs.len() > 16 {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            "at most 16 references",
        ));
    }
    let pairs: Vec<(&str, &str)> = request
        .refs
        .iter()
        .map(|reference| (reference.provider.as_str(), reference.external_id.as_str()))
        .collect();
    let (source, external_id) = preferred_of(&pairs).ok_or_else(|| {
        Problem::new(
            ErrorCode::ValidationFailed,
            "none of these has a cover Tangible fetches: it needs a TMDB, IMDb, MusicBrainz or \
             IGDB reference",
        )
    })?;
    match pick_artwork(
        state.database().pool(),
        id,
        &source,
        &external_id,
        &user.username,
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "a cover could not be picked");
        Problem::storage_unavailable()
    })? {
        Picked::Done => {}
        Picked::NoArtifact => return Err(Problem::not_found("artifact", &raw)),
    }
    state.artwork_nudge().notify_one();
    Ok((
        StatusCode::ACCEPTED,
        Json(PickedArtworkView {
            source,
            external_id,
        }),
    ))
}

/// Remove the cover picked for an artifact.
///
/// # Errors
///
/// `INVALID_PARAMETER` for a malformed identifier.
#[utoipa::path(
    delete,
    path = "/api/v1/artifacts/{artifact_id}/artwork",
    tag = "library",
    description = "Remove the cover picked for an artifact: it shows its request's cover \
                   again, or none. Answers 204 whether or not one was picked. Needs the \
                   catalog permission; audited.",
    params(("artifact_id" = String, Path, description = "Artifact identifier")),
    responses((status = 204, description = "No cover picked")),
)]
pub async fn unpick_artifact_artwork(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(raw): Path<String>,
) -> Result<StatusCode, Problem> {
    let id = artifact_id(&raw)?;
    unpick_artwork(state.database().pool(), id, &user.username)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "a cover could not be removed");
            Problem::storage_unavailable()
        })?;
    Ok(StatusCode::NO_CONTENT)
}

/// The artwork routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/artwork/{artwork_id}", get(get_artwork))
        .route(
            "/artifacts/{artifact_id}/artwork",
            put(pick_artifact_artwork).delete(unpick_artifact_artwork),
        )
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_request_wants_its_best_reference_as_its_cover() {
        let refs = serde_json::json!([
            { "provider": "igdb", "external_id": "1020" },
            { "provider": "imdb", "external_id": "tt0113243" },
            { "provider": "tmdb", "external_id": "movie/10428" },
        ]);
        assert_eq!(
            preferred(&refs),
            Some(("tmdb".to_owned(), "movie/10428".to_owned()))
        );
        let imdb_only = serde_json::json!([{ "provider": "imdb", "external_id": "tt0113243" }]);
        assert_eq!(
            preferred(&imdb_only),
            Some(("imdb".to_owned(), "tt0113243".to_owned()))
        );
        // Nothing it can use: a DAT's name, an empty id, not a list.
        for none in [
            serde_json::json!([{ "provider": "dat", "external_id": "Game (USA)" }]),
            serde_json::json!([{ "provider": "tmdb", "external_id": " " }]),
            serde_json::json!({ "provider": "tmdb" }),
            serde_json::json!([]),
        ] {
            assert_eq!(preferred(&none), None, "{none}");
        }
    }

    #[test]
    fn a_cover_is_served_at_an_address_naming_its_bytes() {
        let ready = ReadyArtwork {
            id: uuid::Uuid::nil(),
            source: "tmdb".to_owned(),
            sha256: "ab".repeat(32),
        };
        assert_eq!(
            CoverView::from(&ready),
            CoverView {
                url: "/api/v1/artwork/00000000-0000-0000-0000-000000000000?v=abababababab"
                    .to_owned(),
                source: "tmdb".to_owned(),
            }
        );
    }

    #[test]
    fn a_failure_is_tried_again_later_each_time_up_to_a_day() {
        assert_eq!(backoff(0), time::Duration::minutes(5));
        assert_eq!(backoff(1), time::Duration::minutes(10));
        assert_eq!(backoff(4), time::Duration::minutes(80));
        assert_eq!(backoff(9), time::Duration::hours(24));
        assert_eq!(backoff(40), time::Duration::hours(24));
        assert_eq!(backoff(-1), time::Duration::minutes(5));
    }
}
