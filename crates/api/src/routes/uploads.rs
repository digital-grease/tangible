// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Uploads sent in parts (docs/plans/acquisition.md A3).
//!
//! A large image uploaded in one request is lost whole when the connection
//! drops. Here the server chooses a part size, the client sends parts in any
//! order (a part sent again replaces itself) and asks which have arrived,
//! and completing the upload queues its import with the bytes already
//! staged. Each part is written at its own place in one file, hashed as it
//! arrives, and checked against the digest the client sent, when it sent
//! one: a browser can compute it only over https or on localhost.
//!
//! An upload is its starter's alone. Room for the whole file is checked when
//! it starts, and one left idle for a day is abandoned and its bytes
//! removed.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tangible_db::uploads::{
    NewUpload, UploadOutcome, UploadRecord, abandon_upload, complete_upload, create_upload,
    get_upload, record_part,
};
use tangible_domain::{ImportJobId, LogicalPath};
use time::format_description::well_known::Rfc3339;
use tokio::io::{AsyncSeekExt as _, AsyncWriteExt as _};
use utoipa::ToSchema;

use crate::auth::SignedIn;
use crate::problem::{ErrorCode, Problem};
use crate::routes::imports::{ImportView, staged_name};
use crate::state::ApiState;

/// Room kept free beyond an upload's own size.
const HEADROOM_BYTES: u64 = 64 * 1024 * 1024;

/// The header a part's SHA-256 travels in.
const CONTENT_SHA256: &str = "x-content-sha256";

/// An upload sent in parts.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UploadView {
    /// Its id, also the import's once completed.
    pub id: String,
    /// The file's name.
    pub filename: String,
    /// The whole file's length.
    pub total_bytes: u64,
    /// Every part's length but the last's.
    pub part_size: u64,
    /// How many parts there are.
    pub part_count: u64,
    /// The parts that have arrived, numbered from 0.
    pub received_parts: Vec<u64>,
    /// Bytes in those parts.
    pub bytes_received: u64,
    /// `open`, `completed` or `abandoned`.
    pub state: String,
    /// When it is abandoned if no part arrives before then.
    pub expires_at: String,
}

impl From<UploadRecord> for UploadView {
    fn from(record: UploadRecord) -> Self {
        Self {
            id: record.id.to_string(),
            part_count: record.plan().map_or(0, |plan| plan.part_count()),
            filename: record.filename,
            total_bytes: record.total_bytes,
            part_size: record.part_size,
            received_parts: record.received_parts,
            bytes_received: record.bytes_received,
            state: record.state,
            expires_at: record.expires_at.format(&Rfc3339).unwrap_or_default(),
        }
    }
}

/// What a client sends to start an upload.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateUploadRequest {
    /// What to call the file: one name, no directories.
    pub filename: String,
    /// The whole file's length.
    pub total_bytes: u64,
    /// The whole file's SHA-256, when known: checked before it is imported.
    #[serde(default)]
    pub expected_sha256: Option<String>,
}

fn unavailable(detail: &str) -> Problem {
    Problem::new(ErrorCode::StorageUnavailable, detail)
}

fn parse_upload(raw: &str) -> Result<uuid::Uuid, Problem> {
    uuid::Uuid::parse_str(raw)
        .map_err(|_| Problem::invalid_parameter("upload_id", "not a valid identifier"))
}

fn outcome_problem(id: uuid::Uuid, outcome: UploadOutcome) -> Problem {
    match outcome {
        UploadOutcome::NotFound => Problem::not_found("upload", &id.to_string()),
        UploadOutcome::NotOpen(state) => Problem::new(
            ErrorCode::Conflict,
            format!("this upload is {state}; start a new one to send the file again"),
        ),
        UploadOutcome::Incomplete { missing } => {
            let shown: Vec<String> = missing.iter().take(10).map(ToString::to_string).collect();
            Problem::new(
                ErrorCode::Conflict,
                format!(
                    "{} part(s) still to send: {}{}",
                    missing.len(),
                    shown.join(", "),
                    if missing.len() > shown.len() {
                        ", …"
                    } else {
                        ""
                    }
                ),
            )
        }
    }
}

