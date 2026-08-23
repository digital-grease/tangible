// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! The burn-worker protocol surface.
//!
//! Enrollment, heartbeat, claiming, lease renewal and event submission.
//!
//! Every authenticated route takes a [`WorkerIdentity`] extracted from the
//! credential, and every route carrying a worker in its path checks that the
//! path matches the identity. Without that check a worker holding a valid
//! credential could drive somebody else's drive by editing a URL, which is the
//! oldest mistake in this shape of API and the one worth the most care here.
//!
//! Enrollment is the single unauthenticated route, because the enrollment
//! token *is* the authentication. It is one-use and short-lived for that
//! reason.

use axum::extract::{Path, State};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tangible_burn::worker::{RecoveryDirective, plan_for};
use tangible_db::repositories::{
    AttemptCompletion, ClaimOutcome, EnrollmentOutcome, IncomingEvent, attempt_state_for_worker,
    authenticate_worker, claim_next_burn_job, complete_attempt, consume_enrollment, record_events,
    record_heartbeat, renew_lease,
};
use tangible_domain::{BurnAttemptId, BurnAttemptState, DriveId, WorkerId};
use time::OffsetDateTime;
use utoipa::ToSchema;

use crate::problem::{ErrorCode, Problem};
use crate::state::ApiState;
use crate::worker_auth::{
    AuthRejection, Secret, WorkerCredential, WorkerIdentity, bearer_worker_credential,
};

/// How long a lease lasts.
///
/// Ninety seconds: long enough that a brief network hiccup does not cost a
/// worker its lease, short enough that a dead worker's drive frees up without
/// an operator intervening.
const LEASE_DURATION_SECONDS: i64 = 90;

/// How often a worker should report in.
const HEARTBEAT_INTERVAL_SECONDS: i64 = 20;

/// Authenticate a request as a worker.
///
/// Implemented as an extractor so a handler cannot be written without it: a
/// worker route that forgets authentication does not compile, because the
/// identity is the only way to reach the worker's own resources.
impl axum::extract::FromRequestParts<ApiState> for WorkerIdentity {
    type Rejection = Problem;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ApiState,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(AuthRejection::Missing)
            .map_err(unauthorized)?;

        let secret = bearer_worker_credential(header).map_err(unauthorized)?;

        let worker_id = authenticate_worker(state.database().pool(), secret.hash().as_str())
            .await
            .map_err(|error| {
                tracing::error!(error = ?error, "could not verify a worker credential");
                unavailable("the credential could not be verified")
            })?
            .ok_or(AuthRejection::Unrecognised)
            .map_err(unauthorized)?;

        Ok(Self { worker_id })
    }
}

/// A storage failure on a worker route.
///
/// Not [`Problem::storage_unavailable`], whose wording is about the artifact
/// library: an operator debugging a worker that cannot enroll should not be
/// sent to look at library mounts.
fn unavailable(detail: &str) -> Problem {
    Problem::new(ErrorCode::StorageUnavailable, detail)
}

/// Turn an authentication failure into a response.
///
/// The specific variant is logged; the caller gets one uniform message, so it
/// cannot distinguish "this credential once existed" from "it never did".
fn unauthorized(rejection: AuthRejection) -> Problem {
    tracing::warn!(rejection = ?rejection, "worker authentication failed");
    Problem::new(ErrorCode::Unauthenticated, rejection.public_reason())
}

/// Confirm the worker in the path is the one that authenticated.
///
/// A valid credential authorises acting as *that* worker and no other.
/// Returning not-found rather than forbidden is deliberate: telling a caller
/// that some other worker exists is information it has no business having.
fn same_worker(identity: WorkerIdentity, path_worker: &str) -> Result<WorkerId, Problem> {
    let requested = path_worker
        .parse::<WorkerId>()
        .map_err(|_| Problem::invalid_parameter("worker_id", "not a valid identifier"))?;

    if requested != identity.worker_id {
        tracing::warn!(
            authenticated = %identity.worker_id,
            requested = %requested,
            "a worker attempted to act as another worker"
        );
        return Err(Problem::not_found("worker", &requested.to_string()));
    }
    Ok(requested)
}

// --- enrollment ----------------------------------------------------------------

/// What a worker sends to enroll.
#[derive(Debug, Deserialize, ToSchema)]
pub struct EnrollmentRequest {
    /// The one-use token an administrator issued.
    pub enrollment_token: String,
    /// A human-meaningful name for this worker.
    pub name: String,
    /// Protocol versions the worker speaks.
    pub protocol_versions: Vec<String>,
    /// The worker's software version.
    pub software_version: String,
}

/// What a newly enrolled worker receives.
#[derive(Debug, Serialize, ToSchema)]
pub struct EnrollmentResponse {
    /// Its identity.
    pub worker_id: String,
    /// The protocol version the server selected.
    pub protocol_version: String,
    /// The credential, returned exactly once and never recoverable.
    pub credential: String,
    /// How often to report in.
    pub heartbeat_interval_seconds: i64,
    /// How long a lease lasts.
    pub lease_duration_seconds: i64,
}

