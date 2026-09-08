// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Getting bytes into the library.
//!
//! Two ways in, and neither accepts a host path. An upload streams its body
//! into a staging area and is queued already staged; a watched-folder import
//! names a root an administrator configured and a relative path beneath it,
//! and the runner copies it in.
//!
//! The API never takes a path from the caller because that is the one place
//! where an untrusted string meets the filesystem. `WatchRoots` closes every
//! way out of a configured root, and this layer refuses anything it will not
//! resolve before a job exists, so an operator is told immediately rather than
//! by a job that fails a minute later.
//!
//! Requests return as soon as the bytes are safe. Hashing and inspecting a
//! large image takes minutes, and a request that held a connection open for it
//! would fail on the first proxy timeout and take the import with it. The
//! background runner does that work and the job records its progress.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tangible_db::repositories::{
    ImportCommandOutcome, ImportJobRecord, NewImportJob, cancel_import_job, create_import_job,
    get_import_job, list_import_jobs, retry_import_job,
};
use tangible_domain::{ImportJobId, ImportState, LogicalPath};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::io::AsyncWriteExt as _;
use utoipa::{IntoParams, ToSchema};

use crate::pagination::{decode_cursor, encode_cursor};
use crate::problem::{ErrorCode, Problem};
use crate::state::ApiState;

/// Largest page a caller may ask for.
const MAX_LIMIT: usize = 500;

/// Default page size.
const DEFAULT_LIMIT: usize = 50;

/// Longest filename accepted on an upload.
const MAX_FILENAME: usize = 255;

/// Who an import is recorded as having been requested by.
///
/// A placeholder until there are user accounts, recorded rather than left
/// null because the column is part of the audit trail.
const REQUESTED_BY: &str = "operator";

fn rfc3339(value: OffsetDateTime) -> String {
    value.format(&Rfc3339).unwrap_or_default()
}

// --- views ---------------------------------------------------------------------

/// An import job.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ImportView {
    /// Opaque identifier.
    pub id: String,
    /// Where the bytes come from.
    pub source_kind: String,
    /// The filename as received, when one was recorded.
    pub source_filename: Option<String>,
    /// Lifecycle state.
    pub state: String,
    /// Whether the import has finished, one way or another.
    pub is_settled: bool,
    /// How many bytes are expected, when the source said.
    pub bytes_expected: Option<i64>,
    /// How many have arrived.
    pub bytes_received: i64,
    /// The artifact it produced, once it has.
    pub artifact_id: Option<String>,
    /// A stable failure code, when it failed.
    pub error_code: Option<String>,
    /// Human-readable failure detail, when it failed.
    pub error_detail: Option<String>,
    /// Whether a retry would resume rather than start again.
    ///
    /// Surfaced because it is the question an operator asks before pressing
    /// retry on a large import.
    pub resumes_from: Option<String>,
    /// Structural observations gathered so far.
    ///
    /// Present as soon as inspection has run, so a warning is visible before
    /// the import finishes rather than only afterwards.
    pub warnings: Vec<String>,
    /// Who asked.
    pub created_by: String,
    /// When it was requested.
    pub created_at: String,
    /// When it last changed.
    pub updated_at: String,
    /// When it finished, if it has.
    pub completed_at: Option<String>,
}

