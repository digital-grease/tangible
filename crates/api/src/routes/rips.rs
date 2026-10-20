// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Ripping the disc in a drive (ADR 0013).
//!
//! An operator asks for a rip of one drive's disc. The drive's worker takes
//! it, reads the disc, sends each file it read, and says it has finished;
//! the server hashes every byte itself, then queues an import of the files,
//! which the import runner handles as it does an upload. The rip follows
//! its import to the end.
//!
//! Reading changes nothing on the disc, so a rip can be withdrawn at any
//! point before its import starts; the worker learns of it from its next
//! report and stops.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tangible_burn::MediumInfo;
use tangible_db::rips::{
    CancelOutcome, ReceivedFile, RequestOutcome, RipEvent, RipEventRecord, RipRecord, RipResult,
    WorkerOutcome, cancel_rip, check_upload, claim_rip, complete_rip, drive_rip_engines, get_rip,
    list_rip_events, list_rips, record_rip_events, record_upload, request_rip,
};
use tangible_domain::rip::{CD_OUTPUTS, is_cd_output};
use tangible_domain::{DriveId, LogicalPath, RipJobId, RipState};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::io::AsyncWriteExt as _;
use utoipa::{IntoParams, ToSchema};

use crate::auth::SignedIn;
use crate::problem::{ErrorCode, Problem};
use crate::routes::burns::MediumView;
use crate::routes::workers::same_worker;
use crate::state::ApiState;
use crate::worker_auth::WorkerIdentity;

/// The largest file a CD rip may send. A 99-minute CD of raw 2352-byte
/// sectors is about 1.05 GB; this leaves room and no more.
pub const MAX_CD_RIP_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// The header a worker sends a file's SHA-256 in, for the server to check.
pub const CONTENT_SHA256: &str = "x-content-sha256";

/// Most rips listed at once.
const MAX_LIMIT: i64 = 200;

/// Rips listed when the request does not say.
const DEFAULT_LIMIT: i64 = 50;

/// Most events in one report.
const MAX_EVENT_BATCH: usize = 100;

/// Most events listed at once.
const MAX_EVENTS: i64 = 500;

/// Longest error code a worker may report, matching the column.
const MAX_ERROR_CODE: usize = 100;

/// Longest error detail kept, matching the column.
const MAX_ERROR_DETAIL: usize = 10_000;

fn rfc3339(value: OffsetDateTime) -> String {
    value.format(&Rfc3339).unwrap_or_default()
}

fn unavailable(what: &str) -> Problem {
    Problem::new(
        ErrorCode::StorageUnavailable,
        format!("{what}; see server logs"),
    )
}

fn database(what: &'static str) -> impl FnOnce(tangible_db::DbError) -> Problem {
    move |error| {
        tracing::error!(error = ?error, "rips: {what}");
        unavailable(what)
    }
}

fn parse_rip(raw: &str) -> Result<RipJobId, Problem> {
    raw.parse()
        .map_err(|_| Problem::invalid_parameter("rip_id", "not a valid identifier"))
}

fn parse_drive(raw: &str) -> Result<DriveId, Problem> {
    raw.parse()
        .map_err(|_| Problem::invalid_parameter("drive_id", "not a valid identifier"))
}

fn worker_refusal(id: RipJobId, outcome: WorkerOutcome) -> Problem {
    match outcome {
        WorkerOutcome::NotYours => Problem::not_found("rip", &id.to_string()),
        WorkerOutcome::Conflict(state) => Problem::new(
            ErrorCode::Conflict,
            format!("this rip is {state}, which does not accept that"),
        ),
        WorkerOutcome::MissingOutputs => Problem::new(
            ErrorCode::ValidationFailed,
            "the files named are not exactly the files that arrived",
        ),
    }
}

/// A stable code: uppercase letters, digits and underscores.
fn checked_code(code: &str, field: &str) -> Result<(), Problem> {
    let valid = !code.is_empty()
        && code.len() <= MAX_ERROR_CODE
        && code
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_');
    if valid {
        Ok(())
    } else {
        Err(Problem::new(
            ErrorCode::ValidationFailed,
            format!("{field} must be uppercase letters, digits and underscores"),
        ))
    }
}

