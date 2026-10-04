// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Drives, and erasing the rewritable disc in one.
//!
//! A burn never erases: it refuses a disc that is not blank. Erasing is this
//! separate operation, which names one drive, needs a role allowed to erase,
//! must say in the request that the data loss is intended, and is audited
//! from request to result. The drive's worker takes it before any burn work,
//! decides from what is actually in the drive whether there is anything to
//! erase, and reports what it found and what it did.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use tangible_db::erasures::{
    CancelOutcome, DriveRecord, ErasureRecord, ErasureResult, RequestOutcome, WorkerOutcome,
    cancel_erasure, claim_erasure, complete_erasure, get_erasure, list_drives, list_erasures,
    request_erasure,
};
use tangible_domain::{DriveId, ErasureId, ErasureMode, ErasureState};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use utoipa::{IntoParams, ToSchema};

use crate::auth::SignedIn;
use crate::problem::{ErrorCode, Problem};
use crate::routes::workers::same_worker;
use crate::state::ApiState;
use crate::worker_auth::WorkerIdentity;

/// Most erasures listed at once.
const MAX_LIMIT: i64 = 200;

/// Erasures listed when the request does not say.
const DEFAULT_LIMIT: i64 = 50;

/// Longest error code a worker may report, matching the column.
const MAX_ERROR_CODE: usize = 100;

/// Longest error detail kept, matching the column.
const MAX_ERROR_DETAIL: usize = 10_000;

/// Longest profile name kept.
const MAX_PROFILE: usize = 100;

fn rfc3339(value: OffsetDateTime) -> String {
    value.format(&Rfc3339).unwrap_or_default()
}

fn unavailable(what: &str) -> Problem {
    Problem::new(
        ErrorCode::StorageUnavailable,
        format!("{what}; see server logs"),
    )
}

fn parse_drive(raw: &str) -> Result<DriveId, Problem> {
    raw.parse()
        .map_err(|_| Problem::invalid_parameter("drive_id", "not a valid identifier"))
}

fn parse_erasure(raw: &str) -> Result<ErasureId, Problem> {
    raw.parse()
        .map_err(|_| Problem::invalid_parameter("erasure_id", "not a valid identifier"))
}

// --- views ---------------------------------------------------------------------

/// A drive, and the worker it belongs to.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DriveView {
    /// Opaque identifier.
    pub id: String,
    /// What the worker calls it.
    pub name: String,
    /// Its device path inside the worker's container.
    pub device: String,
    /// Reported vendor.
    pub vendor: Option<String>,
    /// Reported model.
    pub model: Option<String>,
    /// Last reported status.
    pub status: String,
    /// When it was last reported.
    pub last_seen_at: Option<String>,
    /// The worker.
    pub worker_id: String,
    /// The worker's name.
    pub worker_name: String,
    /// The worker's status.
    pub worker_status: String,
    /// When the worker last reported in.
    pub worker_last_seen_at: Option<String>,
}

impl From<DriveRecord> for DriveView {
    fn from(drive: DriveRecord) -> Self {
        Self {
            id: drive.id.to_string(),
            name: drive.configured_name,
            device: drive.device_alias,
            vendor: drive.vendor,
            model: drive.model,
            status: drive.status,
            last_seen_at: drive.last_seen_at.map(rfc3339),
            worker_id: drive.worker_id.to_string(),
            worker_name: drive.worker_name,
            worker_status: drive.worker_status,
            worker_last_seen_at: drive.worker_last_seen_at.map(rfc3339),
        }
    }
}

/// Every drive.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DriveList {
    /// By worker, then by drive.
    pub items: Vec<DriveView>,
}