impl From<ImportJobRecord> for ImportView {
    fn from(record: ImportJobRecord) -> Self {
        // The checkpoint is the pipeline's own record. Only the parts that
        // mean something to an operator are lifted out of it.
        let warnings = record
            .checkpoint
            .as_ref()
            .and_then(|checkpoint| checkpoint.get("warnings"))
            .and_then(|warnings| warnings.as_array())
            .map(|warnings| {
                warnings
                    .iter()
                    .filter_map(|warning| warning.as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default();

        Self {
            id: record.id.to_string(),
            source_kind: record.source_kind,
            source_filename: record
                .source_descriptor
                .get("filename")
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned),
            state: record.state.to_string(),
            is_settled: record.state.is_terminal(),
            bytes_expected: record.bytes_expected,
            bytes_received: record.bytes_received,
            artifact_id: record.artifact_id.map(|id| id.to_string()),
            error_code: record.error_code,
            error_detail: record.error_detail,
            resumes_from: record.resume_stage,
            warnings,
            created_by: record.created_by,
            created_at: rfc3339(record.created_at),
            updated_at: rfc3339(record.updated_at),
            completed_at: record.completed_at.map(rfc3339),
        }
    }
}

/// A page of imports.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ImportPage {
    /// The imports in this page, newest first.
    pub items: Vec<ImportView>,
    /// Cursor for the next page, or null when this is the last.
    pub next_cursor: Option<String>,
}

/// Where imports may be taken from.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ImportSources {
    /// Identifiers of the watched roots an administrator configured.
    ///
    /// Identifiers only. The paths behind them are the host's business and
    /// are never exposed.
    pub watch_roots: Vec<String>,
    /// Largest upload the server will accept, in bytes.
    pub max_upload_bytes: i64,
}

// --- requests ------------------------------------------------------------------

/// What an operator sends to import from a watched folder.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateImportRequest {
    /// Which configured root the file is under.
    pub path_id: String,
    /// Where beneath that root, as a relative path.
    pub relative_path: String,
}

/// Query parameters for an upload.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct UploadQuery {
    /// Filename as it should be recorded. A detection hint, never a path.
    pub filename: Option<String>,
}

/// Query parameters for the listing.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct ImportQuery {
    /// Maximum imports to return. Clamped.
    pub limit: Option<usize>,
    /// Opaque cursor from a previous response's `next_cursor`.
    pub cursor: Option<String>,
    /// Only imports in this state.
    pub state: Option<String>,
}

fn limit_of(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

fn as_i64(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::try_from(DEFAULT_LIMIT).unwrap_or(50))
}

fn unavailable(detail: &str) -> Problem {
    Problem::new(ErrorCode::StorageUnavailable, detail)
}

fn parse_import(raw: &str) -> Result<ImportJobId, Problem> {
    raw.parse::<ImportJobId>()
        .map_err(|_| Problem::invalid_parameter("import_id", "not a valid identifier"))
}

/// Check an upload filename before it is used as a staged path.
///
/// Kept pure and total: a filename arrives from a browser, and the only safe
/// version of it is one that names a single file and nothing else.
fn staged_name(filename: Option<&str>) -> Result<LogicalPath, Problem> {
    let name = filename.unwrap_or("upload.bin").trim();
    if name.is_empty() || name.len() > MAX_FILENAME {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            format!("filename must be between 1 and {MAX_FILENAME} characters"),
        ));
    }
    // A filename, not a path. Anything with a separator in it is either a
    // mistake or an attempt, and both are refused the same way.
    if name.contains('/') || name.contains('\\') {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            "filename must name one file, without directories",
        ));
    }
    LogicalPath::parse(name).map_err(|_| {
        Problem::new(
            ErrorCode::ValidationFailed,
            "that filename cannot be stored safely",
        )
    })
}

// --- routes --------------------------------------------------------------------

/// What may be imported from.
///
/// # Errors
///
/// `STORAGE_UNAVAILABLE` when no library is configured.
#[utoipa::path(
    get,
    path = "/api/v1/import-sources",
    tag = "imports",
    description = "List the watched roots configured for importing, and the \
                   upload size limit.",
    responses(
        (status = 200, description = "Where imports may come from", body = ImportSources),
        (status = 503, description = "Storage is not configured", body = Problem),
    ),
)]
pub async fn list_sources(State(state): State<ApiState>) -> Result<Json<ImportSources>, Problem> {
    let Some(imports) = state.imports() else {
        return Err(unavailable("importing is not configured on this server"));
    };
    Ok(Json(ImportSources {
        watch_roots: imports.roots.ids(),
        max_upload_bytes: i64::try_from(imports.max_upload_bytes).unwrap_or(i64::MAX),
    }))
}