fn database(what: &'static str) -> impl FnOnce(tangible_db::DbError) -> Problem {
    move |error| {
        tracing::error!(error = ?error, "{what}");
        unavailable(what)
    }
}

/// Start an upload sent in parts.
///
/// # Errors
///
/// `VALIDATION_FAILED` for a filename or digest that will not do, or a size
/// past the limit (413); `INSUFFICIENT_SPACE` (507) when the staging area
/// has no room for it; `RATE_LIMITED` past 60 an hour per account.
#[utoipa::path(
    post,
    path = "/api/v1/uploads",
    tag = "imports",
    description = "Start an upload sent in parts. The answer names the part size; send \\
                   each part with PUT /uploads/{id}/parts/{n}, ask which have arrived \\
                   with GET, and complete it to queue the import.",
    request_body = CreateUploadRequest,
    responses(
        (status = 201, description = "Started", body = UploadView),
        (status = 413, description = "Larger than this server accepts", body = Problem),
        (status = 422, description = "A filename or digest that will not do", body = Problem),
        (status = 429, description = "Too many uploads started for now", body = Problem),
        (status = 507, description = "Not enough room in staging", body = Problem),
    ),
)]
pub async fn start_upload(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Json(request): Json<CreateUploadRequest>,
) -> Result<axum::response::Response, Problem> {
    let Some(imports) = state.imports() else {
        return Err(unavailable("importing is not configured on this server"));
    };
    let logical = staged_name(Some(&request.filename))?;
    if request.total_bytes > imports.max_upload_bytes {
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
    let expected_sha256 = request
        .expected_sha256
        .as_deref()
        .map(|digest| digest.trim().to_ascii_lowercase())
        .map(|digest| {
            (digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()))
                .then_some(digest)
                .ok_or_else(|| {
                    Problem::new(
                        ErrorCode::ValidationFailed,
                        "expected_sha256 must be 64 hexadecimal characters",
                    )
                })
        })
        .transpose()?;
    if let Err(wait) = state.upload_limiter().try_acquire(&user.username) {
        let mut response = Problem::new(
            ErrorCode::RateLimited,
            "too many uploads started from this account for now; try again later",
        )
        .into_response();
        if let Ok(value) = axum::http::HeaderValue::from_str(&wait.as_secs().max(1).to_string()) {
            response
                .headers_mut()
                .insert(axum::http::header::RETRY_AFTER, value);
        }
        return Ok(response);
    }

    let id = ImportJobId::generate();
    let area = imports
        .pipeline
        .open_area(id)
        .await
        .map_err(|_| unavailable("the upload could not be staged"))?;
    let needed = request.total_bytes.saturating_add(HEADROOM_BYTES);
    if let Some(space) = tangible_storage::space::space_of(area.path())
        && space.free_bytes < needed
    {
        let _ = imports.pipeline.discard(id).await;
        return Err(Problem::new(
            ErrorCode::InsufficientSpace,
            format!(
                "the server has {} bytes free for staging and this upload needs {needed}",
                space.free_bytes
            ),
        ));
    }
    // The whole length at once, so every part has its place in the file.
    // Sparse where the filesystem allows; the check above is of room, not a
    // reservation.
    let file = area
        .create_file(&logical)
        .await
        .map_err(|_| unavailable("the upload could not be staged"))?;
    file.set_len(request.total_bytes)
        .await
        .map_err(|_| unavailable("the upload could not be staged"))?;

    let record = create_upload(
        state.database().pool(),
        NewUpload {
            id: *id.as_uuid(),
            filename: logical.as_str(),
            total_bytes: request.total_bytes,
            part_size: imports.upload_part_bytes.max(1),
            expected_sha256: expected_sha256.as_deref(),
            created_by: &user.username,
        },
    )
    .await
    .map_err(database("the upload could not be recorded"))?;
    tracing::info!(upload_id = %record.id, bytes = record.total_bytes, "upload started");
    Ok((StatusCode::CREATED, Json(UploadView::from(record))).into_response())
}