/// A request to erase the disc in a drive, and how it went.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ErasureView {
    /// Opaque identifier.
    pub id: String,
    /// The drive.
    pub drive_id: String,
    /// What the drive is called.
    pub drive_name: String,
    /// `quick` or `full`.
    pub mode: String,
    /// `queued`, `erasing`, `erased`, `already_blank`, `refused`, `failed`
    /// or `canceled`.
    pub state: String,
    /// Whether it is still going.
    pub is_open: bool,
    /// Whether the disc may have been changed: true once an erase has run,
    /// whatever it achieved.
    pub touched_the_disc: bool,
    /// Who asked.
    pub requested_by: String,
    /// What the worker found in the drive before deciding.
    pub medium_before: Option<ErasureMedium>,
    /// Why it did not erase, when it did not.
    pub error_code: Option<String>,
    /// More about that.
    pub error_detail: Option<String>,
    /// How long the erase took, in seconds.
    pub duration_seconds: Option<i32>,
    /// When it was asked for.
    pub created_at: String,
    /// When the worker took it.
    pub started_at: Option<String>,
    /// When it ended.
    pub completed_at: Option<String>,
}

impl From<ErasureRecord> for ErasureView {
    fn from(record: ErasureRecord) -> Self {
        Self {
            id: record.id.to_string(),
            drive_id: record.drive_id.to_string(),
            drive_name: record.drive_name,
            mode: record.mode.to_string(),
            state: record.state.to_string(),
            is_open: record.state.is_open(),
            touched_the_disc: record.state.touched_the_disc(),
            requested_by: record.requested_by,
            medium_before: record
                .medium_before
                .and_then(|value| serde_json::from_value(value).ok()),
            error_code: record.error_code,
            error_detail: record.error_detail,
            duration_seconds: record.duration_seconds,
            created_at: rfc3339(record.created_at),
            started_at: record.started_at.map(rfc3339),
            completed_at: record.completed_at.map(rfc3339),
        }
    }
}

/// Recent erasures.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ErasureList {
    /// Newest first.
    pub items: Vec<ErasureView>,
}

/// What was in the drive.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ErasureMedium {
    /// The media profile the drive reported, such as `DVD-RW`.
    pub profile: String,
    /// Whether it was blank.
    pub blank: bool,
    /// Whether it can be erased.
    pub rewritable: bool,
    /// Sessions on it.
    pub sessions: u32,
}

// --- requests ------------------------------------------------------------------

/// What an operator sends to erase the disc in a drive.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestErasure {
    /// `quick`, which does only what makes the disc writable again, or
    /// `full`, which writes over the whole disc. Quick is fast on a CD-RW;
    /// some drives erase a whole DVD-RW either way, which takes half an hour
    /// or more. Full can take an hour.
    pub mode: String,
    /// Must be `true`. Erasing destroys everything on the disc in the drive,
    /// and the request has to say that this is intended.
    pub confirm_data_loss: bool,
}

/// Paging for the erasure list.
#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ErasureListQuery {
    /// How many, at most 200. Defaults to 50.
    pub limit: Option<i64>,
}

/// What a worker sends to ask for an erasure.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ErasureClaimRequest {
    /// The worker's drive.
    pub drive_id: String,
}

/// An erasure a worker has taken.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ErasureClaim {
    /// The erasure.
    pub erasure_id: String,
    /// `quick` or `full`.
    pub mode: String,
}

/// How an erasure ended, as the worker reports it.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ErasureCompletion {
    /// `erased`, `already_blank`, `refused` or `failed`.
    pub outcome: String,
    /// What was in the drive, when there was a disc to look at.
    pub medium: Option<ErasureMedium>,
    /// A stable code saying why it did not erase.
    pub error_code: Option<String>,
    /// More about that.
    pub error_detail: Option<String>,
    /// How long it took, in seconds.
    pub duration_seconds: Option<u32>,
}

// --- drives ----------------------------------------------------------------------

