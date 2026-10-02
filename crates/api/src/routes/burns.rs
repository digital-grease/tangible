// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Burn jobs, as an operator sees them.
//!
//! The other half of the burn surface. `workers` is what a burn worker talks
//! to; this is what a person talks to: queue a burn, watch it, cancel it while
//! that is still safe, retry it when it is not.
//!
//! Two things here are worth stating plainly, because they are the reason the
//! routes are shaped as they are.
//!
//! **Creation is idempotent.** A burn spends a physical disc. A retried POST
//! that quietly queued a second job would cost a disc for a dropped response,
//! so a request carrying an `Idempotency-Key` returns the job the first
//! attempt created, and the same key used for a *different* request is
//! refused rather than answered.
//!
//! **Cancellation is refused once a disc is being consumed.** Not deferred,
//! not best-effort: refused, with its own error code, because stopping a write
//! mid-way ruins the medium. The protocol calls cancellation a request rather
//! than a kill for the same reason.
//!
//! These routes are unauthenticated for now, as the rest of the product API
//! is. The user model lands with authentication and adds it in one place.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tangible_db::repositories::{
    ArtifactBurnFacts, BurnAttemptRecord, BurnEventRecord, BurnJobFilter, BurnJobRecord,
    CancelOutcome, CreateJobOutcome, NewBurnJob, RetryOutcome, artifact_burn_facts,
    cancel_burn_job, create_burn_job, get_burn_attempt, get_burn_job, list_burn_attempts,
    list_burn_events, list_burn_jobs, resolve_burn_job_attention, retry_burn_job,
};
use tangible_domain::{
    ArtifactId, BurnAttemptId, BurnJobId, BurnJobState, DiscId, EjectPolicy, VerificationStep,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use utoipa::{IntoParams, ToSchema};

use crate::auth::SignedIn;
use crate::pagination::{decode_cursor, encode_cursor};
use crate::problem::{ErrorCode, Problem};
use crate::state::ApiState;

/// Largest page of jobs or events a caller may ask for.
const MAX_LIMIT: usize = 500;

/// Default page size.
const DEFAULT_LIMIT: usize = 50;

/// Longest idempotency key accepted, matching the column's own limit.
const MAX_IDEMPOTENCY_KEY: usize = 200;

fn rfc3339(value: OffsetDateTime) -> String {
    value.format(&Rfc3339).unwrap_or_default()
}

// --- views ---------------------------------------------------------------------

/// What a worker last said it was doing.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BurnProgress {
    /// The stage the worker reported.
    pub stage: String,
    /// Progress within that stage, from 0 to 1, when the stage reports it.
    pub fraction: Option<f32>,
    /// Stable machine-readable code for the event.
    pub code: String,
    /// When the worker observed it.
    pub observed_at: String,
}

/// A burn job.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BurnJobView {
    /// Opaque identifier.
    pub id: String,
    /// The disc this copy represents.
    pub disc_id: String,
    /// The artifact being written.
    pub artifact_id: String,
    /// Lifecycle state.
    pub state: String,
    /// Whether a disc is being or has been consumed.
    ///
    /// Derived from the state so a client does not have to know which states
    /// those are. It is what decides whether cancelling is still offered.
    pub has_started_writing: bool,
    /// Queue priority; higher is claimed first.
    pub priority: i16,
    /// Ordered verification steps.
    pub verification_policy: Vec<String>,
    /// What to do with the disc afterwards.
    pub eject_policy: String,
    /// Media profile requested, when one was.
    pub requested_media_profile: Option<String>,
    /// Who asked for it.
    pub created_by: String,
    /// When it was queued.
    pub created_at: String,
    /// When it last changed.
    pub updated_at: String,
    /// When it finished, if it has.
    pub completed_at: Option<String>,
    /// How many attempts have been made, including failed ones.
    ///
    /// Each attempt past the first spent a disc, so this is not a retry
    /// counter but a count of media consumed.
    pub attempt_count: i64,
    /// The most recent thing a worker reported, if any.
    pub progress: Option<BurnProgress>,
}

impl From<BurnJobRecord> for BurnJobView {
    fn from(record: BurnJobRecord) -> Self {
        let progress = record.latest_stage.as_ref().map(|stage| BurnProgress {
            stage: stage.clone(),
            fraction: record.latest_progress,
            code: record.latest_message_code.clone().unwrap_or_default(),
            observed_at: record.latest_event_at.map(rfc3339).unwrap_or_default(),
        });

        Self {
            id: record.id.to_string(),
            disc_id: record.disc_id.to_string(),
            artifact_id: record.artifact_id.to_string(),
            state: record.state.to_string(),
            has_started_writing: record.state.has_started_writing(),
            priority: record.priority,
            verification_policy: record.verification_policy,
            eject_policy: record.eject_policy,
            requested_media_profile: record.requested_media_profile,
            created_by: record.created_by,
            created_at: rfc3339(record.created_at),
            updated_at: rfc3339(record.updated_at),
            completed_at: record.completed_at.map(rfc3339),
            attempt_count: record.attempt_count,
            progress,
        }
    }
}

/// A page of burn jobs.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BurnJobPage {
    /// The jobs in this page, newest first.
    pub items: Vec<BurnJobView>,
    /// Cursor for the next page, or null when this is the last.
    pub next_cursor: Option<String>,
}