/// Import a file from a watched folder.
///
/// Returns as soon as the job exists. The bytes are copied, hashed and
/// inspected by the background runner, and the job records how far it has got.
///
/// # Errors
///
/// `VALIDATION_FAILED` for a path this server will not resolve, or
/// `REFERENCE_NOT_FOUND` if the root is not configured or the file is not
/// there.
#[utoipa::path(
    post,
    path = "/api/v1/imports",
    tag = "imports",
    description = "Import a file from a configured watched root. The server \
                   never accepts a host path: name a root and a relative path \
                   beneath it.",
    request_body = CreateImportRequest,
    responses(
        (status = 202, description = "Queued", body = ImportView),
        (status = 422, description = "The path will not be resolved", body = Problem),
        (status = 503, description = "Storage is not configured", body = Problem),
    ),
)]
pub async fn create_import(
    State(state): State<ApiState>,
    Json(request): Json<CreateImportRequest>,
) -> Result<axum::response::Response, Problem> {
    let Some(imports) = state.imports() else {
        return Err(unavailable("importing is not configured on this server"));
    };

    // Resolved now, before a job exists, so an operator who mistyped a path is
    // told at once rather than by a job that fails a minute later. The runner
    // resolves it again when it copies, because the filesystem can change in
    // between and the check that matters is the one before the read.
    let source = imports
        .roots
        .resolve(&request.path_id, &request.relative_path)
        .await
        .map_err(|error| match error {
            tangible_storage::WatchRootError::UnknownRoot { path_id } => Problem::new(
                ErrorCode::ReferenceNotFound,
                format!("no watched root is configured as {path_id}"),
            ),
            tangible_storage::WatchRootError::NotFound => Problem::new(
                ErrorCode::ReferenceNotFound,
                "no such file beneath that watched root",
            ),
            other => Problem::new(ErrorCode::ValidationFailed, other.to_string()),
        })?;

    let filename = source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("import.bin")
        .to_owned();
    let bytes_expected = tokio::fs::metadata(&source)
        .await
        .ok()
        .and_then(|metadata| i64::try_from(metadata.len()).ok());

    let descriptor = serde_json::json!({
        "kind": "watch_folder",
        "path_id": request.path_id,
        "relative_path": request.relative_path,
        "filename": filename,
        // Safe to publish in a manifest: it names a configured root and a
        // relative path, never a host path, a credential or a URL.
        "reference": format!("watch_folder:{}/{}", request.path_id, request.relative_path),
    });

    let job = create_import_job(
        state.database().pool(),
        NewImportJob {
            id: ImportJobId::generate(),
            source_kind: "watch_folder",
            source_descriptor: &descriptor,
            state: ImportState::Requested,
            bytes_expected,
            bytes_received: 0,
            created_by: REQUESTED_BY,
        },
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not queue an import");
        unavailable("the import could not be queued")
    })?;

    tracing::info!(import_id = %job.id, path_id = %request.path_id, "watched-folder import queued");

    // 202: the bytes are known-good and the job exists, but nothing has been
    // imported yet. 201 would claim a finished thing.
    Ok((StatusCode::ACCEPTED, Json(ImportView::from(job))).into_response())
}

