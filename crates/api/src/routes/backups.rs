// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Copies of the database, for administrators (ADR 0026,
//! docs/plans/backups-ui.md): the copies in the backups volume, taking one
//! now, fetching one, adding one, and removing one.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Serialize;
use tangible_db::backups::{Actor, audit};
use time::OffsetDateTime;
use utoipa::ToSchema;

use crate::auth::SignedIn;
use crate::backups::{BackupError, Backups, CopyEntry, Operation, Outcome, ToolsStatus};
use crate::problem::{ErrorCode, Problem};
use crate::state::ApiState;

/// The copies, and what the server can do with them.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BackupsView {
    /// Every copy in the backups volume, newest first.
    pub copies: Vec<CopyEntry>,
    /// How many of the copies Tangible names are kept
    /// (`TANGIBLE_BACKUPS_KEPT`); 0 when copies are turned off.
    pub kept: u32,
    /// Whether the server can write copies into the volume.
    pub writable: bool,
    /// Whether the dump tools can copy the database.
    pub tools: ToolsStatus,
    /// What is under way, if anything.
    pub operation: Option<Operation>,
    /// How the last copy taken or added since the server started went.
    pub last_outcome: Option<Outcome>,
    /// The largest copy an upload may be, in bytes.
    pub max_upload_bytes: u64,
}

fn unavailable(detail: impl Into<String>) -> Problem {
    Problem::new(ErrorCode::BackupsUnavailable, detail)
}

fn backups_of(state: &ApiState) -> Result<&Backups, Problem> {
    state
        .backups()
        .ok_or_else(|| unavailable("this server keeps no copies of the database"))
}

/// What a copy's failure answers.
fn problem_for(error: BackupError) -> Problem {
    match error {
        BackupError::Off => Problem::new(ErrorCode::BackupsOff, error.to_string()),
        BackupError::Busy(_) => Problem::new(ErrorCode::BackupBusy, error.to_string()),
        BackupError::NotFound => Problem::new(ErrorCode::NotFound, "no copy is named that"),
        BackupError::ToolsUnavailable(detail) | BackupError::NotWritable(detail) => {
            unavailable(detail)
        }
        BackupError::NotACopy(detail) => Problem::new(ErrorCode::BackupNotACopy, detail),
        BackupError::TooLarge(limit) => {
            let mut problem = Problem::new(
                ErrorCode::ValidationFailed,
                format!("this server accepts copies up to {limit} bytes"),
            );
            problem.status = StatusCode::PAYLOAD_TOO_LARGE.as_u16();
            problem
        }
        BackupError::Interrupted => Problem::new(
            ErrorCode::ValidationFailed,
            "the upload ended before it was complete; send it again",
        ),
        BackupError::Io { .. } | BackupError::Tool { .. } | BackupError::Database(_) => {
            tracing::error!(error = ?error, "a copy of the database failed");
            unavailable(error.to_string())
        }
    }
}

fn audit_failed(error: &tangible_db::DbError) {
    tracing::error!(error = ?error, "a copy's audit record could not be written");
}

async fn view(state: &ApiState, backups: &Backups) -> Result<BackupsView, Problem> {
    let listing = backups.list().await.map_err(problem_for)?;
    Ok(BackupsView {
        copies: listing.copies,
        kept: backups.settings().kept,
        writable: listing.writable,
        tools: backups.tools(state.database()).await,
        operation: backups.operation(),
        last_outcome: backups.last_outcome(),
        max_upload_bytes: backups.settings().max_upload_bytes,
    })
}

/// The copies of the database, and what the server can do with them.
///
/// # Errors
///
/// `BACKUPS_UNAVAILABLE` when this server keeps none, or the volume cannot
/// be read.
#[utoipa::path(
    get,
    path = "/api/v1/backups",
    tag = "backups",
    description = "The copies of the database in the backups volume, newest first: those \
                   Tangible took (`tangible-*.dump`: before every start, on the schedule, or \
                   asked for here), counted and pruned to `kept`, and those uploaded or put \
                   there at the machine, which are never pruned. Also whether the server can \
                   write there and its dump tools can copy this database, and what is under \
                   way. Needs `manage_backups`.",
    responses(
        (status = 200, description = "The copies", body = BackupsView),
        (status = 503, description = "This server keeps no copies, or cannot read them", body = Problem),
    ),
)]
pub async fn list_backups(State(state): State<ApiState>) -> Result<Json<BackupsView>, Problem> {
    let backups = backups_of(&state)?;
    Ok(Json(view(&state, backups).await?))
}