/// Exchange a one-use enrollment token for a worker credential.
///
/// The only unauthenticated worker route: the token is the authentication.
///
/// # Errors
///
/// `UNAUTHENTICATED` for any unusable token, deliberately without saying
/// which way it was unusable, and `CONFLICT` if the name is taken. The token
/// is settled first, so the two are never distinguishable by name.
#[utoipa::path(
    post,
    path = "/api/v1/worker-enrollments/consume",
    tag = "workers",
    description = "Exchange a one-use enrollment token for a worker credential. \
                   The credential is returned once and cannot be recovered.",
    request_body = EnrollmentRequest,
    responses(
        (status = 200, description = "Enrolled", body = EnrollmentResponse),
        (status = 400, description = "No shared protocol version", body = Problem),
        (status = 401, description = "The token is not usable", body = Problem),
        (status = 409, description = "The name is already in use", body = Problem),
    ),
)]
pub async fn consume_enrollment_token(
    State(state): State<ApiState>,
    Json(request): Json<EnrollmentRequest>,
) -> Result<Json<EnrollmentResponse>, Problem> {
    // Negotiate before doing anything: enrolling a worker that cannot speak to
    // us wastes the token, which is one-use.
    let protocol_version = "1alpha1";
    if !request
        .protocol_versions
        .iter()
        .any(|version| version == protocol_version)
    {
        return Err(Problem::new(
            ErrorCode::UnsupportedProtocol,
            format!("this server speaks {protocol_version}"),
        ));
    }

    let supplied = Secret::from_supplied(request.enrollment_token);
    let credential = WorkerCredential::issue().map_err(|error| {
        tracing::error!(error = ?error, "could not generate a worker credential");
        Problem::new(
            ErrorCode::StorageUnavailable,
            "a credential could not be generated",
        )
    })?;

    let worker_id = consume_enrollment(
        state.database().pool(),
        supplied.hash().as_str(),
        &request.name,
        &request.software_version,
        protocol_version,
        credential.hash.as_str(),
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "enrollment failed");
        unavailable("the enrollment could not be recorded")
    })?
    .map_err(|outcome| match outcome {
        EnrollmentOutcome::TokenUnusable => {
            // One message whatever the reason. Distinguishing "already used"
            // from "unknown" would confirm a guessed token once existed.
            tracing::warn!("an enrollment token was rejected");
            Problem::new(
                ErrorCode::Unauthenticated,
                "the enrollment token is not valid",
            )
        }
        EnrollmentOutcome::NameTaken => {
            Problem::new(ErrorCode::Conflict, "another worker already uses that name")
        }
    })?;

    tracing::info!(worker_id = %worker_id, name = %request.name, "worker enrolled");

    Ok(Json(EnrollmentResponse {
        worker_id: worker_id.to_string(),
        protocol_version: protocol_version.to_owned(),
        // The one and only time this leaves the server.
        credential: credential.secret.expose().to_owned(),
        heartbeat_interval_seconds: HEARTBEAT_INTERVAL_SECONDS,
        lease_duration_seconds: LEASE_DURATION_SECONDS,
    }))
}

// --- heartbeat -------------------------------------------------------------------

/// What the server tells a worker on each heartbeat.
#[derive(Debug, Serialize, ToSchema)]
pub struct HeartbeatResponse {
    /// The server's clock, so a worker can notice drift.
    pub server_time: String,
    /// Whether the worker should stop taking new work.
    pub drain: bool,
}

/// Report that a worker is alive.
///
/// # Errors
///
/// `NOT_FOUND` if the path names a different worker than the credential.
#[utoipa::path(
    post,
    path = "/api/v1/workers/{worker_id}/heartbeat",
    tag = "workers",
    description = "Report that a worker is alive and receive any instruction \
                   to stop taking new work.",
    params(("worker_id" = String, Path, description = "Worker identifier")),
    responses(
        (status = 200, description = "Acknowledged", body = HeartbeatResponse),
        (status = 401, description = "Not authenticated", body = Problem),
        (status = 404, description = "Not this worker", body = Problem),
    ),
    security(("worker_credential" = [])),
)]
pub async fn heartbeat(
    State(state): State<ApiState>,
    identity: WorkerIdentity,
    Path(worker_id): Path<String>,
) -> Result<Json<HeartbeatResponse>, Problem> {
    let worker_id = same_worker(identity, &worker_id)?;

    let alive = record_heartbeat(state.database().pool(), worker_id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "heartbeat failed");
            unavailable("the heartbeat could not be recorded")
        })?;

    if !alive {
        // Revoked between authenticating and here, or revoked entirely.
        return Err(Problem::new(
            ErrorCode::Unauthenticated,
            "the credential is not valid",
        ));
    }

    Ok(Json(HeartbeatResponse {
        server_time: OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        drain: false,
    }))
}

