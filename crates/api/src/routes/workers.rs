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
use tangible_db::repositories::{
    ClaimOutcome, EnrollmentOutcome, IncomingEvent, authenticate_worker, claim_next_burn_job,
    consume_enrollment, record_events, record_heartbeat, renew_lease,
};
use tangible_domain::{BurnAttemptId, DriveId, WorkerId};
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

/// The worker protocol routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route(
            "/worker-enrollments/consume",
            post(consume_enrollment_token),
        )
        .route("/workers/{worker_id}/heartbeat", post(heartbeat))
        .route("/workers/{worker_id}/claims", post(claim_work))
        .route("/burn-attempts/{attempt_id}/lease/renew", post(renew))
        .route("/burn-attempts/{attempt_id}/events", post(submit_events))
}