/// Upload a file and import it.
///
/// The body is the file. It is streamed into the job's staging area rather
/// than buffered, so an image larger than memory is not a problem, and the
/// size limit is enforced as the bytes arrive rather than trusted from a
/// header.
///
/// # Errors
///
/// `VALIDATION_FAILED` for an unusable filename, or `413` if the body exceeds
/// the configured limit.
#[utoipa::path(
    post,
    path = "/api/v1/imports/upload",
    tag = "imports",
    description = "Upload a file as the request body and queue it for import. \
                   Streamed to disk as it arrives; the size limit is enforced \
                   on the bytes, not on a header.",
    params(UploadQuery),
    request_body(content = String, description = "The file itself", content_type = "application/octet-stream"),
    responses(
        (status = 202, description = "Queued", body = ImportView),
        (status = 413, description = "Larger than this server accepts", body = Problem),
        (status = 422, description = "The filename is not usable", body = Problem),
        (status = 503, description = "Storage is not configured", body = Problem),
    ),
)]
pub async fn upload_import(
    State(state): State<ApiState>,
    Query(query): Query<UploadQuery>,
    body: axum::body::Body,
) -> Result<axum::response::Response, Problem> {
    use futures_util::StreamExt as _;

    let Some(imports) = state.imports() else {
        return Err(unavailable("importing is not configured on this server"));
    };
    let logical = staged_name(query.filename.as_deref())?;

    let import_id = ImportJobId::generate();
    let area = imports
        .pipeline
        .open_area(import_id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not open a staging area");
            unavailable("the upload could not be staged")
        })?;
    let mut file = area.create_file(&logical).await.map_err(|error| {
        tracing::error!(error = ?error, "could not create a staged file");
        unavailable("the upload could not be staged")
    })?;

    let mut received: u64 = 0;
    let mut stream = std::pin::pin!(body.into_data_stream());
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            tracing::warn!(error = ?error, "an upload was interrupted");
            Problem::new(
                ErrorCode::ValidationFailed,
                "the upload ended before it was complete",
            )
        })?;
        received += chunk.len() as u64;
        if received > imports.max_upload_bytes {
            // Enforced on the bytes rather than on a header, and enforced as
            // they arrive: a limit checked afterwards has already cost the
            // disk space it was meant to protect.
            let _ = tokio::fs::remove_file(area.path().join(logical.to_string())).await;
            let mut problem = Problem::new(
                ErrorCode::ValidationFailed,
                format!(
                    "this server accepts uploads up to {} bytes",
                    imports.max_upload_bytes
                ),
            );
            problem.status = StatusCode::PAYLOAD_TOO_LARGE.as_u16();
            return Err(problem);
        }
        file.write_all(&chunk).await.map_err(|error| {
            tracing::error!(error = ?error, "could not write staged bytes");
            unavailable("the upload could not be staged")
        })?;
    }

    // Durable before the job says the bytes are staged. A job claiming staged
    // bytes that are still in a write-back cache would be claiming something
    // that may not survive a power cut.
    file.flush().await.map_err(|error| {
        tracing::error!(error = ?error, "could not flush staged bytes");
        unavailable("the upload could not be staged")
    })?;
    file.sync_all().await.map_err(|error| {
        tracing::error!(error = ?error, "could not sync staged bytes");
        unavailable("the upload could not be staged")
    })?;

    let descriptor = serde_json::json!({
        "kind": "upload",
        "filename": logical.to_string(),
        "reference": "upload",
    });

    let job = create_import_job(
        state.database().pool(),
        NewImportJob {
            id: import_id,
            source_kind: "upload",
            source_descriptor: &descriptor,
            // Already on disk, so the runner starts at hashing rather than
            // trying to acquire bytes that are here.
            state: ImportState::Staged,
            bytes_expected: i64::try_from(received).ok(),
            bytes_received: i64::try_from(received).unwrap_or(i64::MAX),
            created_by: REQUESTED_BY,
        },
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not queue an upload");
        unavailable("the import could not be queued")
    })?;

    tracing::info!(import_id = %job.id, bytes = received, "upload staged and queued");
    Ok((StatusCode::ACCEPTED, Json(ImportView::from(job))).into_response())
}

