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
use serde::{Deserialize, Serialize};
use tangible_db::backups::{Actor, audit};
use time::OffsetDateTime;
use utoipa::ToSchema;

use crate::auth::SignedIn;
use crate::backups::{BackupError, Backups, CopyEntry, Operation, Outcome, ToolsStatus};
use crate::problem::{ErrorCode, Problem};
use crate::restoring::{self, Progress, Refusal, Step};
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
    /// When copies are taken by themselves.
    pub schedule: ScheduleView,
    /// The last restore this database records: the one that made it, or
    /// one that failed and left it as it was.
    pub last_restore: Option<RestoreView>,
    /// Whether this server restores copies itself.
    pub can_restore: bool,
}

/// A restore, as the database records it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RestoreView {
    /// Its identifier.
    pub id: uuid::Uuid,
    /// Who asked.
    pub requested_by: String,
    /// The copy restored.
    pub copy_name: String,
    /// The copy of the database as it was, taken first: the way back.
    pub safety_copy: Option<String>,
    /// `restored`, or `failed`, which left the database as it was.
    pub outcome: String,
    /// Why it failed.
    pub error: Option<String>,
    /// The restore check's report: what the library lacks, artifacts it has
    /// that the copy does not name, and work the copy recorded as under way.
    pub report: Option<serde_json::Value>,
    /// When it began.
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    /// When it ended.
    #[serde(with = "time::serde::rfc3339")]
    pub finished_at: OffsetDateTime,
}

impl From<tangible_db::restores::RestoreRecord> for RestoreView {
    fn from(record: tangible_db::restores::RestoreRecord) -> Self {
        Self {
            id: record.id,
            requested_by: record.requested_by,
            copy_name: record.copy_name,
            safety_copy: record.safety_copy,
            outcome: record.outcome,
            error: record.error,
            report: record.report,
            started_at: record.started_at,
            finished_at: record.finished_at,
        }
    }
}

/// Restoring a copy: what confirms it.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RestoreRequest {
    /// The copy's name, typed again.
    pub confirm: String,
}

/// Whether a restore is under way: what anyone may see while the server
/// answers nothing else.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RestoreStatus {
    /// Whether one is.
    pub restoring: bool,
    /// Where it is.
    pub progress: Option<Progress>,
}

/// What cataloguing the artifacts a restore found did.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RegisteredView {
    /// How many manifests the library has that the catalogue did not name.
    pub uncatalogued: usize,
    /// How many of those were catalogued now.
    pub registered: usize,
    /// The restore check's report after.
    pub report: serde_json::Value,
}

/// When the server takes a copy by itself: every day at a time in UTC.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ScheduleView {
    /// Whether it does.
    pub enabled: bool,
    /// The time each day, `HH:MM` in UTC.
    pub time: String,
    /// When the next is due, while it is enabled and copies are kept.
    #[serde(with = "time::serde::rfc3339::option")]
    pub next_at: Option<OffsetDateTime>,
    /// Who last changed it; `null` while it is the default, 03:00.
    pub updated_by: Option<String>,
    /// When.
    #[serde(with = "time::serde::rfc3339::option")]
    pub updated_at: Option<OffsetDateTime>,
}

/// A new schedule.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewSchedule {
    /// Whether copies are taken by themselves.
    pub enabled: bool,
    /// The time each day, `HH:MM` in UTC, `00:00` to `23:59`.
    pub time: String,
}

fn schedule_view(schedule: &tangible_db::backups::Schedule, kept: u32) -> ScheduleView {
    ScheduleView {
        enabled: schedule.enabled,
        time: format!(
            "{:02}:{:02}",
            schedule.minute_of_day / 60,
            schedule.minute_of_day % 60
        ),
        next_at: (schedule.enabled && kept > 0)
            .then(|| crate::backups::next_due(OffsetDateTime::now_utc(), schedule.minute_of_day)),
        updated_by: schedule.updated_by.clone(),
        updated_at: schedule.updated_at,
    }
}

