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
    /// Opaque identifier, for downloading it.
    pub id: String,
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
    /// The disc's tracks, when the artifact was described by a CUE sheet or a
    /// TOC whose tracks could be laid out.
    pub disc: Option<DiscLayout>,
}

/// A disc of tracks.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DiscLayout {
    /// Media catalogue number, when the disc carries one.
    pub catalog: Option<String>,
    /// Sessions the tracks span.
    pub session_count: u32,
    /// Tracks in disc order.
    pub tracks: Vec<TrackView>,
}

/// One track.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TrackView {
    /// Track number.
    pub number: u32,
    /// Session.
    pub session: u32,
    /// Mode, as a CUE sheet spells it: `AUDIO`, `MODE1/2048`, `MODE2/2352`.
    pub mode: String,
    /// Whether it is audio.
    pub is_audio: bool,
    /// The component its bytes are in.
    pub component_id: String,
    /// That component's path.
    pub component_path: String,
    /// Where in that component its first sector is.
    pub file_offset_bytes: u64,
    /// Its first present sector, counted from the start of the image.
    pub start_lba: u64,
    /// Sectors present.
    pub sector_count: u64,
    /// Sectors of gap before index 1, generated and in the file together.
    pub pregap_sectors: u64,
    /// Index points, relative to the first present sector.
    pub indexes: Vec<TrackIndexView>,
    /// International Standard Recording Code.
    pub isrc: Option<String>,
    /// Flags: `DCP`, `4CH`, `PRE`, `SCMS`.
    pub flags: Vec<String>,
    /// How an audio track's samples are stored: `little_endian` or
    /// `big_endian`.
    pub sample_byte_order: Option<String>,
}

/// An index point.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TrackIndexView {
    /// Index number; 0 is the pregap, 1 the track proper.
    pub number: u32,
    /// Sectors from the track's first present sector.
    pub relative_lba: u64,
}

fn disc_view(manifest: &ArtifactManifest) -> Option<DiscLayout> {
    let tangible_domain::manifest::Topology::CdTracks {
        catalog,
        session_count,
        tracks,
        ..
    } = &manifest.topology
    else {
        return None;
    };
    let path_of = |id| {
        manifest
            .components
            .iter()
            .find(|component| component.id == id)
            .map_or_else(String::new, |component| component.logical_path.to_string())
    };
    Some(DiscLayout {
        catalog: catalog.clone(),
        session_count: *session_count,
        tracks: tracks
            .iter()
            .map(|track| TrackView {
                number: track.number,
                session: track.session,
                mode: track.mode.clone(),
                is_audio: tangible_domain::cd::is_audio(&track.mode),
                component_id: track.component_id.to_string(),
                component_path: path_of(track.component_id),
                file_offset_bytes: track.file_offset_bytes,
                start_lba: track.start_lba,
                sector_count: track.sector_count,
                pregap_sectors: track.pregap_sectors,
                indexes: track
                    .indexes
                    .iter()
                    .map(|index| TrackIndexView {
                        number: index.number,
                        relative_lba: index.relative_lba,
                    })
                    .collect(),
                isrc: track.isrc.clone(),
                flags: track
                    .flags
                    .iter()
                    .map(|flag| flag.as_str().to_owned())
                    .collect(),
                sample_byte_order: track.sample_byte_order.map(|order| {
                    match order {
                        tangible_domain::cd::SampleByteOrder::LittleEndian => "little_endian",
                        tangible_domain::cd::SampleByteOrder::BigEndian => "big_endian",
                    }
                    .to_owned()
                }),
            })
            .collect(),
    })
}

fn component_view(component: &tangible_domain::manifest::Component) -> ComponentView {
    ComponentView {
        id: component.id.to_string(),
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
        disc: disc_view(&manifest),
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

// --- component bytes ------------------------------------------------------------

/// What a `Range` header asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Requested {
    /// Send the whole object.
    ///
    /// Also what an unparseable or multi-part range gets. RFC 9110 permits
    /// ignoring a range a server does not wish to honour, and a client that
    /// asked for something this does not implement is better served the whole
    /// object than refused.
    Whole,
    /// Send this window.
    Range {
        /// First byte, inclusive.
        start: u64,
        /// How many bytes.
        length: u64,
    },
    /// The range lies outside the object.
    Unsatisfiable,
}

/// Interpret a `Range` header against a known object length.
///
/// Byte ranges are how a worker resumes staging a partly-downloaded image, so
/// this has to be exact: an off-by-one here corrupts a burn rather than
/// producing a visibly wrong page.
fn requested_range(header: Option<&str>, total: u64) -> Requested {
    let Some(header) = header else {
        return Requested::Whole;
    };
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        return Requested::Whole;
    };
    // One range only. Multipart responses are not implemented, and answering
    // a multi-range request with just the first part would be wrong.
    if spec.contains(',') {
        return Requested::Whole;
    }
    let spec = spec.trim();
    let Some((first, last)) = spec.split_once('-') else {
        return Requested::Whole;
    };

    // A suffix range: the last N bytes.
    if first.is_empty() {
        let Ok(suffix) = last.parse::<u64>() else {
            return Requested::Whole;
        };
        if suffix == 0 || total == 0 {
            return Requested::Unsatisfiable;
        }
        let length = suffix.min(total);
        return Requested::Range {
            start: total - length,
            length,
        };
    }

    let Ok(start) = first.parse::<u64>() else {
        return Requested::Whole;
    };
    if start >= total {
        // Includes an empty object, for which every range is unsatisfiable.
        return Requested::Unsatisfiable;
    }

    let end = if last.is_empty() {
        total - 1
    } else {
        match last.parse::<u64>() {
            // Clamped to the object rather than refused: asking for more than
            // exists is how a client says "the rest".
            Ok(end) => end.min(total - 1),
            Err(_) => return Requested::Whole,
        }
    };
    if end < start {
        return Requested::Whole;
    }

    Requested::Range {
        start,
        length: end - start + 1,
    }
}