/// A tool name or version: plain characters, short.
fn checked_token<'a>(value: Option<&'a str>, field: &str) -> Result<&'a str, Problem> {
    value
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 100
                && value
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
        })
        .ok_or_else(|| {
            Problem::new(
                ErrorCode::ValidationFailed,
                format!("{field} must be a plain name or version"),
            )
        })
}

// --- views -----------------------------------------------------------------------

/// One file a worker sent, as the server received it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RipFileView {
    /// Its name, such as `disc.bin`.
    pub name: String,
    /// Its length in bytes.
    pub bytes: i64,
    /// Its SHA-256, as the server computed it.
    pub sha256: String,
}

/// Sectors that gave trouble.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadErrors {
    /// Sectors that could not be read at all.
    pub unreadable_sectors: u64,
    /// Sectors that read only after retrying.
    pub retried_sectors: u64,
}

/// A request to rip the disc in a drive, and how it went.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RipView {
    /// Opaque identifier.
    pub id: String,
    /// The drive.
    pub drive_id: String,
    /// What the drive is called.
    pub drive_name: String,
    /// `queued`, `reading`, `uploading`, `registering`, `complete`,
    /// `canceled`, `refused` or `failed`.
    pub state: String,
    /// Whether it is still going.
    pub is_open: bool,
    /// Whether it can still be withdrawn.
    pub can_cancel: bool,
    /// Who asked.
    pub requested_by: String,
    /// What the worker found in the drive.
    pub medium: Option<MediumView>,
    /// The tool that read the disc, such as `cdrdao`.
    pub engine: Option<String>,
    /// Its version.
    pub engine_version: Option<String>,
    /// Sectors that gave trouble.
    pub read_errors: Option<ReadErrors>,
    /// What arrived.
    pub files: Vec<RipFileView>,
    /// Bytes received across every file.
    pub bytes_received: i64,
    /// The import of what was read, once queued.
    pub import_id: Option<String>,
    /// The artifact it became, once imported.
    pub artifact_id: Option<String>,
    /// Why it did not finish, when it did not.
    pub error_code: Option<String>,
    /// More about that.
    pub error_detail: Option<String>,
    /// When it was asked for.
    pub created_at: String,
    /// When the worker took it.
    pub started_at: Option<String>,
    /// When it ended.
    pub completed_at: Option<String>,
}

impl From<RipRecord> for RipView {
    fn from(record: RipRecord) -> Self {
        let mut files: Vec<RipFileView> = record
            .outputs
            .as_object()
            .into_iter()
            .flatten()
            .map(|(name, file)| RipFileView {
                name: name.clone(),
                bytes: file["bytes"].as_i64().unwrap_or_default(),
                sha256: file["sha256"].as_str().unwrap_or_default().to_owned(),
            })
            .collect();
        files.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            id: record.id.to_string(),
            drive_id: record.drive_id.to_string(),
            drive_name: record.drive_name,
            state: record.state.to_string(),
            is_open: record.state.is_open(),
            can_cancel: record.state.can_cancel(),
            requested_by: record.requested_by,
            medium: record
                .medium
                .and_then(|value| serde_json::from_value::<MediumInfo>(value).ok())
                .map(MediumView::from),
            engine: record.engine,
            engine_version: record.engine_version,
            read_errors: record
                .read_errors
                .and_then(|value| serde_json::from_value(value).ok()),
            files,
            bytes_received: record.bytes_received,
            import_id: record.import_job_id.map(|id| id.to_string()),
            artifact_id: record.artifact_id.map(|id| id.to_string()),
            error_code: record.error_code,
            error_detail: record.error_detail,
            created_at: rfc3339(record.created_at),
            started_at: record.started_at.map(rfc3339),
            completed_at: record.completed_at.map(rfc3339),
        }
    }
}

/// A page of rips, newest first.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RipPage {
    /// The rips.
    pub items: Vec<RipView>,
    /// Pass as `before` for the next page; absent on the last.
    pub next_cursor: Option<String>,
}