/// One execution of a burn job.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BurnAttemptView {
    /// Opaque identifier.
    pub id: String,
    /// The job it belongs to.
    pub burn_job_id: String,
    /// Which attempt this is, starting at 1.
    pub attempt_number: i32,
    /// Where it got to.
    pub state: String,
    /// Whether this attempt consumed a disc.
    pub consumed_media: bool,
    /// The worker that ran it.
    pub worker_id: String,
    /// The drive it ran on.
    pub drive_id: String,
    /// Which engine wrote.
    pub engine: String,
    /// That engine's version.
    pub engine_version: String,
    /// A stable failure code, when it failed.
    pub error_code: Option<String>,
    /// Human-readable failure detail, when it failed.
    pub error_detail: Option<String>,
    /// The disc this attempt produced, if it produced one.
    pub physical_copy_id: Option<String>,
    /// When it started.
    pub started_at: String,
    /// When it ended, if it has.
    pub ended_at: Option<String>,
    /// Highest event sequence held for it.
    pub last_event_sequence: Option<i64>,
}

impl From<BurnAttemptRecord> for BurnAttemptView {
    fn from(record: BurnAttemptRecord) -> Self {
        Self {
            id: record.id.to_string(),
            burn_job_id: record.burn_job_id.to_string(),
            attempt_number: record.attempt_number,
            state: record.state.to_string(),
            consumed_media: record.state.consumed_media(),
            worker_id: record.worker_id.to_string(),
            drive_id: record.drive_id.to_string(),
            engine: record.engine,
            engine_version: record.engine_version,
            error_code: record.error_code,
            error_detail: record.error_detail,
            physical_copy_id: record.physical_copy_id.map(|id| id.to_string()),
            started_at: rfc3339(record.started_at),
            ended_at: record.ended_at.map(rfc3339),
            last_event_sequence: record.last_event_sequence,
        }
    }
}

/// One attempt with the reports it produced.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BurnAttemptDetail {
    /// Summary fields.
    #[serde(flatten)]
    pub attempt: BurnAttemptView,
    /// The engine's account of the write, once there is one.
    pub write_report: Option<serde_json::Value>,
    /// The read-back comparison, when one ran.
    pub verify_report: Option<serde_json::Value>,
}

/// A burn job with its attempts.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BurnJobDetail {
    /// Summary fields.
    #[serde(flatten)]
    pub job: BurnJobView,
    /// Every attempt made, oldest first.
    pub attempts: Vec<BurnAttemptView>,
}

/// A newly queued job, with anything an operator should know about it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BurnJobCreated {
    /// The job.
    #[serde(flatten)]
    pub job: BurnJobView,
    /// Things worth knowing before the disc is spent.
    ///
    /// Warnings, not refusals: an operator may knowingly burn an artifact
    /// that validated with warnings, and the system's job is to say so rather
    /// than to decide for them.
    pub warnings: Vec<String>,
}

/// One recorded event.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BurnEventView {
    /// Monotonic within the attempt.
    pub sequence: i64,
    /// Event type, such as `stage_changed`.
    pub event_type: String,
    /// Stage it relates to.
    pub stage: String,
    /// Progress from 0 to 1, when reported.
    pub fraction: Option<f32>,
    /// Stable machine-readable code.
    pub code: String,
    /// Structured detail.
    pub data: serde_json::Value,
    /// When the worker observed it.
    pub observed_at: String,
    /// When the server stored it.
    pub received_at: String,
}

impl From<BurnEventRecord> for BurnEventView {
    fn from(record: BurnEventRecord) -> Self {
        Self {
            sequence: record.sequence,
            event_type: record.event_type,
            stage: record.stage,
            fraction: record.progress,
            code: record.message_code,
            data: record.data,
            observed_at: rfc3339(record.worker_timestamp),
            received_at: rfc3339(record.server_received_at),
        }
    }
}

/// A page of events.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BurnEventPage {
    /// The events, in sequence order.
    pub items: Vec<BurnEventView>,
    /// Sequence to pass as `after` to continue, or null at the end.
    ///
    /// A sequence rather than an opaque cursor: sequences are already the
    /// currency of this timeline (the worker numbers events with them and
    /// the server acknowledges them), so hiding them here would be a
    /// different vocabulary for the same thing.
    pub next_after: Option<i64>,
}

// --- requests ------------------------------------------------------------------

/// What an operator sends to queue a burn.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateBurnJobRequest {
    /// The disc the copy represents.
    pub disc_id: String,
    /// The artifact to write.
    pub artifact_id: String,
    /// Media profile to require, or null for whatever fits.
    #[serde(default)]
    pub requested_media_profile: Option<String>,
    /// Ordered verification steps. Defaults to a full read-back.
    #[serde(default)]
    pub verification_policy: Option<Vec<String>>,
    /// What to do with the disc afterwards. Defaults to ejecting on success.
    #[serde(default)]
    pub eject_policy: Option<String>,
    /// Queue priority; higher is claimed first. Defaults to 0.
    #[serde(default)]
    pub priority: Option<i64>,
}

/// A create request that has been checked.
#[derive(Debug)]
struct Validated {
    disc_id: DiscId,
    artifact_id: ArtifactId,
    requested_media_profile: Option<String>,
    verification_policy: Vec<String>,
    eject_policy: EjectPolicy,
    priority: i16,
}