/// List imports, newest first.
///
/// # Errors
///
/// `INVALID_PARAMETER` for a malformed filter or cursor.
#[utoipa::path(
    get,
    path = "/api/v1/imports",
    tag = "imports",
    description = "List imports, newest first.",
    params(ImportQuery),
    responses(
        (status = 200, description = "A page of imports", body = ImportPage),
        (status = 400, description = "Malformed filter or cursor", body = Problem),
    ),
)]
pub async fn list_imports(
    State(state): State<ApiState>,
    Query(query): Query<ImportQuery>,
) -> Result<Json<ImportPage>, Problem> {
    let limit = limit_of(query.limit);
    let before = match &query.cursor {
        None => None,
        Some(cursor) => Some(
            *decode_cursor(cursor)?
                .parse::<ImportJobId>()
                .map_err(|_| Problem::new(ErrorCode::InvalidCursor, "the cursor is not usable"))?
                .as_uuid(),
        ),
    };
    let state_filter = match &query.state {
        None => None,
        Some(raw) => Some(raw.parse::<ImportState>().map_err(|_| {
            Problem::invalid_parameter("state", "not an import state this server knows")
        })?),
    };

    let jobs = list_import_jobs(state.database().pool(), state_filter, as_i64(limit), before)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not list imports");
            unavailable("the imports could not be read")
        })?;

    let next_cursor = if jobs.len() < limit {
        None
    } else {
        jobs.last().map(|job| encode_cursor(&job.id.to_string()))
    };

    Ok(Json(ImportPage {
        items: jobs.into_iter().map(ImportView::from).collect(),
        next_cursor,
    }))
}

async fn load(state: &ApiState, id: ImportJobId) -> Result<ImportJobRecord, Problem> {
    get_import_job(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read an import");
            unavailable("the import could not be read")
        })?
        .ok_or_else(|| Problem::not_found("import", &id.to_string()))
}

/// One import.
///
/// # Errors
///
/// `NOT_FOUND` if no such import exists.
#[utoipa::path(
    get,
    path = "/api/v1/imports/{import_id}",
    tag = "imports",
    description = "Fetch one import, including how far it has got and any \
                   structural findings so far.",
    params(("import_id" = String, Path, description = "Import identifier")),
    responses(
        (status = 200, description = "The import", body = ImportView),
        (status = 404, description = "No such import", body = Problem),
    ),
)]
pub async fn get_import(
    State(state): State<ApiState>,
    Path(import_id): Path<String>,
) -> Result<Json<ImportView>, Problem> {
    let id = parse_import(&import_id)?;
    Ok(Json(ImportView::from(load(&state, id).await?)))
}

/// Stop an import.
///
/// Honoured at any stage: unlike a burn, nothing physical is consumed, so
/// there is no point at which stopping does damage.
///
/// # Errors
///
/// `NOT_FOUND`, or `CONFLICT` if it has already finished.
#[utoipa::path(
    post,
    path = "/api/v1/imports/{import_id}/cancel",
    tag = "imports",
    description = "Stop an import. Honoured at any stage: nothing physical is \
                   consumed by importing.",
    params(("import_id" = String, Path, description = "Import identifier")),
    responses(
        (status = 200, description = "Cancelled", body = ImportView),
        (status = 404, description = "No such import", body = Problem),
        (status = 409, description = "Already finished", body = Problem),
    ),
)]
pub async fn cancel_import(
    State(state): State<ApiState>,
    Path(import_id): Path<String>,
) -> Result<Json<ImportView>, Problem> {
    let id = parse_import(&import_id)?;
    cancel_import_job(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not cancel an import");
            unavailable("the import could not be cancelled")
        })?
        .map_err(|outcome| command_problem(id, outcome))?;

    tracing::info!(import_id = %id, "import cancelled");
    Ok(Json(ImportView::from(load(&state, id).await?)))
}