/// Something the worker said while ripping.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RipEventView {
    /// Its place in the worker's sequence.
    pub sequence: i64,
    /// What kind of event, such as `progress`.
    pub event_type: String,
    /// Its stable code.
    pub code: String,
    /// How far along, 0 to 1.
    pub progress: Option<f32>,
    /// What it said, when it said something.
    pub message: Option<String>,
    /// When the worker saw it.
    pub worker_time: String,
}

impl From<RipEventRecord> for RipEventView {
    fn from(event: RipEventRecord) -> Self {
        Self {
            sequence: event.sequence,
            event_type: event.event_type,
            code: event.message_code,
            progress: event.progress_fraction,
            message: event
                .data
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            worker_time: rfc3339(event.worker_timestamp),
        }
    }
}

/// A rip's events, in sequence.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RipEventPage {
    /// The events.
    pub items: Vec<RipEventView>,
}

// --- requests ----------------------------------------------------------------------

/// What an operator sends to rip the disc in a drive. Empty for now: CD
/// rips have no options, and decryption arrives later (ADR 0013).
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestRip {}

/// Paging for the rip list.
#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RipListQuery {
    /// How many, at most 200. Defaults to 50.
    pub limit: Option<i64>,
    /// The `next_cursor` of the page before.
    pub before: Option<String>,
}

/// Paging for a rip's events.
#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RipEventQuery {
    /// Only events after this sequence number.
    pub after: Option<i64>,
}

/// What a worker sends to ask for a rip.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RipClaimRequest {
    /// The worker's drive.
    pub drive_id: String,
}

/// A rip a worker has taken.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RipClaim {
    /// The rip.
    pub rip_id: String,
    /// The files a CD rip sends, by these names and no others.
    pub outputs: Vec<String>,
    /// The largest file the server accepts.
    pub max_file_bytes: u64,
}

/// One thing a worker says while ripping.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RipEventSubmission {
    /// Monotonic within the rip.
    pub sequence: i64,
    /// What kind of event, such as `progress` or `medium_inspected`.
    pub event_type: String,
    /// Its stable code.
    pub code: String,
    /// How far along, 0 to 1.
    pub progress: Option<f32>,
    /// Structured detail, such as `{"message": "..."}`.
    #[serde(default)]
    pub data: serde_json::Value,
    /// When the worker saw it, RFC 3339.
    pub worker_time: String,
}

/// Events a worker reports.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RipEventBatch {
    /// In sequence order.
    pub events: Vec<RipEventSubmission>,
}

/// What the server answers a worker's report with.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RipEventAck {
    /// The rip's state, so a worker reading a rip that was withdrawn stops.
    pub state: String,
}

/// How a rip ended, as the worker reports it.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RipCompletion {
    /// `read`, `refused` or `failed`.
    pub outcome: String,
    /// For `read`: the tool that read the disc, such as `cdrdao`.
    pub engine: Option<String>,
    /// For `read`: its version.
    pub engine_version: Option<String>,
    /// What was in the drive, when there was a disc to look at.
    #[schema(value_type = Option<Object>)]
    pub medium: Option<MediumInfo>,
    /// For `read`: sectors that gave trouble.
    pub read_errors: Option<ReadErrors>,
    /// For `read`: the files that make up the image, all already sent.
    #[serde(default)]
    pub outputs: Vec<String>,
    /// For `refused` and `failed`: why, as a stable code such as
    /// `NO_MEDIUM`.
    pub error_code: Option<String>,
    /// More about that.
    pub error_detail: Option<String>,
}

// --- for people --------------------------------------------------------------------