/// An upload of yours, and which parts have arrived.
///
/// # Errors
///
/// `NOT_FOUND` for an upload that does not exist or is not yours.
#[utoipa::path(
    get,
    path = "/api/v1/uploads/{upload_id}",
    tag = "imports",
    params(("upload_id" = String, Path, description = "Upload identifier")),
    responses(
        (status = 200, description = "The upload", body = UploadView),
        (status = 404, description = "No such upload of yours", body = Problem),
    ),
)]
pub async fn show_upload(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(upload_id): Path<String>,
) -> Result<Json<UploadView>, Problem> {
    let id = parse_upload(&upload_id)?;
    let record = get_upload(state.database().pool(), id, &user.username)
        .await
        .map_err(database("the upload could not be read"))?
        .ok_or_else(|| Problem::not_found("upload", &id.to_string()))?;
    Ok(Json(UploadView::from(record)))
}

/// Send one part.
///
/// # Errors
///
/// `NOT_FOUND`, `CONFLICT` for an upload no longer open, `VALIDATION_FAILED`
/// for a part number past the last, a length that is not the part's, or a
/// digest that does not match what arrived.
#[utoipa::path(
    put,
    path = "/api/v1/uploads/{upload_id}/parts/{part_number}",
    tag = "imports",
    description = "Send part n (from 0) as the body: exactly the part's length, with \\
                   Content-Length, and the part's SHA-256 in X-Content-SHA256 when the \\
                   client can compute it. A part sent again replaces itself.",
    params(
        ("upload_id" = String, Path, description = "Upload identifier"),
        ("part_number" = u64, Path, description = "Part number, from 0"),
    ),
    request_body(content = String, description = "The part's bytes", content_type = "application/octet-stream"),
    responses(
        (status = 200, description = "Received", body = UploadView),
        (status = 404, description = "No such upload of yours", body = Problem),
        (status = 409, description = "The upload is no longer open", body = Problem),
        (status = 422, description = "A part that will not do", body = Problem),
    ),
)]
pub async fn put_part(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path((upload_id, part_number)): Path<(String, u64)>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Result<Json<UploadView>, Problem> {
    let Some(imports) = state.imports() else {
        return Err(unavailable("importing is not configured on this server"));
    };
    let id = parse_upload(&upload_id)?;
    let invalid = |detail: String| Problem::new(ErrorCode::ValidationFailed, detail);
    let upload = get_upload(state.database().pool(), id, &user.username)
        .await
        .map_err(database("the upload could not be read"))?
        .ok_or_else(|| Problem::not_found("upload", &id.to_string()))?;
    if upload.state != "open" {
        return Err(outcome_problem(id, UploadOutcome::NotOpen(upload.state)));
    }
    let (offset, length) = upload
        .plan()
        .and_then(|plan| plan.part(part_number))
        .ok_or_else(|| invalid(format!("this upload has no part {part_number}")))?;
    let declared = headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared != Some(length) {
        return Err(invalid(format!(
            "part {part_number} is {length} bytes; send exactly that, with Content-Length"
        )));
    }
    let claimed = headers
        .get(CONTENT_SHA256)
        .and_then(|value| value.to_str().ok())
        .map(str::to_ascii_lowercase);

    let logical = LogicalPath::parse(&upload.filename)
        .map_err(|_| unavailable("the upload's file name cannot be used"))?;
    let digest = write_part(imports, id, &logical, offset, length, body).await?;
    if let Some(claimed) = claimed
        && claimed != digest
    {
        return Err(invalid(format!(
            "UPLOAD_MISMATCH: part {part_number} arrived with SHA-256 {digest}, not {claimed}; \\
             send it again"
        )));
    }
    let record = record_part(
        state.database().pool(),
        id,
        &user.username,
        part_number,
        length,
        &digest,
    )
    .await
    .map_err(database("the part could not be recorded"))?
    .map_err(|outcome| outcome_problem(id, outcome))?;
    Ok(Json(UploadView::from(record)))
}

/// Write a part's body at its place in the staged file, hashing it, refusing
/// more or fewer bytes than its length. Durable before it returns.
async fn write_part(
    imports: &crate::state::ImportContext,
    id: uuid::Uuid,
    logical: &LogicalPath,
    offset: u64,
    length: u64,
    body: axum::body::Body,
) -> Result<String, Problem> {
    use futures_util::StreamExt as _;

    let staging = |error: &dyn std::fmt::Debug| {
        tracing::error!(error = ?error, "could not stage an upload's part");
        unavailable("the part could not be staged")
    };
    let area = imports
        .pipeline
        .open_area(ImportJobId::from_uuid(id))
        .await
        .map_err(|e| staging(&e))?;
    let path = area
        .prepare_parent(logical)
        .await
        .map_err(|e| staging(&e))?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .await
        .map_err(|e| staging(&e))?;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|e| staging(&e))?;
    let mut hasher = Sha256::new();
    let mut received = 0_u64;
    let mut stream = std::pin::pin!(body.into_data_stream());
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| {
            Problem::new(
                ErrorCode::ValidationFailed,
                "the part ended before it was complete; send it again",
            )
        })?;
        received = received.saturating_add(chunk.len() as u64);
        if received > length {
            return Err(Problem::new(
                ErrorCode::ValidationFailed,
                "the part is longer than its Content-Length",
            ));
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(|e| staging(&e))?;
    }
    if received != length {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            "the part ended before it was complete; send it again",
        ));
    }
    file.flush().await.map_err(|e| staging(&e))?;
    file.sync_data().await.map_err(|e| staging(&e))?;
    Ok(hex::encode(hasher.finalize()))
}