/// Every drive.
///
/// # Errors
///
/// `STORAGE_UNAVAILABLE` if drives cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/drives",
    tag = "workers",
    responses((status = 200, description = "Every drive", body = DriveList)),
)]
pub async fn list_all_drives(State(state): State<ApiState>) -> Result<Json<DriveList>, Problem> {
    let drives = list_drives(state.database().pool())
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not list drives");
            unavailable("drives could not be read")
        })?;
    Ok(Json(DriveList {
        items: drives.into_iter().map(DriveView::from).collect(),
    }))
}

// --- erasures, for people ----------------------------------------------------------

/// Ask for the disc in a drive to be erased.
///
/// # Errors
///
/// `VALIDATION_FAILED` without the confirmation or with an unknown mode,
/// `NOT_FOUND` for an unknown drive, `CONFLICT` if the drive already has an
/// erasure queued or running.
#[utoipa::path(
    post,
    path = "/api/v1/drives/{drive_id}/erasures",
    tag = "workers",
    description = "Erase the rewritable disc in a drive, destroying everything on it. \
                   The request must set confirm_data_loss. The drive's worker takes \
                   it before any burn work, erases only a rewritable disc that holds \
                   data, and reports what it found.",
    params(("drive_id" = String, Path, description = "Drive identifier")),
    request_body = RequestErasure,
    responses(
        (status = 201, description = "Queued", body = ErasureView),
        (status = 404, description = "No such drive", body = Problem),
        (status = 409, description = "The drive already has an erasure open", body = Problem),
        (status = 422, description = "Not confirmed, or an unknown mode", body = Problem),
    ),
)]
pub async fn request_drive_erasure(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(drive_id): Path<String>,
    Json(request): Json<RequestErasure>,
) -> Result<Response, Problem> {
    let drive_id = parse_drive(&drive_id)?;
    let mode = ErasureMode::from_str(&request.mode)
        .map_err(|_| Problem::new(ErrorCode::ValidationFailed, "mode must be quick or full"))?;
    if !request.confirm_data_loss {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            "erasing destroys everything on the disc in this drive; set confirm_data_loss \
             to true to say that is intended",
        ));
    }

    let record = request_erasure(state.database().pool(), drive_id, mode, &user.username)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not queue an erasure");
            unavailable("the erasure could not be queued")
        })?
        .map_err(|outcome| match outcome {
            RequestOutcome::DriveMissing => Problem::not_found("drive", &drive_id.to_string()),
            RequestOutcome::AlreadyOpen => Problem::new(
                ErrorCode::Conflict,
                "this drive already has an erasure queued or running",
            ),
        })?;
    tracing::warn!(
        erasure_id = %record.id,
        %drive_id,
        mode = %mode,
        by = %user.username,
        "disc erasure requested"
    );
    Ok((StatusCode::CREATED, Json(ErasureView::from(record))).into_response())
}

/// Recent erasures.
///
/// # Errors
///
/// `INVALID_PARAMETER` for a limit out of range.
#[utoipa::path(
    get,
    path = "/api/v1/erasures",
    tag = "workers",
    params(ErasureListQuery),
    responses((status = 200, description = "Newest first", body = ErasureList)),
)]
pub async fn list_recent_erasures(
    State(state): State<ApiState>,
    Query(query): Query<ErasureListQuery>,
) -> Result<Json<ErasureList>, Problem> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(Problem::invalid_parameter("limit", "must be from 1 to 200"));
    }
    let items = list_erasures(state.database().pool(), limit)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not list erasures");
            unavailable("erasures could not be read")
        })?;
    Ok(Json(ErasureList {
        items: items.into_iter().map(ErasureView::from).collect(),
    }))
}