// --- claiming --------------------------------------------------------------------

/// What a worker offers when asking for work.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ClaimRequest {
    /// The drive the work would run on.
    pub drive_id: String,
    /// The engine the worker would use.
    pub engine: String,
    /// That engine's version.
    pub engine_version: String,
}

/// A lease on some work.
#[derive(Debug, Serialize, ToSchema)]
pub struct ClaimResponse {
    /// The attempt created for this claim.
    pub attempt_id: String,
    /// The job.
    pub burn_job_id: String,
    /// Which attempt this is.
    pub attempt_number: i32,
    /// The artifact to write.
    pub artifact_id: String,
    /// The lease token, presented on subsequent requests for this attempt.
    pub lease_token: String,
    /// When the lease lapses.
    pub lease_expires_at: String,
    /// Verification steps the job requires.
    pub verification_policy: Vec<String>,
}

/// Ask for work.
///
/// Answers `204 No Content` when there is nothing to do, which is the common
/// case for a polling worker and should not read as an error.
///
/// # Errors
///
/// `NOT_FOUND` if the path names a different worker, `CONFLICT` if the drive
/// is already busy.
#[utoipa::path(
    post,
    path = "/api/v1/workers/{worker_id}/claims",
    tag = "workers",
    description = "Ask for a burn job. Answers 204 when there is nothing to do.",
    params(("worker_id" = String, Path, description = "Worker identifier")),
    request_body = ClaimRequest,
    responses(
        (status = 200, description = "Work leased", body = ClaimResponse),
        (status = 204, description = "Nothing to do"),
        (status = 409, description = "The drive is already busy", body = Problem),
        (status = 404, description = "Not this worker", body = Problem),
    ),
    security(("worker_credential" = [])),
)]
pub async fn claim_work(
    State(state): State<ApiState>,
    identity: WorkerIdentity,
    Path(worker_id): Path<String>,
    Json(request): Json<ClaimRequest>,
) -> Result<axum::response::Response, Problem> {
    use axum::response::IntoResponse as _;

    let worker_id = same_worker(identity, &worker_id)?;
    let drive_id = request
        .drive_id
        .parse::<DriveId>()
        .map_err(|_| Problem::invalid_parameter("drive_id", "not a valid identifier"))?;

    // The lease token is generated here and only its hash is stored, exactly
    // like a credential: the server never needs to reproduce it, only to
    // recognise it.
    let lease = Secret::generate("tgw_lease_").map_err(|error| {
        tracing::error!(error = ?error, "could not generate a lease token");
        Problem::new(
            ErrorCode::StorageUnavailable,
            "a lease could not be generated",
        )
    })?;

    let claimed = claim_next_burn_job(
        state.database().pool(),
        worker_id,
        drive_id,
        &request.engine,
        &request.engine_version,
        lease.hash().as_str(),
        LEASE_DURATION_SECONDS,
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "claim failed");
        unavailable("the queue could not be read")
    })?;

    match claimed {
        Err(ClaimOutcome::NoWork) => Ok(axum::http::StatusCode::NO_CONTENT.into_response()),
        Err(ClaimOutcome::DriveBusy) => Err(Problem::new(
            ErrorCode::Conflict,
            "this drive already has an active attempt",
        )),
        Ok(job) => {
            tracing::info!(
                worker_id = %worker_id,
                attempt_id = %job.attempt_id,
                burn_job_id = %job.burn_job_id,
                "burn job leased"
            );
            Ok(Json(ClaimResponse {
                attempt_id: job.attempt_id.to_string(),
                burn_job_id: job.burn_job_id.to_string(),
                attempt_number: job.attempt_number,
                artifact_id: job.artifact_id.to_string(),
                lease_token: lease.expose().to_owned(),
                lease_expires_at: job
                    .lease_expires_at
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default(),
                verification_policy: job.verification_policy,
            })
            .into_response())
        }
    }
}

// --- lease renewal ------------------------------------------------------------------

/// What a worker sends to renew.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RenewRequest {
    /// The lease token from the claim.
    pub lease_token: String,
}

/// The renewed lease.
#[derive(Debug, Serialize, ToSchema)]
pub struct RenewResponse {
    /// The new expiry.
    pub lease_expires_at: String,
}