/// Check a create request without touching the database.
///
/// Kept pure so the rules are unit-testable: every refusal here is a decision
/// about what the system will agree to burn.
fn validate(request: &CreateBurnJobRequest) -> Result<Validated, Problem> {
    let disc_id = request
        .disc_id
        .parse::<DiscId>()
        .map_err(|_| Problem::invalid_parameter("disc_id", "not a valid identifier"))?;
    let artifact_id = request
        .artifact_id
        .parse::<ArtifactId>()
        .map_err(|_| Problem::invalid_parameter("artifact_id", "not a valid identifier"))?;

    let steps = request
        .verification_policy
        .clone()
        .unwrap_or_else(|| vec![VerificationStep::FullSectorReadback.to_string()]);

    // An empty policy is refused rather than filled in with the default:
    // sending `[]` is a caller asking for no verification, and skipping
    // read-back has to be said out loud, with `none`.
    if steps.is_empty() {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            "verification_policy must name at least one step; use [\"none\"] to \
             burn without reading the disc back",
        ));
    }
    for step in &steps {
        step.parse::<VerificationStep>().map_err(|_| {
            Problem::new(
                ErrorCode::ValidationFailed,
                format!("{step} is not a verification step this server knows"),
            )
        })?;
    }

    let eject_policy = match &request.eject_policy {
        None => EjectPolicy::default(),
        Some(raw) => raw.parse::<EjectPolicy>().map_err(|_| {
            Problem::new(
                ErrorCode::ValidationFailed,
                format!("{raw} is not an eject policy this server knows"),
            )
        })?,
    };

    let priority = match request.priority {
        None => 0,
        Some(value) => i16::try_from(value).map_err(|_| {
            Problem::new(
                ErrorCode::ValidationFailed,
                format!("priority must be between {} and {}", i16::MIN, i16::MAX),
            )
        })?,
    };

    if let Some(profile) = &request.requested_media_profile
        && (profile.is_empty() || profile.len() > 100)
    {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            "requested_media_profile must be between 1 and 100 characters",
        ));
    }

    Ok(Validated {
        disc_id,
        artifact_id,
        requested_media_profile: request.requested_media_profile.clone(),
        verification_policy: steps,
        eject_policy,
        priority,
    })
}

/// Decide whether an artifact may be burned, and what to say about it.
///
/// Refusals are limited to the two cases where writing the disc would be
/// meaningless: bytes the operator has asked the system to delete, and an
/// artifact held back as unsafe. Everything else is a warning, because an
/// operator burning a known-imperfect image is doing something the system
/// should support and describe honestly, not prevent.
fn eligibility(facts: &ArtifactBurnFacts) -> Result<Vec<String>, Problem> {
    if facts.pending_deletion {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            "this artifact's payload is scheduled for deletion",
        ));
    }
    if matches!(facts.quarantine_state.as_str(), "pending" | "confirmed") {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            "this artifact is quarantined and cannot be burned",
        ));
    }

    let mut warnings = Vec::new();
    match facts.validation_state.as_str() {
        "invalid" => warnings.push(
            "the artifact failed structural validation; the disc will hold what \
             was imported, faults included"
                .to_owned(),
        ),
        "unsupported" => warnings.push(
            "this server cannot validate the artifact's format; burn support is \
                   unproven"
                .to_owned(),
        ),
        "pending" => {
            warnings.push("the artifact has not been validated yet".to_owned());
        }
        "valid_with_warnings" => {
            warnings.push("the artifact validated with warnings".to_owned());
        }
        _ => {}
    }
    if facts.total_bytes == 0 {
        warnings.push("the artifact is empty".to_owned());
    }
    // Said plainly on every burn, because it is the claim the project refuses
    // to make and an operator should not infer it from silence.
    warnings.push(
        "target compatibility is unknown: a verified disc holds the same bytes, \
         which is not a promise that any particular player or console will accept it"
            .to_owned(),
    );
    Ok(warnings)
}

/// Read an idempotency key from the request headers.
fn idempotency_key(headers: &HeaderMap) -> Result<Option<String>, Problem> {
    let Some(value) = headers.get("idempotency-key") else {
        return Ok(None);
    };
    let key = value
        .to_str()
        .map_err(|_| Problem::invalid_parameter("Idempotency-Key", "must be printable ASCII"))?;
    if key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY {
        return Err(Problem::invalid_parameter(
            "Idempotency-Key",
            &format!("must be between 1 and {MAX_IDEMPOTENCY_KEY} characters"),
        ));
    }
    Ok(Some(key.to_owned()))
}

// --- listing -------------------------------------------------------------------

/// Query parameters for the job listing.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct BurnJobQuery {
    /// Maximum jobs to return. Clamped.
    pub limit: Option<usize>,
    /// Opaque cursor from a previous response's `next_cursor`.
    pub cursor: Option<String>,
    /// Only jobs in this state.
    pub state: Option<String>,
    /// Only jobs writing this artifact.
    pub artifact_id: Option<String>,
    /// Only jobs producing this disc.
    pub disc_id: Option<String>,
}