/// Ask for the disc in a drive to be ripped into the library.
///
/// # Errors
///
/// `NOT_FOUND` for an unknown drive; `CONFLICT` when the drive already has a
/// rip reading, or its worker cannot rip.
#[utoipa::path(
    post,
    path = "/api/v1/drives/{drive_id}/rips",
    tag = "rips",
    description = "Rip the disc in a drive into the library. The drive's worker \
                   reads it when the drive is free, sends what it read, and the \
                   result is imported like an upload. A CD becomes a TOC/BIN image.",
    params(("drive_id" = String, Path, description = "Drive identifier")),
    request_body = RequestRip,
    responses(
        (status = 201, description = "Queued", body = RipView),
        (status = 404, description = "No such drive", body = Problem),
        (status = 409, description = "A rip is already reading, or the worker cannot rip", body = Problem),
    ),
)]
pub async fn request_drive_rip(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(drive_id): Path<String>,
    Json(_request): Json<RequestRip>,
) -> Result<Response, Problem> {
    let drive_id = parse_drive(&drive_id)?;
    let pool = state.database().pool();
    if state.imports().is_none() {
        return Err(unavailable("importing is not configured on this server"));
    }
    if drive_rip_engines(pool, drive_id)
        .await
        .map_err(database("the drive could not be read"))?
        .is_none()
    {
        // A missing drive also has no engines; say which it is.
        let exists = tangible_db::erasures::get_drive(pool, drive_id)
            .await
            .map_err(database("the drive could not be read"))?
            .is_some();
        return Err(if exists {
            Problem::new(
                ErrorCode::Conflict,
                "this drive's worker has not said it can rip; update the worker",
            )
        } else {
            Problem::not_found("drive", &drive_id.to_string())
        });
    }
    let record = request_rip(pool, drive_id, &user.username)
        .await
        .map_err(database("the rip could not be queued"))?
        .map_err(|outcome| match outcome {
            RequestOutcome::DriveMissing => Problem::not_found("drive", &drive_id.to_string()),
            RequestOutcome::AlreadyOpen => Problem::new(
                ErrorCode::Conflict,
                "this drive already has a rip that has not finished reading",
            ),
        })?;
    tracing::info!(rip_id = %record.id, %drive_id, by = %user.username, "rip requested");
    Ok((StatusCode::CREATED, Json(RipView::from(record))).into_response())
}

/// Rips, newest first.
///
/// # Errors
///
/// `INVALID_PARAMETER` for a limit out of range or a malformed cursor.
#[utoipa::path(
    get,
    path = "/api/v1/rips",
    tag = "rips",
    params(RipListQuery),
    responses((status = 200, description = "Newest first", body = RipPage)),
)]
pub async fn list_all_rips(
    State(state): State<ApiState>,
    Query(query): Query<RipListQuery>,
) -> Result<Json<RipPage>, Problem> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(Problem::invalid_parameter("limit", "must be from 1 to 200"));
    }
    let before = query
        .before
        .as_deref()
        .map(|raw| {
            raw.parse::<RipJobId>().map_err(|_| {
                Problem::invalid_parameter("before", "not a cursor this server issued")
            })
        })
        .transpose()?;
    // One more than asked, to know whether there is a next page.
    let mut items = list_rips(state.database().pool(), limit + 1, before)
        .await
        .map_err(database("rips could not be read"))?;
    let more = items.len() > usize::try_from(limit).unwrap_or(usize::MAX);
    items.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    let next_cursor = more
        .then(|| items.last().map(|rip| rip.id.to_string()))
        .flatten();
    Ok(Json(RipPage {
        items: items.into_iter().map(RipView::from).collect(),
        next_cursor,
    }))
}

/// One rip.
///
/// # Errors
///
/// `NOT_FOUND` for an unknown rip.
#[utoipa::path(
    get,
    path = "/api/v1/rips/{rip_id}",
    tag = "rips",
    params(("rip_id" = String, Path, description = "Rip identifier")),
    responses(
        (status = 200, description = "The rip", body = RipView),
        (status = 404, description = "No such rip", body = Problem),
    ),
)]
pub async fn get_one_rip(
    State(state): State<ApiState>,
    Path(rip_id): Path<String>,
) -> Result<Json<RipView>, Problem> {
    let id = parse_rip(&rip_id)?;
    get_rip(state.database().pool(), id)
        .await
        .map_err(database("the rip could not be read"))?
        .map(|record| Json(RipView::from(record)))
        .ok_or_else(|| Problem::not_found("rip", &id.to_string()))
}