/// Extend a lease.
///
/// A refusal does not stop a write already in progress. It only means the
/// worker may not claim anything further, and should reconcile when it
/// finishes what it holds.
///
/// # Errors
///
/// `NOT_FOUND` if the attempt is unknown, terminal, or the token does not
/// match.
#[utoipa::path(
    post,
    path = "/api/v1/burn-attempts/{attempt_id}/lease/renew",
    tag = "workers",
    description = "Extend a lease. A refusal does not interrupt a write \
                   already in progress; it only prevents claiming more work.",
    params(("attempt_id" = String, Path, description = "Attempt identifier")),
    request_body = RenewRequest,
    responses(
        (status = 200, description = "Renewed", body = RenewResponse),
        (status = 404, description = "No renewable attempt", body = Problem),
    ),
    security(("worker_credential" = [])),
)]
pub async fn renew(
    State(state): State<ApiState>,
    _identity: WorkerIdentity,
    Path(attempt_id): Path<String>,
    Json(request): Json<RenewRequest>,
) -> Result<Json<RenewResponse>, Problem> {
    let attempt_id = parse_attempt(&attempt_id)?;
    let token = Secret::from_supplied(request.lease_token);

    // The token is matched inside the query, so a renewal presented by
    // anything but the holder finds no row. That is the authorisation for this
    // route: holding the attempt's lease, not merely being some worker.
    let renewed = renew_lease(
        state.database().pool(),
        attempt_id,
        token.hash().as_str(),
        LEASE_DURATION_SECONDS,
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "lease renewal failed");
        unavailable("the lease could not be renewed")
    })?;

    renewed
        .map(|expires_at| {
            Json(RenewResponse {
                lease_expires_at: expires_at
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default(),
            })
        })
        .ok_or_else(|| Problem::not_found("renewable attempt", &attempt_id.to_string()))
}

// --- events ---------------------------------------------------------------------------

/// One event in a submitted batch.
#[derive(Debug, Deserialize, ToSchema)]
pub struct EventSubmission {
    /// Monotonic within the attempt.
    pub sequence: i64,
    /// Event type.
    pub event_type: String,
    /// Stage it relates to.
    pub stage: String,
    /// Stable machine-readable code.
    pub code: String,
    /// Progress from 0 to 1, when reported.
    pub progress: Option<f32>,
    /// Structured detail.
    #[serde(default)]
    pub data: serde_json::Value,
    /// When the worker observed it.
    pub worker_time: String,
}

/// A batch of events.
#[derive(Debug, Deserialize, ToSchema)]
pub struct EventBatch {
    /// The events, in sequence order.
    pub events: Vec<EventSubmission>,
}

/// What the server has persisted.
#[derive(Debug, Serialize, ToSchema)]
pub struct EventAck {
    /// The highest contiguous sequence held. The worker prunes against this.
    pub accepted_through_sequence: i64,
}

/// Largest batch accepted in one request.
///
/// Bounded so a worker cannot ask the server to absorb an unlimited backlog in
/// one transaction. A larger backlog simply takes several requests.
const MAX_EVENT_BATCH: usize = 512;

/// Submit worker events.
///
/// Idempotent by sequence: resubmitting a batch the server already holds
/// changes nothing and returns the same acknowledgement, which is what lets a
/// worker resend everything unacknowledged after an outage.
///
/// # Errors
///
/// `INVALID_PARAMETER` for a malformed attempt, timestamp, or oversized batch.
#[utoipa::path(
    post,
    path = "/api/v1/burn-attempts/{attempt_id}/events",
    tag = "workers",
    description = "Submit worker events. Idempotent by sequence: resubmitting \
                   a batch already held changes nothing.",
    params(("attempt_id" = String, Path, description = "Attempt identifier")),
    request_body = EventBatch,
    responses(
        (status = 200, description = "Accepted", body = EventAck),
        (status = 400, description = "Malformed batch", body = Problem),
    ),
    security(("worker_credential" = [])),
)]
pub async fn submit_events(
    State(state): State<ApiState>,
    _identity: WorkerIdentity,
    Path(attempt_id): Path<String>,
    Json(batch): Json<EventBatch>,
) -> Result<Json<EventAck>, Problem> {
    let attempt_id = parse_attempt(&attempt_id)?;

    if batch.events.len() > MAX_EVENT_BATCH {
        return Err(Problem::invalid_parameter(
            "events",
            &format!("at most {MAX_EVENT_BATCH} events per request"),
        ));
    }

    let mut events = Vec::with_capacity(batch.events.len());
    for event in batch.events {
        let worker_timestamp = OffsetDateTime::parse(
            &event.worker_time,
            &time::format_description::well_known::Rfc3339,
        )
        .map_err(|_| Problem::invalid_parameter("worker_time", "not an RFC 3339 timestamp"))?;

        events.push(IncomingEvent {
            sequence: event.sequence,
            event_type: event.event_type,
            stage: event.stage,
            message_code: event.code,
            // Clamped here rather than trusted: an engine parsing its own
            // output can produce nonsense, and the column has a CHECK that
            // would reject the whole batch.
            progress: event.progress.map(|value| value.clamp(0.0, 1.0)),
            data: event.data,
            worker_timestamp,
        });
    }

    let through = record_events(state.database().pool(), attempt_id, &events)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "recording events failed");
            unavailable("the events could not be recorded")
        })?;

    Ok(Json(EventAck {
        accepted_through_sequence: through,
    }))
}