/// Try an import again.
///
/// Resumes from the last stage that completed rather than starting over, so a
/// failure late in a large import does not cost the transfer.
///
/// # Errors
///
/// `NOT_FOUND`, or `CONFLICT` if it is still running or already succeeded.
#[utoipa::path(
    post,
    path = "/api/v1/imports/{import_id}/retry",
    tag = "imports",
    description = "Try a failed or cancelled import again, resuming from the \
                   last stage that completed.",
    params(("import_id" = String, Path, description = "Import identifier")),
    responses(
        (status = 200, description = "Requeued", body = ImportView),
        (status = 404, description = "No such import", body = Problem),
        (status = 409, description = "Still running, or already succeeded", body = Problem),
    ),
)]
pub async fn retry_import(
    State(state): State<ApiState>,
    Path(import_id): Path<String>,
) -> Result<Json<ImportView>, Problem> {
    let id = parse_import(&import_id)?;
    retry_import_job(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not retry an import");
            unavailable("the import could not be requeued")
        })?
        .map_err(|outcome| command_problem(id, outcome))?;

    tracing::info!(import_id = %id, "import requeued");
    Ok(Json(ImportView::from(load(&state, id).await?)))
}

/// Turn a refused command into a problem that says what to do instead.
fn command_problem(id: ImportJobId, outcome: ImportCommandOutcome) -> Problem {
    match outcome {
        ImportCommandOutcome::NotFound => Problem::not_found("import", &id.to_string()),
        ImportCommandOutcome::AlreadyFinished {
            state: ImportState::Complete,
        } => Problem::new(
            ErrorCode::Conflict,
            "this import already produced an artifact; importing the same bytes \
                 again is a new import",
        ),
        ImportCommandOutcome::AlreadyFinished { state } => Problem::new(
            ErrorCode::Conflict,
            format!("this import has already finished, in state {state}"),
        ),
        ImportCommandOutcome::Running { state } => Problem::new(
            ErrorCode::Conflict,
            format!("this import is still running, in state {state}; cancel it first"),
        ),
    }
}

/// The import routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/import-sources", get(list_sources))
        .route("/imports", get(list_imports).post(create_import))
        .route("/imports/upload", post(upload_import))
        .route("/imports/{import_id}", get(get_import))
        .route("/imports/{import_id}/cancel", post(cancel_import))
        .route("/imports/{import_id}/retry", post(retry_import))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_filename_is_a_filename_and_not_a_path() {
        // A browser sends whatever the operating system gave it, and on some
        // systems that includes directories.
        for bad in ["../escape.iso", "a/b.iso", "a\\b.iso", "/etc/passwd"] {
            assert!(staged_name(Some(bad)).is_err(), "{bad} should be refused");
        }
        assert_eq!(
            staged_name(Some("Example Disc.iso"))
                .expect("valid")
                .to_string(),
            "Example Disc.iso"
        );
    }

    #[test]
    fn an_absent_filename_gets_a_safe_default() {
        // The filename is a detection hint, not identity. An upload without
        // one is still importable.
        assert_eq!(
            staged_name(None).expect("default").to_string(),
            "upload.bin"
        );
    }

    #[test]
    fn an_empty_or_oversized_filename_is_refused() {
        assert!(staged_name(Some("   ")).is_err());
        assert!(staged_name(Some(&"x".repeat(MAX_FILENAME + 1))).is_err());
    }

    #[test]
    fn a_limit_is_clamped_rather_than_rejected() {
        assert_eq!(limit_of(None), DEFAULT_LIMIT);
        assert_eq!(limit_of(Some(0)), 1);
        assert_eq!(limit_of(Some(usize::MAX)), MAX_LIMIT);
    }

    #[test]
    fn each_refused_command_says_what_to_do_instead() {
        let id = ImportJobId::generate();
        let finished = command_problem(
            id,
            ImportCommandOutcome::AlreadyFinished {
                state: ImportState::Complete,
            },
        );
        assert!(finished.detail.contains("new import"));

        let running = command_problem(
            id,
            ImportCommandOutcome::Running {
                state: ImportState::Hashing,
            },
        );
        assert!(running.detail.contains("cancel it first"));
    }
}