/// What the worker said while ripping.
///
/// # Errors
///
/// `NOT_FOUND` for an unknown rip.
#[utoipa::path(
    get,
    path = "/api/v1/rips/{rip_id}/events",
    tag = "rips",
    params(("rip_id" = String, Path, description = "Rip identifier"), RipEventQuery),
    responses(
        (status = 200, description = "In sequence", body = RipEventPage),
        (status = 404, description = "No such rip", body = Problem),
    ),
)]
pub async fn list_events_of_rip(
    State(state): State<ApiState>,
    Path(rip_id): Path<String>,
    Query(query): Query<RipEventQuery>,
) -> Result<Json<RipEventPage>, Problem> {
    let id = parse_rip(&rip_id)?;
    let pool = state.database().pool();
    if get_rip(pool, id)
        .await
        .map_err(database("the rip could not be read"))?
        .is_none()
    {
        return Err(Problem::not_found("rip", &id.to_string()));
    }
    let items = list_rip_events(pool, id, query.after, MAX_EVENTS)
        .await
        .map_err(database("the rip's events could not be read"))?;
    Ok(Json(RipEventPage {
        items: items.into_iter().map(RipEventView::from).collect(),
    }))
}

/// Withdraw a rip before its import starts.
///
/// # Errors
///
/// `NOT_FOUND` for an unknown rip; `CONFLICT` once its import has started.
#[utoipa::path(
    post,
    path = "/api/v1/rips/{rip_id}/cancel",
    tag = "rips",
    description = "Withdraw a rip. Allowed while it is queued, reading or sending: \
                   reading changes nothing on the disc. Anything already sent is \
                   discarded.",
    params(("rip_id" = String, Path, description = "Rip identifier")),
    responses(
        (status = 200, description = "Withdrawn", body = RipView),
        (status = 404, description = "No such rip", body = Problem),
        (status = 409, description = "Its import has started, or it has ended", body = Problem),
    ),
)]
pub async fn cancel_one_rip(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(rip_id): Path<String>,
) -> Result<Json<RipView>, Problem> {
    let id = parse_rip(&rip_id)?;
    let record = cancel_rip(state.database().pool(), id, &user.username)
        .await
        .map_err(database("the rip could not be withdrawn"))?
        .map_err(|outcome| match outcome {
            CancelOutcome::NotFound => Problem::not_found("rip", &id.to_string()),
            CancelOutcome::TooLate(state) => Problem::new(
                ErrorCode::Conflict,
                format!("this rip is {state} and can no longer be withdrawn"),
            ),
        })?;
    discard_staged(&state, &record).await;
    Ok(Json(RipView::from(record)))
}

/// Remove whatever a rip that will not be imported had sent.
async fn discard_staged(state: &ApiState, record: &RipRecord) {
    if let Some(imports) = state.imports()
        && let Err(error) = imports.pipeline.discard(record.staging_id).await
    {
        tracing::warn!(rip_id = %record.id, error = ?error, "could not discard a rip's files");
    }
}

// --- for workers ------------------------------------------------------------------

/// Ask for a rip queued for this worker's drive.
///
/// # Errors
///
/// `NOT_FOUND` if the path names another worker or the drive is not this
/// worker's.
#[utoipa::path(
    post,
    path = "/api/v1/workers/{worker_id}/rip-claims",
    tag = "rips",
    description = "Take the next rip queued for this worker's drive. Answers 204 \
                   when there is none, or while a burn or an erasure holds the drive. \
                   Asked after erasure claims and before burn claims.",
    params(("worker_id" = String, Path, description = "Worker identifier")),
    request_body = RipClaimRequest,
    responses(
        (status = 200, description = "Taken", body = RipClaim),
        (status = 204, description = "Nothing to rip"),
        (status = 404, description = "Not this worker, or not its drive", body = Problem),
    ),
    security(("worker_credential" = [])),
)]
pub async fn claim_drive_rip(
    State(state): State<ApiState>,
    identity: WorkerIdentity,
    Path(worker_id): Path<String>,
    Json(request): Json<RipClaimRequest>,
) -> Result<Response, Problem> {
    let worker_id = same_worker(identity, &worker_id)?;
    let drive_id = parse_drive(&request.drive_id)?;
    let claimed = claim_rip(state.database().pool(), worker_id, drive_id)
        .await
        .map_err(database("rips could not be read"))?
        .map_err(|_| Problem::not_found("drive", &drive_id.to_string()))?;
    Ok(match claimed {
        None => StatusCode::NO_CONTENT.into_response(),
        Some(record) => {
            tracing::info!(rip_id = %record.id, %worker_id, %drive_id, "rip taken");
            Json(RipClaim {
                rip_id: record.id.to_string(),
                outputs: CD_OUTPUTS.iter().map(|name| (*name).to_owned()).collect(),
                max_file_bytes: MAX_CD_RIP_FILE_BYTES,
            })
            .into_response()
        }
    })
}