fn parse_attempt(raw: &str) -> Result<BurnAttemptId, Problem> {
    raw.parse::<BurnAttemptId>()
        .map_err(|_| Problem::invalid_parameter("attempt_id", "not a valid identifier"))
}

// --- completion ---------------------------------------------------------------------

/// The engine's account of the write.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct WriteReport {
    /// `success` or a failure description.
    pub state: String,
    /// Which engine wrote.
    pub engine: String,
    /// That engine's version.
    pub engine_version: String,
    /// When writing began.
    pub started_at: String,
    /// When it ended.
    pub completed_at: String,
}

/// The read-back comparison, when one ran.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct VerificationReport {
    /// Which verification ran.
    pub policy: String,
    /// `match`, `mismatch`, or `skipped`.
    pub state: String,
    /// How much was read back.
    pub bytes_read: i64,
    /// The digest the artifact should have.
    pub expected_sha256: String,
    /// The digest actually read from the disc.
    pub observed_sha256: String,
}

/// What was in the drive.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct PhysicalMedium {
    /// Media profile, such as `bd-r-25`.
    pub profile: String,
    /// Manufacturer identifier, when the drive reported one.
    pub manufacturer_id: Option<String>,
    /// Media serial, when the drive reported one.
    pub serial: Option<String>,
}

/// What a worker sends when it finishes.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CompletionRequest {
    /// The lease token from the claim.
    pub lease_token: String,
    /// The last event sequence the worker emitted.
    pub last_sequence: i64,
    /// The engine's account of the write.
    pub write_report: WriteReport,
    /// The read-back comparison, when one ran.
    pub verification_report: Option<VerificationReport>,
    /// What was in the drive.
    pub physical_medium: PhysicalMedium,
}

/// What the server acknowledges.
#[derive(Debug, Serialize, ToSchema)]
pub struct CompletionResponse {
    /// Always true when the server has the completion recorded.
    pub acknowledged: bool,
    /// The disc recorded, when the attempt consumed media.
    pub physical_copy_id: Option<String>,
    /// Whether the drive should eject.
    pub eject: bool,
}

/// How a completion maps onto stored state.
///
/// One place for the whole mapping, so the attempt state, the disc's condition
/// and the recorded verification result cannot drift apart. Getting this wrong
/// is how a disc that does not hold what was intended ends up filed as a good
/// copy.
struct Resolved {
    state: BurnAttemptState,
    copy_status: &'static str,
    verification_result: &'static str,
    error_code: Option<&'static str>,
}

fn resolve(request: &CompletionRequest) -> Resolved {
    let wrote = request.write_report.state == "success";
    let verification = request
        .verification_report
        .as_ref()
        .map(|r| r.state.as_str());

    match (wrote, verification) {
        // The only clean success: written and read back matching.
        (true, Some("match")) => Resolved {
            state: BurnAttemptState::Verified,
            copy_status: "verified",
            verification_result: "passed",
            error_code: None,
        },
        // Physically written, holds the wrong bytes. The disc exists and must
        // be tracked so it can be destroyed rather than shelved.
        (true, Some("mismatch")) => Resolved {
            state: BurnAttemptState::VerificationFailed,
            copy_status: "verification_failed",
            verification_result: "failed",
            error_code: Some("VERIFICATION_MISMATCH"),
        },
        // Written but unverified. Recorded honestly as unverified rather than
        // assumed good, because nothing read it back.
        (true, _) => Resolved {
            state: BurnAttemptState::Written,
            copy_status: "produced_unverified",
            verification_result: "not_performed",
            error_code: None,
        },
        // The write failed. Media was being consumed when it did, so the disc
        // is real and probably ruined: still recorded, still trackable.
        (false, _) => Resolved {
            state: BurnAttemptState::WriteFailed,
            copy_status: "degraded",
            verification_result: "not_performed",
            error_code: Some("WRITE_FAILED"),
        },
    }
}