/// Stream one component's bytes.
///
/// The route a burn worker stages from, which is why it streams rather than
/// reading the object into memory: a Blu-ray image is tens of gigabytes and
/// the server holds one buffer at a time either way.
///
/// The bytes are not re-verified here. The manifest carries every component's
/// digest, and the worker hashes as it writes, so the check happens where a
/// mismatch can still stop a burn rather than where it would only slow a
/// download.
///
/// # Errors
///
/// `INVALID_PARAMETER` for a malformed identifier, `NOT_FOUND` if no such
/// artifact or component exists, or `STORAGE_UNAVAILABLE` if the object is
/// missing from the store the manifest says holds it.
#[utoipa::path(
    get,
    path = "/api/v1/artifacts/{artifact_id}/components/{component_id}/content",
    tag = "library",
    description = "Stream one component's bytes. Supports a single byte range, \
                   which is how a worker resumes an interrupted download.",
    params(
        ("artifact_id" = String, Path, description = "Artifact identifier"),
        ("component_id" = String, Path, description = "Component identifier"),
        ("Range" = Option<String>, Header, description = "Single byte range, e.g. bytes=0-1023"),
    ),
    responses(
        (status = 200, description = "The whole component"),
        (status = 206, description = "The requested range"),
        (status = 404, description = "No such artifact or component", body = Problem),
        (status = 416, description = "The range lies outside the object", body = Problem),
    ),
)]
pub async fn component_content(
    State(state): State<ApiState>,
    Path((artifact_id, component_id)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Result<axum::response::Response, Problem> {
    use axum::http::{StatusCode, header};

    let id = parse_id(&artifact_id)?;
    let component_id = component_id
        .parse::<tangible_domain::ComponentId>()
        .map_err(|_| Problem::invalid_parameter("component_id", "not a valid identifier"))?;

    let manifest = load(&state, id).await?;
    let component = manifest
        .components
        .iter()
        .find(|component| component.id == component_id)
        .ok_or_else(|| Problem::not_found("component", &component_id.to_string()))?;

    let Some(objects) = state
        .manifests()
        .map(tangible_storage::ManifestStore::objects)
    else {
        return Err(Problem::storage_unavailable());
    };

    let object = checked_object(objects, component, id).await?;

    let total = object.size_bytes;
    let range = requested_range(
        headers
            .get(header::RANGE)
            .and_then(|value| value.to_str().ok()),
        total,
    );

    let (status, start, length) = match range {
        Requested::Whole => (StatusCode::OK, 0, total),
        Requested::Range { start, length } => (StatusCode::PARTIAL_CONTENT, start, length),
        Requested::Unsatisfiable => {
            let mut response = Problem::new(
                ErrorCode::InvalidParameter,
                "the requested range lies outside this component",
            )
            .into_response();
            *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            response.headers_mut().insert(
                header::CONTENT_RANGE,
                axum::http::HeaderValue::from_str(&format!("bytes */{total}"))
                    .unwrap_or(axum::http::HeaderValue::from_static("bytes */0")),
            );
            return Ok(response);
        }
    };

    let reader = objects
        .open_range(&component.content.sha256, start, length)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not open a component object");
            Problem::storage_unavailable()
        })?;

    let media_type = component
        .media_type
        .clone()
        .unwrap_or_else(|| "application/octet-stream".to_owned());

    let mut response = axum::response::Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, media_type)
        // Always a download, never rendered: these are bytes somebody
        // imported, and a browser shown one inline would be interpreting
        // them on this origin.
        .header(
            header::CONTENT_DISPOSITION,
            attachment(component.logical_path.file_name()),
        )
        .header(header::CONTENT_LENGTH, length)
        .header(header::ACCEPT_RANGES, "bytes")
        .body(axum::body::Body::from_stream(
            tokio_util::io::ReaderStream::new(reader),
        ))
        .map_err(|error| {
            tracing::error!(error = ?error, "could not build a content response");
            Problem::new(ErrorCode::Internal, "the component could not be served")
        })?;

    if status == StatusCode::PARTIAL_CONTENT {
        let end = start + length - 1;
        if let Ok(value) =
            axum::http::HeaderValue::from_str(&format!("bytes {start}-{end}/{total}"))
        {
            response.headers_mut().insert(header::CONTENT_RANGE, value);
        }
    }

    Ok(response)
}