/// The minutes past midnight of `HH:MM`.
fn minute_of_day(time: &str) -> Option<u16> {
    let (hours, minutes) = time.split_once(':')?;
    if hours.len() != 2 || minutes.len() != 2 {
        return None;
    }
    let hours: u16 = hours.parse().ok()?;
    let minutes: u16 = minutes.parse().ok()?;
    (hours < 24 && minutes < 60).then_some(hours * 60 + minutes)
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
    let schedule = tangible_db::backups::schedule(state.database().pool())
        .await
        .map_err(|error| problem_for(error.into()))?;
    let last_restore = tangible_db::restores::last_restore(state.database().pool())
        .await
        .map_err(|error| problem_for(error.into()))?;
    Ok(BackupsView {
        copies: listing.copies,
        kept: backups.settings().kept,
        writable: listing.writable,
        tools: backups.tools(state.database()).await,
        operation: backups.operation(),
        last_outcome: backups.last_outcome(),
        max_upload_bytes: backups.settings().max_upload_bytes,
        schedule: schedule_view(&schedule, backups.settings().kept),
        last_restore: last_restore.map(RestoreView::from),
        can_restore: state.restore_orders().is_some(),
    })
}

fn refusal_problem(refusal: Refusal) -> Problem {
    match refusal {
        Refusal::Backup(error) => problem_for(error),
        Refusal::NotConfirmed(_) => Problem::new(ErrorCode::ValidationFailed, refusal.to_string()),
        Refusal::NotPermitted | Refusal::NameTooLong { .. } => {
            Problem::new(ErrorCode::BackupRestoreNotPermitted, refusal.to_string())
        }
        Refusal::WorkUnderWay(detail) => Problem::new(ErrorCode::BackupWorkUnderWay, detail),
        Refusal::NotRestorable(detail) => Problem::new(ErrorCode::BackupNotRestorable, detail),
    }
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

/// Set when copies are taken by themselves.
///
/// # Errors
///
/// `INVALID_PARAMETER` for a time that is not `HH:MM`; `BACKUPS_UNAVAILABLE`.
#[utoipa::path(
    put,
    path = "/api/v1/backups/schedule",
    tag = "backups",
    description = "Take a copy every day at `time` (`HH:MM`, UTC), or none when `enabled` \
                   is false. Copies are taken at 03:00 unless set; they count towards \
                   `kept` with the rest. A time missed while the server is down is not made \
                   up: the next start takes one. Audited as `backup.schedule_changed`. Needs \
                   `manage_backups`.",
    request_body = NewSchedule,
    responses(
        (status = 200, description = "The schedule", body = ScheduleView),
        (status = 400, description = "A time that is not HH:MM", body = Problem),
        (status = 503, description = "This server keeps no copies", body = Problem),
    ),
)]
pub async fn set_backup_schedule(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Json(request): Json<NewSchedule>,
) -> Result<Json<ScheduleView>, Problem> {
    let backups = backups_of(&state)?;
    let minute = minute_of_day(&request.time).ok_or_else(|| {
        Problem::invalid_parameter("time", "must be HH:MM in UTC, from 00:00 to 23:59")
    })?;
    let schedule = tangible_db::backups::set_schedule(
        state.database().pool(),
        request.enabled,
        minute,
        &user.username,
    )
    .await
    .map_err(|error| problem_for(error.into()))?;
    backups.schedule_changed();
    Ok(Json(schedule_view(&schedule, backups.settings().kept)))
}

/// Restore the database from a copy.
///
/// # Errors
///
/// `NOT_FOUND`; `VALIDATION_FAILED` when `confirm` is not the copy's name;
/// `BACKUP_WORK_UNDER_WAY`; `BACKUP_NOT_RESTORABLE`; `BACKUP_BUSY`;
/// `BACKUP_RESTORE_NOT_PERMITTED`; `BACKUPS_UNAVAILABLE`.
#[utoipa::path(
    post,
    path = "/api/v1/backups/{name}/restore",
    tag = "backups",
    description = "Replace the database with the copy: everything since it was taken goes \
                   from the catalogue, and everyone signs in again. A copy of the database \
                   as it is is taken first, the way back. The copy's definitions are never \
                   run: the schema is this version's own, and only the copy's rows are \
                   loaded, by a role that can do nothing else, then checked. Refused while a \
                   burn, rip, erasure, import or derivation is under way, for a copy a later \
                   version took or not of a Tangible database, and unless `confirm` is the \
                   copy's name. Then every API route but `GET /api/v1/backups/restore` \
                   answers 503 `SERVER_RESTORING` until the server starts again on the \
                   restored database, or on the one it left as it was if the restore failed; \
                   either way the database records it (`last_restore`), audited as \
                   `database.restored` or `database.restore_failed`. Needs `manage_backups`.",
    params(("name" = String, Path, description = "The copy's name, as listed")),
    request_body = RestoreRequest,
    responses(
        (status = 202, description = "Restoring", body = Progress),
        (status = 404, description = "No copy is named that", body = Problem),
        (status = 409, description = "Work under way, something else under way, or the database user cannot restore here", body = Problem),
        (status = 422, description = "Not confirmed, or a copy this version cannot restore", body = Problem),
        (status = 503, description = "This server cannot restore copies", body = Problem),
    ),
)]
pub async fn restore_backup(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(name): Path<String>,
    Json(request): Json<RestoreRequest>,
) -> Result<(StatusCode, Json<Progress>), Problem> {
    let backups = backups_of(&state)?;
    let Some(orders) = state.restore_orders() else {
        return Err(unavailable(
            "this server does not restore copies itself; restore at the machine \
             (deploy/README.md, \"Backups\")",
        ));
    };
    let order = restoring::prepare(&state, backups, &name, &request.confirm, &user.username)
        .await
        .map_err(refusal_problem)?;
    tracing::warn!(copy = %order.copy.name, by = %user.username, "restoring the database from a copy");
    state.maintenance().step(Step::Stopping);
    // Every open event stream ends; new ones are refused with the rest.
    state.events().close();
    if orders.send(order).await.is_err() {
        state.maintenance().end();
        return Err(unavailable("the server could not begin the restore"));
    }
    let progress = state
        .maintenance()
        .current()
        .ok_or_else(|| unavailable("the restore ended at once"))?;
    Ok((StatusCode::ACCEPTED, Json(progress)))
}