/// Record the end of an attempt.
///
/// Idempotent: a worker retrying after a lost response gets the same answer,
/// including the same physical copy, rather than recording a second disc.
///
/// # Errors
///
/// `NOT_FOUND` if no attempt with that identifier holds that lease.
#[utoipa::path(
    post,
    path = "/api/v1/burn-attempts/{attempt_id}/complete",
    tag = "workers",
    description = "Record the end of an attempt. Idempotent: retrying after a \
                   lost response returns the same physical copy rather than \
                   recording a second disc.",
    params(("attempt_id" = String, Path, description = "Attempt identifier")),
    request_body = CompletionRequest,
    responses(
        (status = 200, description = "Recorded", body = CompletionResponse),
        (status = 404, description = "No such leased attempt", body = Problem),
    ),
    security(("worker_credential" = [])),
)]
pub async fn complete(
    State(state): State<ApiState>,
    _identity: WorkerIdentity,
    Path(attempt_id): Path<String>,
    Json(request): Json<CompletionRequest>,
) -> Result<Json<CompletionResponse>, Problem> {
    let attempt_id = parse_attempt(&attempt_id)?;
    let lease = Secret::from_supplied(request.lease_token.clone());
    let resolved = resolve(&request);

    let write_report = serde_json::to_value(&request.write_report).map_err(|error| {
        tracing::error!(error = ?error, "could not serialize a write report");
        unavailable("the write report could not be stored")
    })?;
    let verify_report = request
        .verification_report
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|error| {
            tracing::error!(error = ?error, "could not serialize a verification report");
            unavailable("the verification report could not be stored")
        })?;

    let completion = AttemptCompletion {
        state: resolved.state,
        write_report: &write_report,
        verify_report: verify_report.as_ref(),
        media_profile: &request.physical_medium.profile,
        manufacturer_id: request.physical_medium.manufacturer_id.as_deref(),
        media_serial: request.physical_medium.serial.as_deref(),
        verification_level: request
            .verification_report
            .as_ref()
            .map_or("none", |report| report.policy.as_str()),
        verification_result: resolved.verification_result,
        copy_status: resolved.copy_status,
        error_code: resolved.error_code,
        error_detail: None,
    };

    let recorded = complete_attempt(
        state.database().pool(),
        attempt_id,
        lease.hash().as_str(),
        completion,
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "completion failed");
        unavailable("the completion could not be recorded")
    })?
    .map_err(|_| Problem::not_found("leased attempt", &attempt_id.to_string()))?;

    if recorded.already_completed {
        tracing::info!(attempt_id = %attempt_id, "a completed attempt was reported again");
    } else {
        tracing::info!(
            attempt_id = %attempt_id,
            state = resolved.state.as_str(),
            last_sequence = request.last_sequence,
            "burn attempt completed"
        );
    }

    Ok(Json(CompletionResponse {
        acknowledged: true,
        physical_copy_id: recorded.physical_copy_id.map(|id| id.to_string()),
        // Only on a clean success. A disc that failed verification should stay
        // where an operator will find it rather than be handed back as though
        // it were good.
        eject: resolved.state.is_success(),
    }))
}

// --- recovery -------------------------------------------------------------------------

/// What a worker reports when it restarts holding local state.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RecoveryRequest {
    /// The attempt the worker was running.
    pub attempt_id: String,
    /// The stage the worker believes it reached.
    pub local_stage: String,
    /// The last event sequence it emitted.
    pub last_event_sequence: i64,
    /// Whether the engine process is still running.
    pub engine_process_state: String,
}

/// What the worker must do.
#[derive(Debug, Serialize, ToSchema)]
pub struct RecoveryResponse {
    /// One of the protocol's recovery directives.
    pub directive: String,
    /// Whether local state may be dropped.
    pub discard_local_state: bool,
    /// Whether the worker may take new work.
    pub may_accept_new_work: bool,
    /// Always false. Restarting a write is never a recovery action.
    pub may_write: bool,
}

/// Decide what a recovering worker may do.
///
/// The governing rule is that no answer may cause a second write. Where the
/// worker's account and the server's disagree, the more dangerous reading
/// wins: a worker that might have written is never told to discard.
fn directive_for(
    server_state: Option<BurnAttemptState>,
    local_stage: Option<BurnAttemptState>,
    engine_running: bool,
) -> RecoveryDirective {
    // The worker's own claim to have started writing is enough to rule out a
    // discard, whatever the server believes. The server's record can lag: the
    // event carrying "writing" may never have arrived.
    let worker_may_have_written = local_stage.is_some_and(|stage| stage.consumed_media());

    match server_state {
        // The write is still going. Reattach and keep reporting.
        Some(BurnAttemptState::Writing) if engine_running => RecoveryDirective::ResumeReporting,
        // A write whose engine is gone. Whether the laser finished is
        // unknowable from here, and that is precisely the case where guessing
        // destroys a second disc.
        Some(BurnAttemptState::Writing) => RecoveryDirective::MarkNeedsAttention,
        Some(BurnAttemptState::Written | BurnAttemptState::Verifying) => {
            RecoveryDirective::ResumeVerification
        }
        // The server already holds the outcome, including any physical copy.
        // Nothing local adds to it. The directive's name speaks of pre-write
        // state, but what it grants (discard, accept new work, never write)
        // is exactly right here.
        Some(state) if state.is_terminal() => RecoveryDirective::DiscardPrewriteState,
        // Either the server has no record of this attempt for this worker, or
        // it has one that never reached a write. Both come down to the
        // worker's own account: if it never wrote, its local state is
        // meaningless and can go; if it may have, a human must reconcile a
        // disc the server cannot account for.
        None | Some(_) => {
            if worker_may_have_written {
                RecoveryDirective::MarkNeedsAttention
            } else {
                RecoveryDirective::DiscardPrewriteState
            }
        }
    }
}