/// Report what is happening in a rip.
///
/// # Errors
///
/// `NOT_FOUND` if the rip is not on this worker's drive; `CONFLICT` once it
/// is no longer reading or sending, which is how a worker learns that it was
/// withdrawn; `INVALID_PARAMETER` for a malformed event.
#[utoipa::path(
    post,
    path = "/api/v1/rips/{rip_id}/events",
    tag = "rips",
    description = "Report events. Idempotent by sequence. A 409 means the rip was \
                   withdrawn or has ended: stop reading.",
    params(("rip_id" = String, Path, description = "Rip identifier")),
    request_body = RipEventBatch,
    responses(
        (status = 200, description = "Recorded", body = RipEventAck),
        (status = 404, description = "Not this worker's rip", body = Problem),
        (status = 409, description = "Withdrawn or ended", body = Problem),
    ),
    security(("worker_credential" = [])),
)]
pub async fn submit_rip_events(
    State(state): State<ApiState>,
    identity: WorkerIdentity,
    Path(rip_id): Path<String>,
    Json(batch): Json<RipEventBatch>,
) -> Result<Json<RipEventAck>, Problem> {
    let id = parse_rip(&rip_id)?;
    if batch.events.len() > MAX_EVENT_BATCH {
        return Err(Problem::invalid_parameter(
            "events",
            &format!("at most {MAX_EVENT_BATCH} events per request"),
        ));
    }
    let mut times = Vec::with_capacity(batch.events.len());
    for event in &batch.events {
        checked_code(&event.code, "code")?;
        if event.event_type.is_empty() || event.event_type.len() > 100 || event.sequence < 0 {
            return Err(Problem::invalid_parameter(
                "events",
                "each needs a sequence of 0 or more and a short event_type",
            ));
        }
        times.push(
            OffsetDateTime::parse(&event.worker_time, &Rfc3339).map_err(|_| {
                Problem::invalid_parameter("worker_time", "not an RFC 3339 timestamp")
            })?,
        );
    }
    let events: Vec<RipEvent<'_>> = batch
        .events
        .iter()
        .zip(times)
        .map(|(event, worker_timestamp)| RipEvent {
            sequence: event.sequence,
            event_type: &event.event_type,
            message_code: &event.code,
            progress_fraction: event.progress.map(|value| value.clamp(0.0, 1.0)),
            data: &event.data,
            worker_timestamp,
        })
        .collect();
    let rip_state = record_rip_events(state.database().pool(), identity.worker_id, id, &events)
        .await
        .map_err(database("the events could not be recorded"))?
        .map_err(|outcome| worker_refusal(id, outcome))?;
    Ok(Json(RipEventAck {
        state: rip_state.to_string(),
    }))
}

