// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Read routes over the artifact library.
//!
//! Backed by the manifest store rather than a database. That is deliberate for
//! this slice: it exercises the property the manifest exists for (that the
//! catalog can be rebuilt from storage alone), and it means the library is
//! readable before any repository code exists.
//!
//! A database-backed implementation will replace the listing when repositories
//! land; the response shapes here are the contract and will not change.

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tangible_domain::ArtifactId;
use tangible_domain::manifest::ArtifactManifest;
use utoipa::ToSchema;

use crate::pagination::{Page, PageQuery};
use crate::problem::{ErrorCode, Problem};
use crate::state::ApiState;

/// An artifact as it appears in a listing.
///
/// Deliberately not the whole manifest: a listing of a large library should
/// not carry every component of every artifact.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ArtifactSummary {
    /// Opaque identifier.
    pub id: String,
    /// Detected container format.
    pub format: String,
    /// Shape of the artifact.
    pub artifact_kind: String,
    /// Structural validation outcome.
    pub validation_state: String,
    /// Number of files.
    pub component_count: usize,
    /// Total size of all components.
    pub total_bytes: u64,
    /// Filename as received, when one was recorded.
    pub source_filename: Option<String>,
    /// Whether anything structural is worth an operator's attention.
    ///
    /// Surfaced in the summary so a damaged artifact is visible in a list
    /// without opening it.
    pub has_warnings: bool,
}

impl From<&ArtifactManifest> for ArtifactSummary {
    fn from(manifest: &ArtifactManifest) -> Self {
        Self {
            id: manifest.artifact_id.to_string(),
            format: manifest.classification.format.to_string(),
            artifact_kind: manifest.classification.artifact_kind.to_string(),
            validation_state: manifest.validation.state.to_string(),
            component_count: manifest.components.len(),
            total_bytes: manifest.classification.total_bytes,
            source_filename: manifest.origin.source_filename.clone(),
            has_warnings: !manifest.validation.validators.is_empty(),
        }
    }
}

/// A page of artifact summaries.
///
/// A concrete type rather than a generic instantiation so the published
/// OpenAPI names something a client generator can produce cleanly.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ArtifactSummaryPage {
    /// The artifacts in this page.
    pub items: Vec<ArtifactSummary>,
    /// Cursor for the next page, or null when this is the last.
    pub next_cursor: Option<String>,
}

impl From<Page<ArtifactSummary>> for ArtifactSummaryPage {
    fn from(page: Page<ArtifactSummary>) -> Self {
        Self {
            items: page.items,
            next_cursor: page.next_cursor,
        }
    }
}

/// One file within an artifact.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ComponentView {
    /// Portable path inside the artifact.
    pub logical_path: String,
    /// What part it plays.
    pub role: String,
    /// Position within the artifact.
    pub ordinal: u32,
    /// Size in bytes.
    pub length_bytes: u64,
    /// Content digest, as lowercase hex.
    pub sha256: String,
}

/// Everything known about one artifact, short of its full manifest.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ArtifactDetail {
    /// Summary fields.
    #[serde(flatten)]
    pub summary: ArtifactSummary,
    /// Detection confidence, from 0 to 1. Diagnostic only.
    pub format_confidence: f32,
    /// Where the bytes came from.
    pub source_kind: String,
    /// Structural findings, if any.
    pub warnings: Vec<String>,
    /// The files making up the artifact.
    pub components: Vec<ComponentView>,
}

fn component_view(component: &tangible_domain::manifest::Component) -> ComponentView {
    ComponentView {
        logical_path: component.logical_path.to_string(),
        role: component.role.to_string(),
        ordinal: component.ordinal,
        length_bytes: component.length_bytes,
        sha256: component.content.sha256.to_hex(),
    }
}

/// Read one manifest, mapping absence and corruption to distinct problems.
async fn load(state: &ApiState, id: ArtifactId) -> Result<ArtifactManifest, Problem> {
    use tangible_storage::ManifestStoreError;

    let Some(manifests) = state.manifests() else {
        return Err(Problem::storage_unavailable());
    };

    match manifests.read(id).await {
        Ok(manifest) => Ok(manifest),
        Err(ManifestStoreError::NotFound { .. }) => {
            Err(Problem::not_found("artifact", &id.to_string()))
        }
        Err(ManifestStoreError::Invalid(error)) => {
            // The request was fine; the stored data is not. Worth its own code
            // so an operator can tell a bad request from a bad library.
            tracing::error!(artifact_id = %id, error = ?error, "stored manifest is invalid");
            Err(Problem::new(
                ErrorCode::ManifestInvalid,
                format!("the manifest for artifact {id} failed validation"),
            ))
        }
        Err(error) => {
            tracing::error!(artifact_id = %id, error = ?error, "could not read manifest");
            Err(Problem::storage_unavailable())
        }
    }
}