fn limit_of(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

fn as_i64(limit: usize) -> i64 {
    // The limit is already clamped to MAX_LIMIT, so the fallback is only
    // reachable if that clamp is ever removed.
    i64::try_from(limit).unwrap_or(i64::try_from(DEFAULT_LIMIT).unwrap_or(50))
}

/// List burn jobs, newest first.
///
/// # Errors
///
/// `INVALID_PARAMETER` for a malformed filter or cursor.
#[utoipa::path(
    get,
    path = "/api/v1/burn-jobs",
    tag = "burns",
    description = "List burn jobs, newest first. Filter by state, artifact, or \
                   disc; page with `next_cursor`.",
    params(BurnJobQuery),
    responses(
        (status = 200, description = "A page of burn jobs", body = BurnJobPage),
        (status = 400, description = "Malformed filter or cursor", body = Problem),
    ),
)]
pub async fn list_jobs(
    State(state): State<ApiState>,
    Query(query): Query<BurnJobQuery>,
) -> Result<Json<BurnJobPage>, Problem> {
    let limit = limit_of(query.limit);

    // A cursor is the last job identifier of the previous page, so it has to
    // parse as one. Anything else was not issued by this server.
    let before = match &query.cursor {
        None => None,
        Some(cursor) => Some(
            *decode_cursor(cursor)?
                .parse::<BurnJobId>()
                .map_err(|_| Problem::new(ErrorCode::InvalidCursor, "the cursor is not usable"))?
                .as_uuid(),
        ),
    };

    let filter = BurnJobFilter {
        state: match &query.state {
            None => None,
            Some(raw) => Some(raw.parse::<BurnJobState>().map_err(|_| {
                Problem::invalid_parameter("state", "not a burn job state this server knows")
            })?),
        },
        artifact_id: match &query.artifact_id {
            None => None,
            Some(raw) => Some(
                raw.parse::<ArtifactId>()
                    .map_err(|_| {
                        Problem::invalid_parameter("artifact_id", "not a valid identifier")
                    })?
                    .as_uuid()
                    .to_owned(),
            ),
        },
        disc_id: match &query.disc_id {
            None => None,
            Some(raw) => Some(
                raw.parse::<DiscId>()
                    .map_err(|_| Problem::invalid_parameter("disc_id", "not a valid identifier"))?
                    .as_uuid()
                    .to_owned(),
            ),
        },
    };

    let jobs = list_burn_jobs(state.database().pool(), filter, as_i64(limit), before)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not list burn jobs");
            unavailable("the burn queue could not be read")
        })?;

    let next_cursor = if jobs.len() < limit {
        None
    } else {
        jobs.last().map(|job| encode_cursor(&job.id.to_string()))
    };

    Ok(Json(BurnJobPage {
        items: jobs.into_iter().map(BurnJobView::from).collect(),
        next_cursor,
    }))
}

// --- creation ------------------------------------------------------------------

/// Queue a burn job.
///
/// Send an `Idempotency-Key` header when a retry is possible, which for
/// anything that spends a disc means always. A repeated request carrying the
/// same key returns the job the first one created, with `200` rather than
/// `201`; the same key with a different body is refused.
///
/// # Errors
///
/// `VALIDATION_FAILED` for a request the server will not act on,
/// `REFERENCE_NOT_FOUND` if the disc or artifact does not exist, and
/// `IDEMPOTENCY_CONFLICT` for a key reused with a different body.
#[utoipa::path(
    post,
    path = "/api/v1/burn-jobs",
    tag = "burns",
    description = "Queue a burn job. Idempotent when an Idempotency-Key header \
                   is supplied: a repeat returns the first job rather than \
                   spending a second disc.",
    params(
        ("Idempotency-Key" = Option<String>, Header,
         description = "Replay protection for a request that spends physical media"),
    ),
    request_body = CreateBurnJobRequest,
    responses(
        (status = 201, description = "Queued", body = BurnJobCreated),
        (status = 200, description = "Already queued by an identical request", body = BurnJobCreated),
        (status = 409, description = "The key was used for a different request", body = Problem),
        (status = 422, description = "The request will not be acted on", body = Problem),
    ),
)]
pub async fn create_job(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    headers: HeaderMap,
    Json(request): Json<CreateBurnJobRequest>,
) -> Result<axum::response::Response, Problem> {
    let validated = validate(&request)?;
    let key = idempotency_key(&headers)?;
    let pool = state.database().pool();

    // Read the artifact first so a refusal names what is wrong with it. The
    // foreign key would catch a missing artifact, but it would report a
    // constraint rather than the reason.
    let facts = artifact_burn_facts(pool, *validated.artifact_id.as_uuid())
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read the artifact");
            unavailable("the artifact could not be read")
        })?
        .ok_or_else(|| {
            Problem::new(
                ErrorCode::ReferenceNotFound,
                format!("no artifact with id {}", validated.artifact_id),
            )
        })?;
    let warnings = eligibility(&facts)?;

    let created = create_burn_job(
        pool,
        NewBurnJob {
            disc_id: *validated.disc_id.as_uuid(),
            artifact_id: *validated.artifact_id.as_uuid(),
            requested_media_profile: validated.requested_media_profile.as_deref(),
            verification_policy: &validated.verification_policy,
            eject_policy: validated.eject_policy.as_str(),
            priority: validated.priority,
            created_by: &user.username,
            idempotency_key: key.as_deref(),
        },
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not queue a burn job");
        unavailable("the burn job could not be queued")
    })?
    .map_err(|outcome| match outcome {
        CreateJobOutcome::DiscMissing => Problem::new(
            ErrorCode::ReferenceNotFound,
            format!("no disc with id {}", validated.disc_id),
        ),
        CreateJobOutcome::ArtifactMissing => Problem::new(
            ErrorCode::ReferenceNotFound,
            format!("no artifact with id {}", validated.artifact_id),
        ),
        CreateJobOutcome::IdempotencyConflict => Problem::new(
            ErrorCode::IdempotencyConflict,
            "that idempotency key was used for a different burn; use a new key",
        ),
    })?;

    if created.replayed {
        tracing::info!(
            burn_job_id = %created.job.id,
            "a burn job was requested again under the same idempotency key"
        );
    } else {
        tracing::info!(
            burn_job_id = %created.job.id,
            artifact_id = %created.job.artifact_id,
            disc_id = %created.job.disc_id,
            "burn job queued"
        );
    }

    // 200 for a replay, 201 for a job that did not exist a moment ago. A
    // caller retrying a request it never saw answered can tell which happened
    // without comparing timestamps.
    let status = if created.replayed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };

    Ok((
        status,
        Json(BurnJobCreated {
            job: created.job.into(),
            warnings,
        }),
    )
        .into_response())
}