/// One erasure.
///
/// # Errors
///
/// `NOT_FOUND` for an unknown erasure.
#[utoipa::path(
    get,
    path = "/api/v1/erasures/{erasure_id}",
    tag = "workers",
    params(("erasure_id" = String, Path, description = "Erasure identifier")),
    responses(
        (status = 200, description = "The erasure", body = ErasureView),
        (status = 404, description = "No such erasure", body = Problem),
    ),
)]
pub async fn get_one_erasure(
    State(state): State<ApiState>,
    Path(erasure_id): Path<String>,
) -> Result<Json<ErasureView>, Problem> {
    let id = parse_erasure(&erasure_id)?;
    get_erasure(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read an erasure");
            unavailable("the erasure could not be read")
        })?
        .map(|record| Json(ErasureView::from(record)))
        .ok_or_else(|| Problem::not_found("erasure", &id.to_string()))
}

/// Withdraw an erasure the worker has not taken yet.
///
/// # Errors
///
/// `NOT_FOUND` for an unknown erasure, `CONFLICT` once the drive has started.
#[utoipa::path(
    post,
    path = "/api/v1/erasures/{erasure_id}/cancel",
    tag = "workers",
    description = "Withdraw a queued erasure. Refused once the drive has started: \
                   stopping an erase partway leaves the disc needing another one.",
    params(("erasure_id" = String, Path, description = "Erasure identifier")),
    responses(
        (status = 200, description = "Withdrawn", body = ErasureView),
        (status = 404, description = "No such erasure", body = Problem),
        (status = 409, description = "Already started or finished", body = Problem),
    ),
)]
pub async fn cancel_one_erasure(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(erasure_id): Path<String>,
) -> Result<Json<ErasureView>, Problem> {
    let id = parse_erasure(&erasure_id)?;
    let record = cancel_erasure(state.database().pool(), id, &user.username)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not withdraw an erasure");
            unavailable("the erasure could not be withdrawn")
        })?
        .map_err(|outcome| match outcome {
            CancelOutcome::NotFound => Problem::not_found("erasure", &id.to_string()),
            CancelOutcome::NotQueued(state) => Problem::new(
                ErrorCode::Conflict,
                format!("this erasure is {state} and can no longer be withdrawn"),
            ),
        })?;
    Ok(Json(ErasureView::from(record)))
}

// --- erasures, for workers ------------------------------------------------------------

/// Ask for an erasure queued for this worker's drive.
///
/// # Errors
///
/// `NOT_FOUND` if the path names another worker or the drive is not this
/// worker's.
#[utoipa::path(
    post,
    path = "/api/v1/workers/{worker_id}/erasure-claims",
    tag = "workers",
    description = "Take the next erasure queued for this worker's drive. Answers \
                   204 when there is none, or while a burn attempt holds the drive.",
    params(("worker_id" = String, Path, description = "Worker identifier")),
    request_body = ErasureClaimRequest,
    responses(
        (status = 200, description = "Taken", body = ErasureClaim),
        (status = 204, description = "Nothing to erase"),
        (status = 404, description = "Not this worker, or not its drive", body = Problem),
    ),
    security(("worker_credential" = [])),
)]
pub async fn claim_drive_erasure(
    State(state): State<ApiState>,
    identity: WorkerIdentity,
    Path(worker_id): Path<String>,
    Json(request): Json<ErasureClaimRequest>,
) -> Result<Response, Problem> {
    let worker_id = same_worker(identity, &worker_id)?;
    let drive_id = parse_drive(&request.drive_id)?;
    let claimed = claim_erasure(state.database().pool(), worker_id, drive_id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not claim an erasure");
            unavailable("erasures could not be read")
        })?
        .map_err(|_| Problem::not_found("drive", &drive_id.to_string()))?;
    Ok(match claimed {
        None => StatusCode::NO_CONTENT.into_response(),
        Some(record) => {
            tracing::warn!(erasure_id = %record.id, %worker_id, %drive_id, "disc erasure taken");
            Json(ErasureClaim {
                erasure_id: record.id.to_string(),
                mode: record.mode.to_string(),
            })
            .into_response()
        }
    })
}