/// List artifacts, newest identifiers last.
///
/// Ordering is by identifier, which for UUIDv7 is creation order.
///
/// # Errors
///
/// `INVALID_CURSOR` for a cursor this server did not issue, or
/// `STORAGE_UNAVAILABLE` if the library cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/artifacts",
    tag = "library",
    description = "List artifacts in creation order. Use `next_cursor` from the \
                   response to fetch the following page.",
    params(PageQuery),
    responses(
        (status = 200, description = "A page of artifacts", body = ArtifactSummaryPage),
        (status = 400, description = "Invalid cursor", body = Problem),
        (status = 503, description = "Library unreadable", body = Problem),
    ),
)]
pub async fn list_artifacts(
    State(state): State<ApiState>,
    Query(query): Query<PageQuery>,
) -> Result<Json<ArtifactSummaryPage>, Problem> {
    let Some(manifests) = state.manifests() else {
        return Err(Problem::storage_unavailable());
    };

    let after = query.after()?;
    let limit = query.limit();

    let mut ids = manifests.list().await.map_err(|error| {
        tracing::error!(error = ?error, "could not list manifests");
        Problem::storage_unavailable()
    })?;

    // Keyset: keep only identifiers strictly after the cursor. The listing is
    // already sorted, so this is a window rather than a scan-and-skip.
    if let Some(after) = after {
        ids.retain(|id| id.to_string() > after);
    }
    ids.truncate(limit);

    let mut items = Vec::with_capacity(ids.len());
    for id in ids {
        // One unreadable manifest must not fail the whole listing: a library
        // with a single corrupt file should still be browsable.
        match manifests.read(id).await {
            Ok(manifest) => items.push(ArtifactSummary::from(&manifest)),
            Err(error) => {
                tracing::warn!(artifact_id = %id, error = ?error, "skipping unreadable manifest");
            }
        }
    }

    Ok(Json(Page::new(items, limit, |item| item.id.clone()).into()))
}

/// One artifact in detail.
///
/// # Errors
///
/// `INVALID_PARAMETER` for a malformed identifier, `NOT_FOUND` if no such
/// artifact exists, or `MANIFEST_INVALID` if the stored manifest fails its own
/// validation.
#[utoipa::path(
    get,
    path = "/api/v1/artifacts/{artifact_id}",
    tag = "library",
    description = "Fetch one artifact with its components and any structural \
                   findings.",
    params(("artifact_id" = String, Path, description = "Artifact identifier")),
    responses(
        (status = 200, description = "The artifact", body = ArtifactDetail),
        (status = 404, description = "No such artifact", body = Problem),
        (status = 422, description = "Stored manifest is invalid", body = Problem),
    ),
)]
pub async fn get_artifact(
    State(state): State<ApiState>,
    Path(artifact_id): Path<String>,
) -> Result<Json<ArtifactDetail>, Problem> {
    let id = parse_id(&artifact_id)?;
    let manifest = load(&state, id).await?;

    let warnings = manifest
        .validation
        .validators
        .iter()
        .filter_map(|validator| {
            validator
                .details
                .get("message")
                .and_then(|value| value.as_str().map(ToOwned::to_owned))
        })
        .collect();

    Ok(Json(ArtifactDetail {
        summary: ArtifactSummary::from(&manifest),
        format_confidence: manifest.classification.format_confidence,
        source_kind: manifest.origin.source_kind.clone(),
        warnings,
        components: manifest.components.iter().map(component_view).collect(),
    }))
}

/// The artifact's full manifest, exactly as stored.
///
/// Returned verbatim so an external tool receives the same bytes the library
/// holds, rather than a re-serialization that might differ.
///
/// # Errors
///
/// `INVALID_PARAMETER`, `NOT_FOUND`, or `MANIFEST_INVALID`.
#[utoipa::path(
    get,
    path = "/api/v1/artifacts/{artifact_id}/manifest",
    tag = "library",
    description = "Fetch the artifact manifest exactly as stored, byte for \
                   byte.",
    params(("artifact_id" = String, Path, description = "Artifact identifier")),
    responses(
        (status = 200, description = "The manifest document"),
        (status = 404, description = "No such artifact", body = Problem),
    ),
)]
pub async fn get_manifest(
    State(state): State<ApiState>,
    Path(artifact_id): Path<String>,
) -> Result<impl IntoResponse, Problem> {
    let id = parse_id(&artifact_id)?;
    let manifest = load(&state, id).await?;
    let json = manifest.to_json().map_err(|error| {
        tracing::error!(error = ?error, "could not serialize manifest");
        Problem::new(ErrorCode::Internal, "the manifest could not be serialized")
    })?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        json,
    ))
}

/// The artifact's components.
///
/// # Errors
///
/// `INVALID_PARAMETER`, `NOT_FOUND`, or `MANIFEST_INVALID`.
#[utoipa::path(
    get,
    path = "/api/v1/artifacts/{artifact_id}/components",
    tag = "library",
    description = "List the files making up an artifact.",
    params(("artifact_id" = String, Path, description = "Artifact identifier")),
    responses(
        (status = 200, description = "The components", body = Vec<ComponentView>),
        (status = 404, description = "No such artifact", body = Problem),
    ),
)]
pub async fn list_components(
    State(state): State<ApiState>,
    Path(artifact_id): Path<String>,
) -> Result<Json<Vec<ComponentView>>, Problem> {
    let id = parse_id(&artifact_id)?;
    let manifest = load(&state, id).await?;
    Ok(Json(
        manifest.components.iter().map(component_view).collect(),
    ))
}

/// Parse a path identifier, rejecting anything malformed with a clear problem.
fn parse_id(raw: &str) -> Result<ArtifactId, Problem> {
    raw.parse::<ArtifactId>()
        .map_err(|_| Problem::invalid_parameter("artifact_id", "not a valid identifier"))
}

/// The library routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/artifacts", get(list_artifacts))
        .route("/artifacts/{artifact_id}", get(get_artifact))
        .route("/artifacts/{artifact_id}/manifest", get(get_manifest))
        .route("/artifacts/{artifact_id}/components", get(list_components))
}