// --- one job -------------------------------------------------------------------

fn parse_job(raw: &str) -> Result<BurnJobId, Problem> {
    raw.parse::<BurnJobId>()
        .map_err(|_| Problem::invalid_parameter("burn_job_id", "not a valid identifier"))
}

fn parse_attempt(raw: &str) -> Result<BurnAttemptId, Problem> {
    raw.parse::<BurnAttemptId>()
        .map_err(|_| Problem::invalid_parameter("attempt_id", "not a valid identifier"))
}

/// A storage failure on a burn route.
fn unavailable(detail: &str) -> Problem {
    Problem::new(ErrorCode::StorageUnavailable, detail)
}

async fn load_job(state: &ApiState, id: BurnJobId) -> Result<BurnJobRecord, Problem> {
    get_burn_job(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read a burn job");
            unavailable("the burn job could not be read")
        })?
        .ok_or_else(|| Problem::not_found("burn job", &id.to_string()))
}

async fn attempts_of(state: &ApiState, id: BurnJobId) -> Result<Vec<BurnAttemptView>, Problem> {
    let attempts = list_burn_attempts(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read burn attempts");
            unavailable("the burn attempts could not be read")
        })?;
    Ok(attempts.into_iter().map(BurnAttemptView::from).collect())
}

/// One burn job, with every attempt made at it.
///
/// # Errors
///
/// `NOT_FOUND` if no such job exists.
#[utoipa::path(
    get,
    path = "/api/v1/burn-jobs/{burn_job_id}",
    tag = "burns",
    description = "Fetch one burn job with its attempts. Each attempt past the \
                   first spent a disc.",
    params(("burn_job_id" = String, Path, description = "Burn job identifier")),
    responses(
        (status = 200, description = "The burn job", body = BurnJobDetail),
        (status = 404, description = "No such burn job", body = Problem),
    ),
)]
pub async fn get_job(
    State(state): State<ApiState>,
    Path(burn_job_id): Path<String>,
) -> Result<Json<BurnJobDetail>, Problem> {
    let id = parse_job(&burn_job_id)?;
    let job = load_job(&state, id).await?;
    let attempts = attempts_of(&state, id).await?;

    Ok(Json(BurnJobDetail {
        job: job.into(),
        attempts,
    }))
}

/// The attempts made at one job, oldest first.
///
/// # Errors
///
/// `NOT_FOUND` if no such job exists.
#[utoipa::path(
    get,
    path = "/api/v1/burn-jobs/{burn_job_id}/attempts",
    tag = "burns",
    description = "List the attempts made at a burn job, oldest first.",
    params(("burn_job_id" = String, Path, description = "Burn job identifier")),
    responses(
        (status = 200, description = "The attempts", body = Vec<BurnAttemptView>),
        (status = 404, description = "No such burn job", body = Problem),
    ),
)]
pub async fn list_attempts(
    State(state): State<ApiState>,
    Path(burn_job_id): Path<String>,
) -> Result<Json<Vec<BurnAttemptView>>, Problem> {
    let id = parse_job(&burn_job_id)?;
    // Load the job first so an unknown identifier is a 404 rather than an
    // empty list, which would read as "this job has never been attempted".
    load_job(&state, id).await?;
    Ok(Json(attempts_of(&state, id).await?))
}