/// Reconcile a worker that restarted holding local state.
///
/// The protocol also defines a `worker_revoked` directive. It is unreachable
/// here by construction: a revoked worker fails authentication and receives
/// 401 before any handler runs, which tells it the same thing sooner.
///
/// # Errors
///
/// `NOT_FOUND` if the path names a different worker.
#[utoipa::path(
    post,
    path = "/api/v1/workers/{worker_id}/recoveries",
    tag = "workers",
    description = "Reconcile a worker that restarted holding local state. No \
                   directive ever permits writing.",
    params(("worker_id" = String, Path, description = "Worker identifier")),
    request_body = RecoveryRequest,
    responses(
        (status = 200, description = "Directive issued", body = RecoveryResponse),
        (status = 404, description = "Not this worker", body = Problem),
    ),
    security(("worker_credential" = [])),
)]
pub async fn recover(
    State(state): State<ApiState>,
    identity: WorkerIdentity,
    Path(worker_id): Path<String>,
    Json(request): Json<RecoveryRequest>,
) -> Result<Json<RecoveryResponse>, Problem> {
    let worker_id = same_worker(identity, &worker_id)?;
    let attempt_id = parse_attempt(&request.attempt_id)?;

    let server_state = attempt_state_for_worker(state.database().pool(), attempt_id, worker_id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "recovery lookup failed");
            unavailable("the attempt could not be read")
        })?;

    // An unrecognised stage is treated as absent rather than rejected: the
    // worker is already in trouble, and the decision below defaults to the
    // cautious answer when it cannot tell.
    let local_stage = request.local_stage.parse::<BurnAttemptState>().ok();
    let engine_running = request.engine_process_state == "running";

    let directive = directive_for(server_state, local_stage, engine_running);
    let plan = plan_for(directive);

    tracing::info!(
        worker_id = %worker_id,
        attempt_id = %attempt_id,
        directive = ?directive,
        server_state = ?server_state,
        "recovery reconciled"
    );

    Ok(Json(RecoveryResponse {
        directive: serde_json::to_value(directive)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| "mark_needs_attention".to_owned()),
        discard_local_state: plan.discard_local_state,
        may_accept_new_work: plan.may_accept_new_work,
        may_write: plan.may_write,
    }))
}