/// Send one file a rip read.
///
/// # Errors
///
/// `NOT_FOUND` if the rip is not on this worker's drive; `CONFLICT` once it
/// no longer accepts files; `VALIDATION_FAILED` for a name a rip does not
/// send, a missing or malformed digest or length, a file too large, or bytes
/// that do not match what the worker said.
#[utoipa::path(
    put,
    path = "/api/v1/rips/{rip_id}/files/{name}",
    tag = "rips",
    description = "Send one file a rip read, as the request body, with its length in \
                   Content-Length and its SHA-256 in X-Content-SHA256. The server \
                   hashes every byte and refuses a file that does not match. Sending \
                   a file again replaces it.",
    params(
        ("rip_id" = String, Path, description = "Rip identifier"),
        ("name" = String, Path, description = "The file, such as disc.bin"),
    ),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses(
        (status = 200, description = "Received and checked", body = RipView),
        (status = 404, description = "Not this worker's rip", body = Problem),
        (status = 409, description = "No longer accepting files", body = Problem),
        (status = 422, description = "A name, length or digest that will not do", body = Problem),
    ),
    security(("worker_credential" = [])),
)]
pub async fn upload_rip_file(
    State(state): State<ApiState>,
    identity: WorkerIdentity,
    Path((rip_id, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Result<Json<RipView>, Problem> {
    let id = parse_rip(&rip_id)?;
    if !is_cd_output(&name) {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            format!("a CD rip sends only {}", CD_OUTPUTS.join(" and ")),
        ));
    }
    let claimed = headers
        .get(CONTENT_SHA256)
        .and_then(|value| value.to_str().ok())
        .filter(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .ok_or_else(|| {
            Problem::new(
                ErrorCode::ValidationFailed,
                "X-Content-SHA256 must be the file's SHA-256 in lowercase hex",
            )
        })?
        .to_owned();
    let length = headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| Problem::new(ErrorCode::ValidationFailed, "Content-Length is required"))?;
    if length > MAX_CD_RIP_FILE_BYTES {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            format!("a CD rip's files are at most {MAX_CD_RIP_FILE_BYTES} bytes"),
        ));
    }
    let Some(imports) = state.imports() else {
        return Err(unavailable("importing is not configured on this server"));
    };
    let pool = state.database().pool();
    let record = check_upload(pool, identity.worker_id, id)
        .await
        .map_err(database("the rip could not be read"))?
        .map_err(|outcome| worker_refusal(id, outcome))?;

    let logical = LogicalPath::parse(&name)
        .map_err(|_| Problem::new(ErrorCode::ValidationFailed, "not a valid file name"))?;
    let area = imports
        .pipeline
        .open_area(record.staging_id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not open a rip's staging area");
            unavailable("the file could not be staged")
        })?;
    let target = area.path().join(logical.as_str());
    let received = stream_to(&area, &logical, body, length).await;
    let (digest, bytes) = match received {
        Ok(done) => done,
        Err(problem) => {
            let _ = tokio::fs::remove_file(&target).await;
            return Err(problem);
        }
    };
    if bytes != length || digest != claimed {
        let _ = tokio::fs::remove_file(&target).await;
        tracing::warn!(rip_id = %id, file = %name, "a rip's file did not match what the worker said");
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            format!(
                "UPLOAD_MISMATCH: received {bytes} bytes with SHA-256 {digest}, not \
                 {length} bytes with {claimed}"
            ),
        ));
    }

    let bytes_i64 = i64::try_from(bytes).unwrap_or(i64::MAX);
    let recorded = record_upload(
        pool,
        identity.worker_id,
        id,
        &ReceivedFile {
            name: &name,
            sha256: &digest,
            bytes: bytes_i64,
        },
    )
    .await
    .map_err(database("the file could not be recorded"))?;
    match recorded {
        Ok(record) => Ok(Json(RipView::from(record))),
        Err(outcome) => {
            // Withdrawn while the bytes were arriving.
            let _ = tokio::fs::remove_file(&target).await;
            Err(worker_refusal(id, outcome))
        }
    }
}

/// Stream a request body into a staged file, hashing it, refusing more than
/// `length` bytes. Durable before it returns.
async fn stream_to(
    area: &tangible_storage::StagingArea,
    logical: &LogicalPath,
    body: axum::body::Body,
    length: u64,
) -> Result<(String, u64), Problem> {
    use futures_util::StreamExt as _;

    let staging = |error: &dyn std::fmt::Debug| {
        tracing::error!(error = ?error, "could not stage a rip's file");
        unavailable("the file could not be staged")
    };
    let mut file = area.create_file(logical).await.map_err(|e| staging(&e))?;
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    let mut stream = std::pin::pin!(body.into_data_stream());
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            tracing::warn!(error = ?error, "a rip's file upload was interrupted");
            Problem::new(
                ErrorCode::ValidationFailed,
                "the file ended before it was complete",
            )
        })?;
        received += chunk.len() as u64;
        if received > length {
            return Err(Problem::new(
                ErrorCode::ValidationFailed,
                "the file is longer than its Content-Length",
            ));
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(|e| staging(&e))?;
    }
    file.flush().await.map_err(|e| staging(&e))?;
    file.sync_all().await.map_err(|e| staging(&e))?;
    Ok((hex::encode(hasher.finalize()), received))
}