/// Cancel a burn job that has not started writing.
///
/// Refused with `WRITE_IN_PROGRESS` once a disc is being consumed. That is not
/// a temporary condition to retry past: stopping a write ruins the medium, so
/// the attempt is allowed to finish and its outcome is recorded.
///
/// Cancelling an already-cancelled job succeeds and changes nothing.
///
/// # Errors
///
/// `NOT_FOUND`, `WRITE_IN_PROGRESS`, or `CONFLICT` if the job already
/// finished.
#[utoipa::path(
    post,
    path = "/api/v1/burn-jobs/{burn_job_id}/cancel",
    tag = "burns",
    description = "Cancel a burn job that has not started writing. Refused once \
                   a disc is being consumed, because stopping a write ruins the \
                   medium.",
    params(("burn_job_id" = String, Path, description = "Burn job identifier")),
    responses(
        (status = 200, description = "Cancelled", body = BurnJobDetail),
        (status = 404, description = "No such burn job", body = Problem),
        (status = 409, description = "Writing, or already finished", body = Problem),
    ),
)]
pub async fn cancel_job(
    State(state): State<ApiState>,
    Path(burn_job_id): Path<String>,
) -> Result<Json<BurnJobDetail>, Problem> {
    let id = parse_job(&burn_job_id)?;

    cancel_burn_job(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not cancel a burn job");
            unavailable("the burn job could not be cancelled")
        })?
        .map_err(|outcome| match outcome {
            CancelOutcome::NotFound => Problem::not_found("burn job", &id.to_string()),
            CancelOutcome::WriteInProgress => Problem::new(
                ErrorCode::WriteInProgress,
                "this burn is writing to a disc; stopping now would ruin it, so the \
                 attempt will finish and its outcome will be recorded",
            ),
            CancelOutcome::AlreadyFinished { state } => Problem::new(
                ErrorCode::Conflict,
                format!("this burn job has already finished, in state {state}"),
            ),
        })?;

    tracing::info!(burn_job_id = %id, "burn job cancelled");

    let job = load_job(&state, id).await?;
    let attempts = attempts_of(&state, id).await?;
    Ok(Json(BurnJobDetail {
        job: job.into(),
        attempts,
    }))
}

/// Requeue a failed or cancelled job for another attempt.
///
/// The previous attempts stay exactly as they are. A retry spends another
/// disc and records another attempt; it does not revise the history of the
/// discs already spent.
///
/// # Errors
///
/// `NOT_FOUND`, or `NOT_RETRYABLE` if the job finished cleanly, needs a human,
/// or is still running.
#[utoipa::path(
    post,
    path = "/api/v1/burn-jobs/{burn_job_id}/retry",
    tag = "burns",
    description = "Requeue a failed or cancelled burn job. Previous attempts \
                   are left as they are: a retry spends another disc and \
                   records another attempt.",
    params(("burn_job_id" = String, Path, description = "Burn job identifier")),
    responses(
        (status = 200, description = "Requeued", body = BurnJobDetail),
        (status = 404, description = "No such burn job", body = Problem),
        (status = 409, description = "Not in a state that can be retried", body = Problem),
    ),
)]
pub async fn retry_job(
    State(state): State<ApiState>,
    Path(burn_job_id): Path<String>,
) -> Result<Json<BurnJobDetail>, Problem> {
    let id = parse_job(&burn_job_id)?;

    retry_burn_job(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not retry a burn job");
            unavailable("the burn job could not be requeued")
        })?
        .map_err(|outcome| match outcome {
            RetryOutcome::NotFound => Problem::not_found("burn job", &id.to_string()),
            RetryOutcome::NotRetryable { state } => {
                Problem::new(ErrorCode::NotRetryable, retry_refusal(state))
            }
        })?;

    tracing::info!(burn_job_id = %id, "burn job requeued");

    let job = load_job(&state, id).await?;
    let attempts = attempts_of(&state, id).await?;
    Ok(Json(BurnJobDetail {
        job: job.into(),
        attempts,
    }))
}

/// Record that a person has accounted for a burn's disc.
///
/// `needs_attention` means a disc may exist that the system cannot account
/// for (the worker lost contact mid-write, or its engine vanished), and it
/// is deliberately a dead end. Nothing automatic may release it, because the
/// release is a claim about the physical world.
///
/// Record what was found against the disc itself first: the inventory is
/// where a disc's condition lives, and this route only says that somebody has
/// looked. The job becomes `failed`, from which an ordinary retry applies.
///
/// # Errors
///
/// `NOT_FOUND`, or `NOT_RETRYABLE` if the job is not asking for attention.
#[utoipa::path(
    post,
    path = "/api/v1/burn-jobs/{burn_job_id}/resolve-attention",
    tag = "burns",
    description = "Record that a person has accounted for a burn's disc. \
                   Releases a job stuck at needs_attention, leaving it failed \
                   so it can be retried.",
    params(("burn_job_id" = String, Path, description = "Burn job identifier")),
    responses(
        (status = 200, description = "Released", body = BurnJobDetail),
        (status = 404, description = "No such burn job", body = Problem),
        (status = 409, description = "This burn is not asking for attention", body = Problem),
    ),
)]
pub async fn resolve_attention(
    State(state): State<ApiState>,
    Path(burn_job_id): Path<String>,
) -> Result<Json<BurnJobDetail>, Problem> {
    let id = parse_job(&burn_job_id)?;

    resolve_burn_job_attention(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not resolve a burn job");
            unavailable("the burn job could not be released")
        })?
        .map_err(|outcome| match outcome {
            RetryOutcome::NotFound => Problem::not_found("burn job", &id.to_string()),
            RetryOutcome::NotRetryable { state } => Problem::new(
                ErrorCode::NotRetryable,
                format!("this burn is not asking for attention; it is {state}"),
            ),
        })?;

    tracing::info!(burn_job_id = %id, "burn job attention resolved");

    let job = load_job(&state, id).await?;
    let attempts = attempts_of(&state, id).await?;
    Ok(Json(BurnJobDetail {
        job: job.into(),
        attempts,
    }))
}

/// Why a retry was refused, in terms of what to do instead.
///
/// Each refusal has a different remedy, and a caller told only "not
/// retryable" would have to guess which.
fn retry_refusal(state: BurnJobState) -> String {
    match state {
        BurnJobState::Complete => "this burn finished successfully; queue a new burn job to \
             produce another disc"
            .to_owned(),
        BurnJobState::NeedsAttention => "this burn needs a person: a disc it may have written is \
             unaccounted for. Record what happened to that disc, then resolve the attention on \
             this job to make it retryable"
            .to_owned(),
        other => format!("this burn is still running, in state {other}"),
    }
}