/// Whether a restore is under way.
#[utoipa::path(
    get,
    path = "/api/v1/backups/restore",
    tag = "backups",
    description = "Whether the database is being restored, and how far it has got: the one \
                   route that answers while it is. Open to anyone, and names nothing.",
    responses((status = 200, description = "Whether one is", body = RestoreStatus)),
)]
pub async fn restore_status(State(state): State<ApiState>) -> Json<RestoreStatus> {
    let progress = state.maintenance().current();
    Json(RestoreStatus {
        restoring: progress.is_some(),
        progress,
    })
}

/// Catalogue again the artifacts the library has and the catalogue lacks.
///
/// # Errors
///
/// `STORAGE_UNAVAILABLE` when there is no library, or it cannot be read.
#[utoipa::path(
    post,
    path = "/api/v1/backups/register-missing",
    tag = "backups",
    description = "After a restore, catalogue again each artifact the library has a manifest \
                   for and the catalogue does not name, imported after the copy was taken, \
                   once every object it names is there at its size: `tangible restore-check \
                   --register-missing`. Their imports' history and burns since are not in the \
                   copy, and stay lost. Each is audited as `artifact.recatalogued`. Needs \
                   `manage_backups`.",
    responses(
        (status = 200, description = "What was catalogued", body = RegisteredView),
        (status = 503, description = "No library, or it cannot be read", body = Problem),
    ),
)]
pub async fn register_missing(
    State(state): State<ApiState>,
) -> Result<Json<RegisteredView>, Problem> {
    let manifests = state.manifests().ok_or_else(|| {
        Problem::new(
            ErrorCode::StorageUnavailable,
            "this server has no library configured",
        )
    })?;
    let report = crate::restore_check::restore_check(
        state.database(),
        manifests.objects(),
        manifests,
        crate::restore_check::RestoreCheckSettings {
            register: true,
            list: 50,
        },
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "the restore check could not run");
        Problem::new(
            ErrorCode::StorageUnavailable,
            "the library or catalogue could not be read",
        )
    })?;
    Ok(Json(RegisteredView {
        uncatalogued: report.uncatalogued.count,
        registered: report.registered,
        report: serde_json::to_value(&report).unwrap_or_default(),
    }))
}

/// The routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/backups", get(list_backups).merge(post(take_backup)))
        .route("/backups/uploads", put(upload_backup))
        .route("/backups/schedule", put(set_backup_schedule))
        .route("/backups/restore", get(restore_status))
        .route("/backups/register-missing", post(register_missing))
        .route("/backups/{name}/restore", post(restore_backup))
        .route("/backups/{name}/download", get(download_backup))
        .route("/backups/{name}", delete(delete_backup))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_time_is_hours_and_minutes_in_the_day() {
        assert_eq!(minute_of_day("03:00"), Some(180));
        assert_eq!(minute_of_day("00:00"), Some(0));
        assert_eq!(minute_of_day("23:59"), Some(1439));
        for refused in [
            "24:00", "3:00", "03:60", "03:00:00", "0300", "", "aa:bb", "-1:00",
        ] {
            assert_eq!(minute_of_day(refused), None, "{refused}");
        }
    }
}