/// Report how a rip ended.
///
/// # Errors
///
/// `NOT_FOUND` if the rip is not on this worker's drive; `CONFLICT` if it is
/// not in a state that can end that way; `VALIDATION_FAILED` for an outcome
/// or field that will not do, or files named that did not all arrive.
#[utoipa::path(
    post,
    path = "/api/v1/rips/{rip_id}/complete",
    tag = "rips",
    description = "Report how a rip ended: read (every file named must have been sent, \
                   and its import is queued), refused or failed. Repeating the same \
                   report is accepted.",
    params(("rip_id" = String, Path, description = "Rip identifier")),
    request_body = RipCompletion,
    responses(
        (status = 200, description = "Recorded", body = RipView),
        (status = 404, description = "Not this worker's rip", body = Problem),
        (status = 409, description = "Not in a state that can end that way", body = Problem),
        (status = 422, description = "An outcome or field that will not do", body = Problem),
    ),
    security(("worker_credential" = [])),
)]
pub async fn complete_drive_rip(
    State(state): State<ApiState>,
    identity: WorkerIdentity,
    Path(rip_id): Path<String>,
    Json(report): Json<RipCompletion>,
) -> Result<Json<RipView>, Problem> {
    let id = parse_rip(&rip_id)?;
    let medium = report
        .medium
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| Problem::new(ErrorCode::ValidationFailed, "medium could not be read"))?;
    let read_errors = report
        .read_errors
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| Problem::new(ErrorCode::ValidationFailed, "read_errors could not be read"))?;
    let detail: Option<String> = report
        .error_detail
        .as_deref()
        .map(|detail| detail.chars().take(MAX_ERROR_DETAIL).collect());

    let result = match report.outcome.as_str() {
        "read" => {
            if let Some(name) = report.outputs.iter().find(|name| !is_cd_output(name)) {
                return Err(Problem::new(
                    ErrorCode::ValidationFailed,
                    format!("{name} is not a file a CD rip sends"),
                ));
            }
            RipResult::Read {
                engine: checked_token(report.engine.as_deref(), "engine")?,
                engine_version: checked_token(report.engine_version.as_deref(), "engine_version")?,
                medium: medium.as_ref(),
                read_errors: read_errors.as_ref(),
                outputs: &report.outputs,
            }
        }
        "refused" | "failed" => {
            let code = report.error_code.as_deref().ok_or_else(|| {
                Problem::new(
                    ErrorCode::ValidationFailed,
                    "error_code is required when a rip is refused or failed",
                )
            })?;
            checked_code(code, "error_code")?;
            if report.outcome == "refused" {
                RipResult::Refused {
                    code,
                    detail: detail.as_deref(),
                    medium: medium.as_ref(),
                }
            } else {
                RipResult::Failed {
                    code,
                    detail: detail.as_deref(),
                }
            }
        }
        _ => {
            return Err(Problem::new(
                ErrorCode::ValidationFailed,
                "outcome must be read, refused or failed",
            ));
        }
    };

    let record = complete_rip(state.database().pool(), identity.worker_id, id, &result)
        .await
        .map_err(database("the rip could not be recorded"))?
        .map_err(|outcome| worker_refusal(id, outcome))?;
    if matches!(record.state, RipState::Refused | RipState::Failed) {
        discard_staged(&state, &record).await;
    }
    tracing::info!(
        rip_id = %record.id,
        state = %record.state,
        error_code = ?record.error_code,
        "rip reported"
    );
    Ok(Json(RipView::from(record)))
}

/// The rip routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/drives/{drive_id}/rips", post(request_drive_rip))
        .route("/rips", get(list_all_rips))
        .route("/rips/{rip_id}", get(get_one_rip))
        .route(
            "/rips/{rip_id}/events",
            get(list_events_of_rip).post(submit_rip_events),
        )
        .route("/rips/{rip_id}/cancel", post(cancel_one_rip))
        .route("/rips/{rip_id}/files/{name}", put(upload_rip_file))
        .route("/rips/{rip_id}/complete", post(complete_drive_rip))
        .route("/workers/{worker_id}/rip-claims", post(claim_drive_rip))
}