// --- attempts and events --------------------------------------------------------

/// One attempt, with the reports it produced.
///
/// # Errors
///
/// `NOT_FOUND` if no such attempt exists.
#[utoipa::path(
    get,
    path = "/api/v1/burn-attempts/{attempt_id}",
    tag = "burns",
    description = "Fetch one burn attempt, including the engine's write report \
                   and the read-back comparison when there is one.",
    params(("attempt_id" = String, Path, description = "Attempt identifier")),
    responses(
        (status = 200, description = "The attempt", body = BurnAttemptDetail),
        (status = 404, description = "No such attempt", body = Problem),
    ),
)]
pub async fn get_attempt(
    State(state): State<ApiState>,
    Path(attempt_id): Path<String>,
) -> Result<Json<BurnAttemptDetail>, Problem> {
    let id = parse_attempt(&attempt_id)?;
    let record = get_burn_attempt(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read a burn attempt");
            unavailable("the burn attempt could not be read")
        })?
        .ok_or_else(|| Problem::not_found("burn attempt", &id.to_string()))?;

    let write_report = record.write_report.clone();
    let verify_report = record.verify_report.clone();
    Ok(Json(BurnAttemptDetail {
        attempt: record.into(),
        write_report,
        verify_report,
    }))
}

/// Query parameters for an event listing.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct EventQuery {
    /// Return only events after this sequence.
    pub after: Option<i64>,
    /// Maximum events to return. Clamped.
    pub limit: Option<usize>,
}

/// The events recorded for one attempt, in sequence order.
///
/// # Errors
///
/// `NOT_FOUND` if no such attempt exists.
#[utoipa::path(
    get,
    path = "/api/v1/burn-attempts/{attempt_id}/events",
    tag = "burns",
    description = "List the events a worker reported for an attempt, in \
                   sequence order. Pass the response's `next_after` as `after` \
                   to continue.",
    params(
        ("attempt_id" = String, Path, description = "Attempt identifier"),
        EventQuery,
    ),
    responses(
        (status = 200, description = "The events", body = BurnEventPage),
        (status = 404, description = "No such attempt", body = Problem),
    ),
)]
pub async fn list_events(
    State(state): State<ApiState>,
    Path(attempt_id): Path<String>,
    Query(query): Query<EventQuery>,
) -> Result<Json<BurnEventPage>, Problem> {
    let id = parse_attempt(&attempt_id)?;
    let limit = limit_of(query.limit);

    // Confirm the attempt exists so an unknown identifier is a 404 rather
    // than an empty timeline, which would read as a burn that reported
    // nothing.
    let exists = get_burn_attempt(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read a burn attempt");
            unavailable("the burn attempt could not be read")
        })?;
    if exists.is_none() {
        return Err(Problem::not_found("burn attempt", &id.to_string()));
    }

    let events = list_burn_events(state.database().pool(), id, query.after, as_i64(limit))
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read burn events");
            unavailable("the burn events could not be read")
        })?;

    let next_after = if events.len() < limit {
        None
    } else {
        events.last().map(|event| event.sequence)
    };

    Ok(Json(BurnEventPage {
        items: events.into_iter().map(BurnEventView::from).collect(),
        next_after,
    }))
}