/// Report how an erasure ended.
///
/// # Errors
///
/// `NOT_FOUND` if the erasure is not on this worker's drive, `CONFLICT` if it
/// is not in progress, `VALIDATION_FAILED` for an outcome or field that will
/// not do.
#[utoipa::path(
    post,
    path = "/api/v1/erasures/{erasure_id}/complete",
    tag = "workers",
    description = "Report how an erasure ended. Repeating the same report is accepted.",
    params(("erasure_id" = String, Path, description = "Erasure identifier")),
    request_body = ErasureCompletion,
    responses(
        (status = 200, description = "Recorded", body = ErasureView),
        (status = 404, description = "Not this worker's erasure", body = Problem),
        (status = 409, description = "Not in progress", body = Problem),
        (status = 422, description = "An outcome or field that will not do", body = Problem),
    ),
    security(("worker_credential" = [])),
)]
pub async fn complete_drive_erasure(
    State(state): State<ApiState>,
    identity: WorkerIdentity,
    Path(erasure_id): Path<String>,
    Json(report): Json<ErasureCompletion>,
) -> Result<Json<ErasureView>, Problem> {
    let id = parse_erasure(&erasure_id)?;
    let outcome = ErasureState::from_str(&report.outcome)
        .ok()
        .filter(|state| {
            matches!(
                state,
                ErasureState::Erased
                    | ErasureState::AlreadyBlank
                    | ErasureState::Refused
                    | ErasureState::Failed
            )
        })
        .ok_or_else(|| {
            Problem::new(
                ErrorCode::ValidationFailed,
                "outcome must be erased, already_blank, refused or failed",
            )
        })?;
    let error_code = report
        .error_code
        .as_deref()
        .map(|code| {
            let valid = !code.is_empty()
                && code.len() <= MAX_ERROR_CODE
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_');
            if valid {
                Ok(code)
            } else {
                Err(Problem::new(
                    ErrorCode::ValidationFailed,
                    "error_code must be uppercase letters, digits and underscores",
                ))
            }
        })
        .transpose()?;
    let error_detail: Option<String> = report
        .error_detail
        .map(|detail| detail.chars().take(MAX_ERROR_DETAIL).collect());
    let medium = report
        .medium
        .map(|mut medium| {
            medium.profile = medium.profile.chars().take(MAX_PROFILE).collect();
            serde_json::to_value(medium)
        })
        .transpose()
        .map_err(|_| Problem::new(ErrorCode::ValidationFailed, "medium could not be read"))?;

    let record = complete_erasure(
        state.database().pool(),
        identity.worker_id,
        id,
        &ErasureResult {
            state: outcome,
            medium_before: medium.as_ref(),
            error_code,
            error_detail: error_detail.as_deref(),
            duration_seconds: report
                .duration_seconds
                .map(|seconds| i32::try_from(seconds).unwrap_or(i32::MAX)),
        },
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not record an erasure");
        unavailable("the erasure could not be recorded")
    })?
    .map_err(|outcome| match outcome {
        WorkerOutcome::NotYours => Problem::not_found("erasure", &id.to_string()),
        WorkerOutcome::Conflict(state) => Problem::new(
            ErrorCode::Conflict,
            format!("this erasure is {state}, not erasing"),
        ),
    })?;
    tracing::warn!(
        erasure_id = %record.id,
        state = %record.state,
        error_code = ?record.error_code,
        "disc erasure finished"
    );
    Ok(Json(ErasureView::from(record)))
}

/// The drive and erasure routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/drives", get(list_all_drives))
        .route("/drives/{drive_id}/erasures", post(request_drive_erasure))
        .route("/erasures", get(list_recent_erasures))
        .route("/erasures/{erasure_id}", get(get_one_erasure))
        .route("/erasures/{erasure_id}/cancel", post(cancel_one_erasure))
        .route(
            "/erasures/{erasure_id}/complete",
            post(complete_drive_erasure),
        )
        .route(
            "/workers/{worker_id}/erasure-claims",
            post(claim_drive_erasure),
        )
}