/// Take a copy of the database now.
///
/// # Errors
///
/// `BACKUPS_OFF`, `BACKUP_BUSY`, or `BACKUPS_UNAVAILABLE` when the tools or
/// the volume are not ready.
#[utoipa::path(
    post,
    path = "/api/v1/backups",
    tag = "backups",
    description = "Take a copy of the database now, in the background: `operation` says it \
                   is under way, and `last_outcome` how it went. It is named as every copy \
                   Tangible takes, and the oldest beyond `kept` go. Audited as \
                   `backup.taken`. Needs `manage_backups`.",
    responses(
        (status = 202, description = "Being taken", body = BackupsView),
        (status = 409, description = "Copies are turned off, or something is under way", body = Problem),
        (status = 503, description = "The tools or the volume are not ready", body = Problem),
    ),
)]
pub async fn take_backup(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
) -> Result<(StatusCode, Json<BackupsView>), Problem> {
    let backups = backups_of(&state)?.clone();
    if backups.settings().kept == 0 {
        return Err(problem_for(BackupError::Off));
    }
    ready(&state, &backups).await?;
    let turn = backups
        .begin(Operation::Taking {
            started_at: OffsetDateTime::now_utc(),
            by: user.username.clone(),
        })
        .map_err(problem_for)?;
    let database = state.database().clone();
    let copies = backups.clone();
    let username = user.username.clone();
    tokio::spawn(async move {
        let taken = copies.take(&turn).await;
        drop(turn);
        let (name, succeeded, metadata) = match &taken {
            Ok(copy) => (
                copy.name.clone(),
                true,
                serde_json::json!({ "size_bytes": copy.size_bytes }),
            ),
            Err(error) => {
                tracing::warn!(error = %error, "a copy asked for was not taken");
                (
                    String::new(),
                    false,
                    serde_json::json!({ "error": error.to_string() }),
                )
            }
        };
        let name = if name.is_empty() { "-" } else { name.as_str() };
        if let Err(error) = audit(
            database.pool(),
            Actor::User(&username),
            "backup.taken",
            name,
            succeeded,
            metadata,
        )
        .await
        {
            audit_failed(&error);
        }
    });
    Ok((StatusCode::ACCEPTED, Json(view(&state, &backups).await?)))
}

/// Whether the tools can copy the database and the volume can be written.
async fn ready(state: &ApiState, backups: &Backups) -> Result<(), Problem> {
    let tools = backups.tools(state.database()).await;
    if !tools.ready {
        return Err(unavailable(tools.problem.unwrap_or_else(|| {
            "the dump tools cannot copy this database".to_owned()
        })));
    }
    let listing = backups.list().await.map_err(problem_for)?;
    if !listing.writable {
        return Err(problem_for(BackupError::NotWritable(
            "start the stack once with this release's compose.yaml (`docker compose up -d`), \
             whose backup service lets the server write there"
                .to_owned(),
        )));
    }
    Ok(())
}

/// Add a copy: the request's body is the file.
///
/// # Errors
///
/// `BACKUP_NOT_A_COPY`, `BACKUP_BUSY`, `VALIDATION_FAILED` (413) for one too
/// large, or `BACKUPS_UNAVAILABLE`.
#[utoipa::path(
    put,
    path = "/api/v1/backups/uploads",
    tag = "backups",
    description = "Add a copy of the database, from another install or one downloaded \
                   earlier: the request's body is the file, a PostgreSQL custom-format \
                   archive (`pg_dump --format=custom`), at most `max_upload_bytes`. It is \
                   kept as `uploaded-YYYYMMDDTHHMMSSZ.dump`, never pruned, once whole and \
                   read by `pg_restore` as an archive; nothing in it is run. Audited as \
                   `backup.uploaded`. Needs `manage_backups`.",
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses(
        (status = 201, description = "Kept", body = CopyEntry),
        (status = 409, description = "Something is under way", body = Problem),
        (status = 413, description = "Larger than this server accepts", body = Problem),
        (status = 422, description = "Not a copy of a database", body = Problem),
        (status = 503, description = "The tools or the volume are not ready", body = Problem),
    ),
)]
pub async fn upload_backup(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Result<(StatusCode, Json<CopyEntry>), Problem> {
    let backups = backups_of(&state)?.clone();
    let limit = backups.settings().max_upload_bytes;
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|length| length > limit) {
        return Err(problem_for(BackupError::TooLarge(limit)));
    }
    ready(&state, &backups).await?;
    let turn = backups
        .begin(Operation::Uploading {
            started_at: OffsetDateTime::now_utc(),
            by: user.username.clone(),
        })
        .map_err(problem_for)?;
    let stored = backups.store_upload(&turn, body.into_data_stream()).await;
    drop(turn);
    let copy = stored.map_err(problem_for)?;
    audit(
        state.database().pool(),
        Actor::User(&user.username),
        "backup.uploaded",
        &copy.name,
        true,
        serde_json::json!({ "size_bytes": copy.size_bytes }),
    )
    .await
    .map_err(|error| {
        audit_failed(&error);
        unavailable("the copy was kept, but its audit record could not be written")
    })?;
    Ok((StatusCode::CREATED, Json(copy)))
}