/// The operator-facing burn routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/burn-jobs", get(list_jobs).post(create_job))
        .route("/burn-jobs/{burn_job_id}", get(get_job))
        .route("/burn-jobs/{burn_job_id}/attempts", get(list_attempts))
        .route("/burn-jobs/{burn_job_id}/cancel", post(cancel_job))
        .route("/burn-jobs/{burn_job_id}/retry", post(retry_job))
        .route(
            "/burn-jobs/{burn_job_id}/resolve-attention",
            post(resolve_attention),
        )
        .route("/burn-attempts/{attempt_id}", get(get_attempt))
        .route("/burn-attempts/{attempt_id}/events", get(list_events))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn request() -> CreateBurnJobRequest {
        CreateBurnJobRequest {
            disc_id: DiscId::generate().to_string(),
            artifact_id: ArtifactId::generate().to_string(),
            requested_media_profile: None,
            verification_policy: None,
            eject_policy: None,
            priority: None,
        }
    }

    fn facts() -> ArtifactBurnFacts {
        ArtifactBurnFacts {
            validation_state: "valid".to_owned(),
            quarantine_state: "none".to_owned(),
            pending_deletion: false,
            format: "iso".to_owned(),
            total_bytes: 10,
        }
    }

    // --- request validation ------------------------------------------------

    #[test]
    fn a_burn_verifies_by_reading_the_disc_back_unless_told_otherwise() {
        // The default has to be the strong one. Anything else makes skipping
        // verification the thing that happens when nobody chose.
        let validated = validate(&request()).expect("valid");
        assert_eq!(validated.verification_policy, vec!["full_sector_readback"]);
        assert!(
            VerificationStep::FullSectorReadback.reads_media_back(),
            "the default must actually read the disc"
        );
    }

    #[test]
    fn an_empty_verification_policy_is_refused_rather_than_defaulted() {
        // Sending `[]` is a caller asking for no verification. Silently
        // substituting the default would answer a different question than the
        // one asked; filling it in with nothing would skip read-back without
        // anyone saying so.
        let mut request = request();
        request.verification_policy = Some(vec![]);
        let problem = validate(&request).expect_err("must refuse");
        assert_eq!(problem.code, "VALIDATION_FAILED");
    }

    #[test]
    fn skipping_verification_must_be_said_out_loud() {
        // It is permitted, because an expert may want it, but only by naming
        // it.
        let mut request = request();
        request.verification_policy = Some(vec!["none".to_owned()]);
        let validated = validate(&request).expect("permitted when explicit");
        assert_eq!(validated.verification_policy, vec!["none"]);
    }

    #[test]
    fn an_unknown_verification_step_is_refused() {
        let mut request = request();
        request.verification_policy = Some(vec!["vibes".to_owned()]);
        assert!(validate(&request).is_err());
    }

    #[test]
    fn the_default_eject_policy_keeps_a_bad_disc_in_the_drive() {
        let validated = validate(&request()).expect("valid");
        assert_eq!(validated.eject_policy, EjectPolicy::EjectOnSuccess);
        assert!(!validated.eject_policy.ejects_after(false));
    }

    #[test]
    fn an_unknown_eject_policy_is_refused() {
        let mut request = request();
        request.eject_policy = Some("after_success".to_owned());
        assert!(
            validate(&request).is_err(),
            "a policy name this server does not implement must not be accepted"
        );
    }

    #[test]
    fn a_priority_outside_the_column_is_refused_rather_than_truncated() {
        // Truncating would silently reorder the queue.
        let mut request = request();
        request.priority = Some(i64::from(i16::MAX) + 1);
        assert!(validate(&request).is_err());

        request.priority = Some(i64::from(i16::MAX));
        assert_eq!(validate(&request).expect("valid").priority, i16::MAX);
    }

    #[test]
    fn malformed_identifiers_are_rejected_before_anything_is_queued() {
        let mut request = request();
        request.disc_id = "not-a-uuid".to_owned();
        assert_eq!(
            validate(&request).expect_err("must refuse").code,
            "INVALID_PARAMETER"
        );
    }

    // --- what may be burned ------------------------------------------------

    #[test]
    fn an_artifact_awaiting_deletion_is_not_burned() {
        // The operator has already said these bytes should go. Spending a
        // disc on them would be the system arguing with them.
        let mut facts = facts();
        facts.pending_deletion = true;
        assert_eq!(
            eligibility(&facts).expect_err("must refuse").code,
            "VALIDATION_FAILED"
        );
    }

    #[test]
    fn a_quarantined_artifact_is_not_burned() {
        for state in ["pending", "confirmed"] {
            let mut facts = facts();
            facts.quarantine_state = state.to_owned();
            assert!(
                eligibility(&facts).is_err(),
                "{state} quarantine must refuse"
            );
        }
        // Cleared quarantine is not quarantine.
        let mut facts = facts();
        facts.quarantine_state = "cleared".to_owned();
        assert!(eligibility(&facts).is_ok());
    }

    #[test]
    fn an_imperfect_artifact_is_burned_with_a_warning_rather_than_refused() {
        // An operator burning a known-faulty image is doing something the
        // system should support and describe honestly. Refusing would send
        // them to a tool that says nothing at all.
        let mut facts = facts();
        facts.validation_state = "invalid".to_owned();
        let warnings = eligibility(&facts).expect("permitted");
        assert!(
            warnings.iter().any(|line| line.contains("structural")),
            "{warnings:?}"
        );
    }

    #[test]
    fn every_burn_says_that_target_compatibility_is_unknown() {
        // The claim the project refuses to make. Silence would let an
        // operator infer it.
        let warnings = eligibility(&facts()).expect("permitted");
        assert!(
            warnings
                .iter()
                .any(|line| line.contains("target compatibility is unknown")),
            "{warnings:?}"
        );
    }

    // --- headers -----------------------------------------------------------

    #[test]
    fn an_absent_idempotency_key_is_not_an_error() {
        assert_eq!(idempotency_key(&HeaderMap::new()).expect("ok"), None);
    }

    #[test]
    fn an_oversized_idempotency_key_is_refused() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "idempotency-key",
            "k".repeat(MAX_IDEMPOTENCY_KEY + 1).parse().expect("header"),
        );
        assert!(idempotency_key(&headers).is_err());
    }

    #[test]
    fn a_usable_idempotency_key_is_returned_as_given() {
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "burn-42".parse().expect("header"));
        assert_eq!(
            idempotency_key(&headers).expect("ok"),
            Some("burn-42".to_owned())
        );
    }

    // --- refusals ----------------------------------------------------------

    #[test]
    fn each_retry_refusal_says_what_to_do_instead() {
        assert!(retry_refusal(BurnJobState::Complete).contains("new burn job"));
        assert!(retry_refusal(BurnJobState::NeedsAttention).contains("unaccounted for"));
        assert!(retry_refusal(BurnJobState::Writing).contains("still running"));
    }

    #[test]
    fn a_limit_is_clamped_rather_than_rejected() {
        assert_eq!(limit_of(None), DEFAULT_LIMIT);
        assert_eq!(limit_of(Some(0)), 1);
        assert_eq!(limit_of(Some(usize::MAX)), MAX_LIMIT);
    }
}