/// The stored object behind a component, if it is still the one the manifest
/// describes.
async fn checked_object(
    objects: &tangible_storage::FilesystemStore,
    component: &tangible_domain::manifest::Component,
    id: ArtifactId,
) -> Result<tangible_storage::ObjectStat, Problem> {
    // The manifest's length is the contract, but the object on disk is what
    // will be sent. Disagreement means the store has been altered underneath
    // the catalog, which is worth refusing rather than serving.
    let object = objects
        .stat(&component.content.sha256)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not stat a component object");
            Problem::storage_unavailable()
        })?
        .ok_or_else(|| {
            tracing::error!(
                artifact_id = %id,
                "a manifest references an object the store does not hold"
            );
            Problem::storage_unavailable()
        })?;
    if object.size_bytes != component.length_bytes {
        tracing::error!(
            artifact_id = %id,
            expected = component.length_bytes,
            found = object.size_bytes,
            "a stored object no longer matches the length its manifest records"
        );
        return Err(Problem::storage_unavailable());
    }

    Ok(object)
}

/// A `Content-Disposition` value that downloads under `name`.
///
/// The plain `filename` is ASCII with anything awkward replaced, for clients
/// that read only that; `filename*` carries the real name, percent-encoded,
/// for those that read RFC 6266. Neither can carry a quote, a separator or a
/// control character into the header.
fn attachment(name: &str) -> String {
    use std::fmt::Write as _;

    let plain: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ' | '(' | ')') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut encoded = String::new();
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    format!("attachment; filename=\"{plain}\"; filename*=UTF-8''{encoded}")
}

/// The library routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/artifacts", get(list_artifacts))
        .route("/artifacts/{artifact_id}", get(get_artifact))
        .route("/artifacts/{artifact_id}/manifest", get(get_manifest))
        .route("/artifacts/{artifact_id}/components", get(list_components))
        .route(
            "/artifacts/{artifact_id}/components/{component_id}/content",
            get(component_content),
        )
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn no_range_header_means_the_whole_object() {
        assert_eq!(requested_range(None, 100), Requested::Whole);
    }

    #[test]
    fn a_closed_range_is_inclusive_at_both_ends() {
        // The off-by-one that would corrupt a staged image. bytes=0-0 is one
        // byte, not zero and not two.
        assert_eq!(
            requested_range(Some("bytes=0-0"), 100),
            Requested::Range {
                start: 0,
                length: 1
            }
        );
        assert_eq!(
            requested_range(Some("bytes=10-19"), 100),
            Requested::Range {
                start: 10,
                length: 10
            }
        );
    }

    #[test]
    fn an_open_range_runs_to_the_end() {
        assert_eq!(
            requested_range(Some("bytes=90-"), 100),
            Requested::Range {
                start: 90,
                length: 10
            }
        );
    }

    #[test]
    fn a_suffix_range_counts_back_from_the_end() {
        assert_eq!(
            requested_range(Some("bytes=-10"), 100),
            Requested::Range {
                start: 90,
                length: 10
            }
        );
        // A suffix larger than the object is the whole object, which is what
        // a resuming client asking for "the last lot" means.
        assert_eq!(
            requested_range(Some("bytes=-500"), 100),
            Requested::Range {
                start: 0,
                length: 100
            }
        );
    }

    #[test]
    fn an_end_past_the_object_is_clamped_rather_than_refused() {
        // Asking for more than exists is how a client says "the rest".
        assert_eq!(
            requested_range(Some("bytes=95-500"), 100),
            Requested::Range {
                start: 95,
                length: 5
            }
        );
    }

    #[test]
    fn a_start_past_the_object_is_unsatisfiable() {
        assert_eq!(
            requested_range(Some("bytes=100-"), 100),
            Requested::Unsatisfiable
        );
        assert_eq!(
            requested_range(Some("bytes=200-300"), 100),
            Requested::Unsatisfiable
        );
    }

    #[test]
    fn every_range_over_an_empty_object_is_unsatisfiable() {
        assert_eq!(
            requested_range(Some("bytes=0-"), 0),
            Requested::Unsatisfiable
        );
        assert_eq!(
            requested_range(Some("bytes=-1"), 0),
            Requested::Unsatisfiable
        );
    }

    #[test]
    fn a_zero_length_suffix_is_unsatisfiable() {
        // "the last nothing" has no meaningful answer.
        assert_eq!(
            requested_range(Some("bytes=-0"), 100),
            Requested::Unsatisfiable
        );
    }

    #[test]
    fn anything_this_does_not_implement_falls_back_to_the_whole_object() {
        // Permitted by RFC 9110, and better for a client than a refusal.
        for header in [
            "bytes=0-10,20-30",
            "items=0-10",
            "bytes=abc-def",
            "bytes=",
            "nonsense",
            "bytes=10-5",
        ] {
            assert_eq!(
                requested_range(Some(header), 100),
                Requested::Whole,
                "{header} should have been ignored"
            );
        }
    }
}