/// The worker protocol routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route(
            "/worker-enrollments/consume",
            post(consume_enrollment_token),
        )
        .route("/workers/{worker_id}/heartbeat", post(heartbeat))
        .route("/workers/{worker_id}/claims", post(claim_work))
        .route("/workers/{worker_id}/recoveries", post(recover))
        .route("/burn-attempts/{attempt_id}/lease/renew", post(renew))
        .route("/burn-attempts/{attempt_id}/events", post(submit_events))
        .route("/burn-attempts/{attempt_id}/complete", post(complete))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn completion(write: &str, verification: Option<&str>) -> CompletionRequest {
        CompletionRequest {
            lease_token: "tgw_lease_x".to_owned(),
            last_sequence: 1,
            write_report: WriteReport {
                state: write.to_owned(),
                engine: "fake".to_owned(),
                engine_version: "0.1.0".to_owned(),
                started_at: "2026-01-01T00:00:00Z".to_owned(),
                completed_at: "2026-01-01T00:10:00Z".to_owned(),
            },
            verification_report: verification.map(|state| VerificationReport {
                policy: "full_sector_readback".to_owned(),
                state: state.to_owned(),
                bytes_read: 10,
                expected_sha256: "a".repeat(64),
                observed_sha256: "a".repeat(64),
            }),
            physical_medium: PhysicalMedium {
                profile: "bd-r-25".to_owned(),
                manufacturer_id: None,
                serial: None,
            },
        }
    }

    // --- completion mapping ------------------------------------------------

    #[test]
    fn only_a_verified_write_is_a_success() {
        assert_eq!(
            resolve(&completion("success", Some("match"))).state,
            BurnAttemptState::Verified
        );
        for outcome in ["mismatch", "skipped", "anything-else"] {
            assert_ne!(
                resolve(&completion("success", Some(outcome))).state,
                BurnAttemptState::Verified,
                "{outcome} must not read as a verified burn"
            );
        }
    }

    #[test]
    fn an_unverified_write_is_not_recorded_as_verified() {
        // Nothing read the disc back, so claiming it matches would be an
        // invention. It is recorded as produced-unverified instead.
        let resolved = resolve(&completion("success", None));
        assert_eq!(resolved.state, BurnAttemptState::Written);
        assert_eq!(resolved.copy_status, "produced_unverified");
        assert_eq!(resolved.verification_result, "not_performed");
    }

    #[test]
    fn a_mismatch_is_recorded_as_a_disc_that_exists_and_is_wrong() {
        // The distinction that matters: the disc is physically real, so it
        // must be trackable in order to be destroyed, but it must never be
        // filed as a good copy.
        let resolved = resolve(&completion("success", Some("mismatch")));
        assert_eq!(resolved.state, BurnAttemptState::VerificationFailed);
        assert!(resolved.state.consumed_media());
        assert!(!resolved.state.is_success());
        assert_eq!(resolved.copy_status, "verification_failed");
    }

    #[test]
    fn a_failed_write_still_records_a_disc() {
        // Media was being consumed when the write failed, so a physical disc
        // exists. An untracked ruined disc gets shelved and reused.
        let resolved = resolve(&completion("failed", None));
        assert_eq!(resolved.state, BurnAttemptState::WriteFailed);
        assert!(resolved.state.consumed_media());
        assert!(resolved.error_code.is_some());
    }

    #[test]
    fn every_resolution_agrees_with_the_physical_copy_trigger() {
        // The route decides whether a disc exists; the database enforces the
        // same judgement in require_completed_write. If they ever disagree,
        // completion fails at the constraint instead of at review.
        let permitted = [
            "writing",
            "written",
            "verifying",
            "verified",
            "verification_failed",
            "write_failed",
            "interrupted",
        ];
        for write in ["success", "failed"] {
            for verification in [None, Some("match"), Some("mismatch"), Some("skipped")] {
                let resolved = resolve(&completion(write, verification));
                if resolved.state.consumed_media() {
                    assert!(
                        permitted.contains(&resolved.state.as_str()),
                        "{} would be refused by the trigger",
                        resolved.state.as_str()
                    );
                }
            }
        }
    }

    // --- recovery ----------------------------------------------------------

    #[test]
    fn no_recovery_directive_ever_permits_writing() {
        // The one property the whole route exists to guarantee. A worker that
        // restarts cannot know whether the laser already ran.
        for server in BurnAttemptState::all() {
            for local in BurnAttemptState::all() {
                for running in [true, false] {
                    let directive = directive_for(Some(*server), Some(*local), running);
                    assert!(
                        !plan_for(directive).may_write,
                        "{server:?}/{local:?} produced a directive permitting a write"
                    );
                }
            }
        }
    }

    #[test]
    fn a_worker_that_may_have_written_is_never_told_to_discard() {
        // Discarding is only safe when nothing physical happened. The worker's
        // own account is enough to rule it out, because the server's record
        // can lag: the event carrying "writing" may never have arrived.
        for local in BurnAttemptState::all()
            .iter()
            .filter(|s| s.consumed_media())
        {
            for running in [true, false] {
                assert_ne!(
                    directive_for(None, Some(*local), running),
                    RecoveryDirective::DiscardPrewriteState,
                    "an unknown attempt with local stage {local:?} must not be discarded"
                );
                for server in [
                    BurnAttemptState::Claimed,
                    BurnAttemptState::Staging,
                    BurnAttemptState::Preflighting,
                ] {
                    assert_ne!(
                        directive_for(Some(server), Some(*local), running),
                        RecoveryDirective::DiscardPrewriteState,
                        "{server:?} disagreeing with local {local:?} must not be discarded"
                    );
                }
            }
        }
    }

    #[test]
    fn an_interrupted_write_with_no_engine_needs_attention() {
        assert_eq!(
            directive_for(
                Some(BurnAttemptState::Writing),
                Some(BurnAttemptState::Writing),
                false
            ),
            RecoveryDirective::MarkNeedsAttention
        );
    }

    #[test]
    fn a_live_write_resumes_reporting() {
        assert_eq!(
            directive_for(
                Some(BurnAttemptState::Writing),
                Some(BurnAttemptState::Writing),
                true
            ),
            RecoveryDirective::ResumeReporting
        );
    }

    #[test]
    fn a_finished_write_resumes_verification() {
        for state in [BurnAttemptState::Written, BurnAttemptState::Verifying] {
            assert_eq!(
                directive_for(Some(state), Some(state), false),
                RecoveryDirective::ResumeVerification
            );
        }
    }

    #[test]
    fn a_settled_attempt_lets_the_worker_move_on() {
        // The common case after a crash between completing and clearing local
        // state. The server already holds the outcome, so holding the worker
        // would strand a drive for nothing.
        for state in BurnAttemptState::all().iter().filter(|s| s.is_terminal()) {
            let directive = directive_for(Some(*state), Some(*state), false);
            assert_eq!(directive, RecoveryDirective::DiscardPrewriteState);
            assert!(plan_for(directive).may_accept_new_work);
        }
    }

    #[test]
    fn an_unparseable_local_stage_does_not_widen_permissions() {
        // A worker reporting a stage this server does not know is already in
        // trouble. Treating it as absent must not turn a cautious answer into
        // a permissive one for anything that consumed media.
        for server in BurnAttemptState::all()
            .iter()
            .filter(|s| s.consumed_media())
        {
            let directive = directive_for(Some(*server), None, false);
            assert!(!plan_for(directive).may_write);
        }
    }
}