/// Complete an upload whose parts have all arrived, and queue its import.
///
/// Repeating it answers the same import.
///
/// # Errors
///
/// `NOT_FOUND`; `CONFLICT` for an upload abandoned, or with parts still to
/// send, which the detail lists.
#[utoipa::path(
    post,
    path = "/api/v1/uploads/{upload_id}/complete",
    tag = "imports",
    params(("upload_id" = String, Path, description = "Upload identifier")),
    responses(
        (status = 202, description = "Import queued", body = ImportView),
        (status = 404, description = "No such upload of yours", body = Problem),
        (status = 409, description = "Parts missing, or no longer open", body = Problem),
    ),
)]
pub async fn finish_upload(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(upload_id): Path<String>,
) -> Result<axum::response::Response, Problem> {
    let id = parse_upload(&upload_id)?;
    let upload = get_upload(state.database().pool(), id, &user.username)
        .await
        .map_err(database("the upload could not be read"))?
        .ok_or_else(|| Problem::not_found("upload", &id.to_string()))?;
    let mut descriptor = serde_json::json!({
        "kind": "upload",
        "filename": upload.filename,
        "reference": "upload",
        "expected_bytes": upload.total_bytes,
    });
    if let Some(digest) = &upload.expected_sha256 {
        descriptor["expected_sha256"] = serde_json::json!(digest);
    }
    let job = complete_upload(state.database().pool(), id, &user.username, &descriptor)
        .await
        .map_err(database("the upload could not be completed"))?
        .map_err(|outcome| outcome_problem(id, outcome))?;
    tracing::info!(upload_id = %id, "upload completed and its import queued");
    Ok((StatusCode::ACCEPTED, Json(ImportView::from(job))).into_response())
}

/// Abandon an upload, removing what it sent.
///
/// # Errors
///
/// `NOT_FOUND`; `CONFLICT` once it is completed.
#[utoipa::path(
    delete,
    path = "/api/v1/uploads/{upload_id}",
    tag = "imports",
    params(("upload_id" = String, Path, description = "Upload identifier")),
    responses(
        (status = 204, description = "Abandoned"),
        (status = 404, description = "No such upload of yours", body = Problem),
        (status = 409, description = "Already completed or abandoned", body = Problem),
    ),
)]
pub async fn drop_upload(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(upload_id): Path<String>,
) -> Result<StatusCode, Problem> {
    let id = parse_upload(&upload_id)?;
    abandon_upload(state.database().pool(), id, &user.username)
        .await
        .map_err(database("the upload could not be abandoned"))?
        .map_err(|outcome| outcome_problem(id, outcome))?;
    if let Some(imports) = state.imports()
        && let Err(error) = imports.pipeline.discard(ImportJobId::from_uuid(id)).await
    {
        tracing::warn!(upload_id = %id, error = ?error, "could not remove an abandoned upload");
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The upload routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/uploads", post(start_upload))
        .route("/uploads/{upload_id}", get(show_upload).delete(drop_upload))
        .route("/uploads/{upload_id}/parts/{part_number}", put(put_part))
        .route("/uploads/{upload_id}/complete", post(finish_upload))
}