/// Fetch a copy.
///
/// # Errors
///
/// `NOT_FOUND`, or `BACKUPS_UNAVAILABLE`.
#[utoipa::path(
    get,
    path = "/api/v1/backups/{name}/download",
    tag = "backups",
    description = "The copy's file, as a download. It holds every account's password hash, \
                   the integration passwords sealed with the server's key (not the key), and \
                   the audit trail: keep it where only you can read it. Audited as \
                   `backup.downloaded`. Needs `manage_backups`.",
    params(("name" = String, Path, description = "The copy's name, as listed")),
    responses(
        (status = 200, description = "The copy", content_type = "application/octet-stream", body = Vec<u8>),
        (status = 404, description = "No copy is named that", body = Problem),
        (status = 503, description = "This server keeps no copies", body = Problem),
    ),
)]
pub async fn download_backup(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(name): Path<String>,
) -> Result<Response, Problem> {
    let backups = backups_of(&state)?;
    let (file, copy) = backups.open(&name).await.map_err(problem_for)?;
    audit(
        state.database().pool(),
        Actor::User(&user.username),
        "backup.downloaded",
        &copy.name,
        true,
        serde_json::json!({ "size_bytes": copy.size_bytes }),
    )
    .await
    .map_err(|error| {
        audit_failed(&error);
        unavailable("the copy's download could not be recorded, so it is not served")
    })?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        // The name is letters, digits, `-`, `_` and `.` alone.
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", copy.name),
        )
        .header(header::CONTENT_LENGTH, copy.size_bytes)
        .header(header::CACHE_CONTROL, "no-store")
        .body(axum::body::Body::from_stream(
            tokio_util::io::ReaderStream::new(file),
        ))
        .map_err(|error| {
            tracing::error!(error = ?error, "could not build a copy's download");
            Problem::new(ErrorCode::Internal, "the copy could not be served")
        })
}

/// Remove a copy.
///
/// # Errors
///
/// `NOT_FOUND`, `BACKUP_BUSY`, or `BACKUPS_UNAVAILABLE`.
#[utoipa::path(
    delete,
    path = "/api/v1/backups/{name}",
    tag = "backups",
    description = "Remove a copy from the backups volume, whoever took or added it. Audited \
                   as `backup.deleted`. Needs `manage_backups`.",
    params(("name" = String, Path, description = "The copy's name, as listed")),
    responses(
        (status = 204, description = "Removed"),
        (status = 404, description = "No copy is named that", body = Problem),
        (status = 409, description = "Something is under way", body = Problem),
        (status = 503, description = "This server keeps no copies, or cannot remove it", body = Problem),
    ),
)]
pub async fn delete_backup(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(name): Path<String>,
) -> Result<Response, Problem> {
    let backups = backups_of(&state)?;
    let copy = backups
        .delete(&name, &user.username)
        .await
        .map_err(problem_for)?;
    audit(
        state.database().pool(),
        Actor::User(&user.username),
        "backup.deleted",
        &copy.name,
        true,
        serde_json::json!({ "size_bytes": copy.size_bytes, "kind": copy.kind }),
    )
    .await
    .map_err(|error| {
        audit_failed(&error);
        unavailable("the copy was removed, but its audit record could not be written")
    })?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// The routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/backups", get(list_backups).merge(post(take_backup)))
        .route("/backups/uploads", put(upload_backup))
        .route("/backups/{name}/download", get(download_backup))
        .route("/backups/{name}", delete(delete_backup))
}
