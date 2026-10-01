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
//! Enrollment is the single unauthenticated worker route, because the
//! enrollment token *is* the authentication. It is one-use and short-lived for
//! that reason.
//!
//! Issuing a token is an operator route rather than a worker one, and it is
//! unauthenticated too, for a different and temporary reason: operator
//! authentication does not exist yet, and every operator route is open to
//! whoever can reach the server. Until it does, the trade is a server that can
//! enroll a worker at all against one that anybody on its network could enroll
//! a worker on. The first was chosen; each issue is audited, the token
//! expires within the hour, and a worker it creates can be revoked.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::response::IntoResponse as _;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tangible_burn::worker::{RecoveryDirective, WorkerStage, plan_for};
use tangible_db::repositories::{
    AttemptCompletion, ClaimOutcome, DriveReport, EnrollmentOutcome, IncomingEvent, ReportedStage,
    abandon_prewrite_attempt, attempt_state_for_worker, authenticate_worker, claim_next_burn_job,
    complete_attempt, consume_enrollment, issue_enrollment, record_capabilities, record_events,
    record_heartbeat, renew_lease,
};
use tangible_domain::{BurnAttemptId, BurnAttemptState, DriveId, EjectPolicy, WorkerId};
use time::OffsetDateTime;
use utoipa::ToSchema;

use crate::problem::{ErrorCode, Problem};
use crate::state::ApiState;
use crate::worker_auth::{
    AuthRejection, EnrollmentToken, Secret, WorkerCredential, WorkerIdentity,
    bearer_worker_credential,
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

// --- issuing an enrollment token -------------------------------------------------

/// How long an enrollment token lasts when the request does not say.
///
/// Long enough to paste it into a worker's configuration and start the
/// container, short enough that one left in a terminal's scrollback is soon
/// worth nothing.
const DEFAULT_ENROLLMENT_MINUTES: u32 = 15;

/// The longest an enrollment token may last.
const MAX_ENROLLMENT_MINUTES: u32 = 60;

/// Who issued a token, as the audit log records it, until operators sign in.
const UNAUTHENTICATED_ACTOR: &str = "unauthenticated";

/// What an operator sends to issue an enrollment token. Every field is
/// optional; `{}` asks for the defaults.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct IssueEnrollmentRequest {
    /// Minutes until the token lapses, from 1 to 60. Defaults to 15.
    pub expires_in_minutes: Option<u32>,
}

/// A newly issued enrollment token.
#[derive(Debug, Serialize, ToSchema)]
pub struct IssuedEnrollment {
    /// The enrollment's identifier, for the audit log and for revoking it.
    pub enrollment_id: String,
    /// The token. Returned this once and never again: the server keeps only
    /// its hash.
    pub enrollment_token: String,
    /// When it lapses, RFC 3339.
    pub expires_at: String,
}

/// Issue a one-use enrollment token for a new burn worker.
///
/// Not idempotent, although the API design once listed it with the operations
/// that are. A token is shown once and stored only as a hash, so a retried
/// request cannot be given the same token back; it gets a new one, and the
/// first, never used, lapses on its own within the hour.
///
/// # Errors
///
/// `INVALID_PARAMETER` for a lifetime outside 1 to 60 minutes, and
/// `STORAGE_UNAVAILABLE` if the token cannot be generated or recorded.
#[utoipa::path(
    post,
    path = "/api/v1/worker-enrollments",
    tag = "workers",
    description = "Issue a one-use enrollment token for a new burn worker. The \
                   token is returned once and cannot be recovered; it expires \
                   after 15 minutes unless asked otherwise, at most 60. \
                   Unauthenticated until operator sign-in exists, and audited.",
    request_body = IssueEnrollmentRequest,
    responses(
        (status = 201, description = "Issued", body = IssuedEnrollment),
        (status = 400, description = "The lifetime is out of range", body = Problem),
    ),
)]
pub async fn issue_enrollment_token(
    State(state): State<ApiState>,
    Json(request): Json<IssueEnrollmentRequest>,
) -> Result<axum::response::Response, Problem> {
    let minutes = request
        .expires_in_minutes
        .unwrap_or(DEFAULT_ENROLLMENT_MINUTES);
    if !(1..=MAX_ENROLLMENT_MINUTES).contains(&minutes) {
        return Err(Problem::invalid_parameter(
            "expires_in_minutes",
            "must be from 1 to 60",
        ));
    }

    let token = EnrollmentToken::issue(
        OffsetDateTime::now_utc(),
        time::Duration::minutes(i64::from(minutes)),
    )
    .map_err(|error| {
        tracing::error!(error = ?error, "could not generate an enrollment token");
        Problem::new(
            ErrorCode::StorageUnavailable,
            "an enrollment token could not be generated",
        )
    })?;

    let enrollment_id = issue_enrollment(
        state.database().pool(),
        token.hash.as_str(),
        token.expires_at,
        UNAUTHENTICATED_ACTOR,
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not record an enrollment token");
        unavailable("the enrollment token could not be recorded")
    })?;

    // The identifier, never the token.
    tracing::info!(%enrollment_id, minutes, "worker enrollment token issued");

    let expires_at = token
        .expires_at
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    Ok((
        StatusCode::CREATED,
        Json(IssuedEnrollment {
            enrollment_id: enrollment_id.to_string(),
            // The one and only time this leaves the server.
            enrollment_token: token.secret.expose().to_owned(),
            expires_at,
        }),
    )
        .into_response())
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

// --- capabilities ----------------------------------------------------------------

/// One engine a worker can run.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct EngineReport {
    /// Engine name, such as `xorriso`.
    pub name: String,
    /// Its version, as the engine reports it.
    pub version: String,
}

/// What a worker says about one drive.
#[derive(Debug, Deserialize, ToSchema)]
pub struct DriveReportBody {
    /// Worker-local stable alias, such as `/dev/disc-block`.
    ///
    /// Supplied by the worker and never a host device node. Together with the
    /// worker it is the drive's identity, which is why the same alias
    /// reported twice updates one drive rather than creating a second.
    pub device_alias: String,
    /// Human-meaningful name an operator chose.
    pub configured_name: String,
    /// Reported vendor.
    #[serde(default)]
    pub vendor: Option<String>,
    /// Reported model.
    #[serde(default)]
    pub model: Option<String>,
    /// Reported firmware revision.
    #[serde(default)]
    pub firmware: Option<String>,
    /// SHA-256 of the drive serial, hashed by the worker.
    ///
    /// Hashed before it is sent, so the raw serial never leaves the machine
    /// that read it. The server only needs to recognise a drive, not to name
    /// its hardware in an export.
    #[serde(default)]
    pub serial_hash: Option<String>,
    /// Current drive status.
    pub status: String,
    /// What the drive says it can do.
    #[serde(default)]
    pub capabilities: serde_json::Value,
}

/// A worker's account of itself.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CapabilityReport {
    /// The worker's software version.
    pub software_version: String,
    /// Engines it can run.
    #[serde(default)]
    pub engines: Vec<EngineReport>,
    /// The drive it operates.
    pub drive: DriveReportBody,
    /// Free bytes in its staging cache.
    #[serde(default)]
    pub cache_free_bytes: Option<i64>,
}

/// What the server assigned.
#[derive(Debug, Serialize, ToSchema)]
pub struct CapabilityResponse {
    /// The drive's identifier, to be used when claiming work.
    ///
    /// Assigned by the server rather than supplied: public identifiers are
    /// the server's to issue, and a worker that could choose one could claim
    /// another worker's drive.
    pub drive_id: String,
}

/// Drive statuses a worker may report.
///
/// Matched here as well as in the column so a bad value is a clear refusal
/// rather than a constraint violation reported as a storage failure.
const REPORTABLE_DRIVE_STATUSES: &[&str] = &[
    "unknown",
    "ready_empty",
    "ready_with_media",
    "busy",
    "tray_open",
    "missing",
    "error",
    "disabled",
];

/// Longest free-text field accepted, matching the columns.
const MAX_DRIVE_FIELD: usize = 200;

/// Record what a worker and its drive can do.
///
/// Evidence, not permission. A drive claiming a profile may still fail to
/// write it, which is why preflight inspects the medium rather than trusting
/// this report.
///
/// Idempotent by worker and device alias: a restarting worker gets the same
/// drive identifier back instead of accumulating a drive row per restart and
/// leaving burn history pointing at hardware nobody can find.
///
/// # Errors
///
/// `NOT_FOUND` if the path names a different worker, or `VALIDATION_FAILED`
/// for a report the schema will not accept.
#[utoipa::path(
    put,
    path = "/api/v1/workers/{worker_id}/capabilities",
    tag = "workers",
    description = "Report what this worker and its drive can do, and receive \
                   the drive identifier to claim work with.",
    params(("worker_id" = String, Path, description = "Worker identifier")),
    request_body = CapabilityReport,
    responses(
        (status = 200, description = "Recorded", body = CapabilityResponse),
        (status = 401, description = "Not authenticated", body = Problem),
        (status = 404, description = "Not this worker", body = Problem),
        (status = 422, description = "The report will not be accepted", body = Problem),
    ),
    security(("worker_credential" = [])),
)]
pub async fn report_capabilities(
    State(state): State<ApiState>,
    identity: WorkerIdentity,
    Path(worker_id): Path<String>,
    Json(report): Json<CapabilityReport>,
) -> Result<Json<CapabilityResponse>, Problem> {
    let worker_id = same_worker(identity, &worker_id)?;

    let invalid = |detail: &str| Problem::new(ErrorCode::ValidationFailed, detail.to_owned());
    let bounded_field = |value: &str, name: &str| {
        if value.is_empty() || value.len() > MAX_DRIVE_FIELD {
            Err(invalid(&format!(
                "{name} must be between 1 and {MAX_DRIVE_FIELD} characters"
            )))
        } else {
            Ok(())
        }
    };

    bounded_field(&report.drive.device_alias, "device_alias")?;
    bounded_field(&report.drive.configured_name, "configured_name")?;
    bounded_field(&report.software_version, "software_version")?;
    for (value, name) in [
        (&report.drive.vendor, "vendor"),
        (&report.drive.model, "model"),
        (&report.drive.firmware, "firmware"),
    ] {
        if let Some(value) = value
            && value.len() > MAX_DRIVE_FIELD
        {
            return Err(invalid(&format!(
                "{name} must be at most {MAX_DRIVE_FIELD} characters"
            )));
        }
    }

    if !REPORTABLE_DRIVE_STATUSES.contains(&report.drive.status.as_str()) {
        return Err(invalid("that is not a drive status this server knows"));
    }

    if let Some(hash) = &report.drive.serial_hash
        && !(hash.len() == 64
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    {
        return Err(invalid(
            "serial_hash must be a lowercase hexadecimal SHA-256 digest",
        ));
    }

    // Capabilities are stored as an object. A bare array or string would be
    // accepted by JSONB and then confuse every reader of the column.
    let capabilities = if report.drive.capabilities.is_null() {
        serde_json::json!({})
    } else if report.drive.capabilities.is_object() {
        report.drive.capabilities.clone()
    } else {
        return Err(invalid("capabilities must be an object"));
    };

    let worker_capabilities = serde_json::json!({
        "engines": report
            .engines
            .iter()
            .map(|engine| serde_json::json!({ "name": engine.name, "version": engine.version }))
            .collect::<Vec<_>>(),
        "cache_free_bytes": report.cache_free_bytes,
    });

    let drive_id = record_capabilities(
        state.database().pool(),
        worker_id,
        &report.software_version,
        &worker_capabilities,
        DriveReport {
            device_alias: &report.drive.device_alias,
            configured_name: &report.drive.configured_name,
            vendor: report.drive.vendor.as_deref(),
            model: report.drive.model.as_deref(),
            firmware: report.drive.firmware.as_deref(),
            serial_hash: report.drive.serial_hash.as_deref(),
            status: &report.drive.status,
            capabilities: &capabilities,
        },
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not record capabilities");
        unavailable("the capability report could not be recorded")
    })?;

    tracing::info!(
        worker_id = %worker_id,
        drive_id = %drive_id,
        alias = %report.drive.device_alias,
        "capabilities recorded"
    );

    Ok(Json(CapabilityResponse {
        drive_id: drive_id.to_string(),
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

/// Where a worker fetches what it is to write.
#[derive(Debug, Serialize, ToSchema)]
pub struct ClaimedArtifact {
    /// The artifact.
    pub artifact_id: String,
    /// Path to fetch the manifest from, relative to the server.
    ///
    /// A path rather than an absolute URL: the worker already knows which
    /// server it is talking to, and a server that told it otherwise would be
    /// redirecting a staging download somewhere the operator never
    /// configured.
    pub manifest_url: String,
    /// Digest of the manifest as published, when the library is readable.
    ///
    /// The worker checks what it downloads against this. It does not make the
    /// manifest trustworthy (it came from the same server), but it catches a
    /// document altered or truncated between publication and the download.
    pub manifest_sha256: Option<String>,
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
    /// What to write, and where to fetch it.
    pub artifact: ClaimedArtifact,
    /// The lease token, presented on subsequent requests for this attempt.
    pub lease_token: String,
    /// When the lease lapses.
    pub lease_expires_at: String,
    /// Verification steps the job requires.
    pub verification_policy: Vec<String>,
    /// What to do with the disc when the attempt ends.
    pub eject_policy: String,
    /// Media profile the operator asked for, if any.
    ///
    /// Advisory until claims filter on drive capability: a worker whose drive
    /// cannot write this must fail preflight rather than write the wrong
    /// medium.
    pub requested_media_profile: Option<String>,
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
            // Absent rather than fabricated when the library is unreadable:
            // the worker then stages without the extra check instead of
            // comparing against a digest of nothing.
            let manifest_sha256 = match state.manifests() {
                None => None,
                Some(manifests) => {
                    match manifests
                        .digest(tangible_domain::ArtifactId::from_uuid(job.artifact_id))
                        .await
                    {
                        Ok(digest) => Some(digest.to_hex()),
                        Err(error) => {
                            tracing::warn!(
                                artifact_id = %job.artifact_id,
                                error = ?error,
                                "could not digest a manifest for a claim"
                            );
                            None
                        }
                    }
                }
            };

            Ok(Json(ClaimResponse {
                attempt_id: job.attempt_id.to_string(),
                burn_job_id: job.burn_job_id.to_string(),
                attempt_number: job.attempt_number,
                artifact: ClaimedArtifact {
                    artifact_id: job.artifact_id.to_string(),
                    manifest_url: format!(
                        "{}/artifacts/{}/manifest",
                        crate::API_BASE,
                        job.artifact_id
                    ),
                    manifest_sha256,
                },
                lease_token: lease.expose().to_owned(),
                lease_expires_at: job
                    .lease_expires_at
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default(),
                verification_policy: job.verification_policy,
                eject_policy: job.eject_policy,
                requested_media_profile: job.requested_media_profile,
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

/// Longest failure code stored, matching the column.
const MAX_ERROR_CODE: usize = 100;

/// Longest failure detail stored, matching the column.
const MAX_ERROR_DETAIL: usize = 10_000;

/// Truncate to a column's limit, on a character boundary.
///
/// Truncated rather than refused. Completion is reported after the disc is
/// burned, and failing it over an oversized message would lose the record of a
/// disc that physically exists.
fn bounded(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut end = limit;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
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

    // What the worker says it is doing, taken from the last event that names
    // a stage this build knows. Events arrive in sequence order, so the last
    // one is the most recent; a stage from an older release is skipped rather
    // than guessed at, which leaves the record where it was instead of moving
    // it somewhere invented.
    let reported = batch
        .events
        .iter()
        .rev()
        .find_map(|event| event.stage.parse::<WorkerStage>().ok())
        .and_then(|stage| stage.job_state().zip(stage.attempt_state()))
        .map(|(job, attempt)| ReportedStage { job, attempt });

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

    let through = record_events(state.database().pool(), attempt_id, &events, reported)
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
    /// What was checked on each track, for a disc described as tracks.
    #[serde(default)]
    pub tracks: Vec<TrackVerificationReport>,
}

/// The check on one track of a disc described as tracks.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct TrackVerificationReport {
    /// Track number.
    pub number: u32,
    /// `byte_compare`, or `length_and_readable` for a track that was read back
    /// and measured but not compared.
    pub check: String,
    /// `match`, `mismatch` or `unreadable`.
    pub outcome: String,
    /// Sectors the layout says the track proper holds.
    pub sectors_expected: u64,
    /// Sectors found on the disc.
    pub sectors_read: u64,
    /// The digest of what was written, for a byte comparison.
    #[serde(default)]
    pub expected_sha256: Option<String>,
    /// The digest of what was read back, for a byte comparison.
    #[serde(default)]
    pub observed_sha256: Option<String>,
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

/// Why an attempt did not succeed.
///
/// Sent alongside the reports rather than inferred from them, because the
/// reports say *what* happened and this says why. Without it a failed burn
/// tells an operator only that it failed.
#[derive(Debug, Deserialize, ToSchema)]
pub struct FailureReport {
    /// Stable machine-readable code, such as `PREFLIGHT_MEDIUM_NOT_BLANK`.
    pub code: String,
    /// Human-readable detail, bounded by the server before storage.
    #[serde(default)]
    pub detail: Option<String>,
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
    /// Why it failed, when it did.
    #[serde(default)]
    pub failure: Option<FailureReport>,
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
    // A report that says it matched while some track was only measured is
    // recorded as partial whatever it says: the record never claims more than
    // was checked, and that rule is the server's to keep, not each worker's.
    let verification = request.verification_report.as_ref().map(|report| {
        let only_measured = report
            .tracks
            .iter()
            .any(|track| track.check != "byte_compare");
        if report.state == "match" && only_measured {
            "partial"
        } else {
            report.state.as_str()
        }
    });

    // A write that never started consumed nothing. Checked first, because
    // every other arm below assumes the laser ran: recording a preflight
    // failure as a failed write would invent a disc that does not exist and
    // put it in the inventory to be hunted for.
    if request.write_report.state == "not_attempted" {
        return Resolved {
            state: BurnAttemptState::FailedBeforeWrite,
            copy_status: "unknown",
            verification_result: "not_performed",
            error_code: Some("WRITE_NOT_ATTEMPTED"),
        };
    }

    match (wrote, verification) {
        // The only clean success: written and read back matching.
        (true, Some("match")) => Resolved {
            state: BurnAttemptState::Verified,
            copy_status: "verified",
            verification_result: "passed",
            error_code: None,
        },
        // Written, and everything checked passed, but some tracks were only
        // read back and measured rather than compared: audio, or raw data
        // tracks. A good disc, recorded as exactly that.
        (true, Some("partial")) => Resolved {
            state: BurnAttemptState::Verified,
            copy_status: "verified",
            verification_result: "partial",
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

    let reported_code = request
        .failure
        .as_ref()
        .map(|failure| bounded(&failure.code, MAX_ERROR_CODE));
    let reported_detail = request
        .failure
        .as_ref()
        .and_then(|failure| failure.detail.as_ref())
        .map(|detail| bounded(detail, MAX_ERROR_DETAIL));

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
        // The worker's own code when it sent one: it knows which check
        // stopped it, and the resolved code only knows the shape of the
        // outcome. Both are bounded here because the columns are, and a
        // completion must never fail on a length: the disc is already burned.
        error_code: reported_code.as_deref().or(resolved.error_code),
        error_detail: reported_detail.as_deref(),
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

    // The operator's policy decides, not the route. The default ejects only a
    // verified disc, so one that failed verification stays where somebody will
    // find it rather than being handed back as though it were good; an
    // operator who asked for `always` gets always.
    let eject = recorded
        .eject_policy
        .parse::<EjectPolicy>()
        .unwrap_or_default()
        .ejects_after(resolved.state.is_success());

    Ok(Json(CompletionResponse {
        acknowledged: true,
        physical_copy_id: recorded.physical_copy_id.map(|id| id.to_string()),
        eject,
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

    // A discarded attempt that the server still holds open, short of a write,
    // is closed here. Without it the worker drops its record, the attempt
    // stays open on an expired lease, and the job is never claimable again.
    // Closing it fails the job rather than requeueing it; spending another
    // disc stays the operator's decision.
    if directive == RecoveryDirective::DiscardPrewriteState
        && server_state.is_some_and(|state| !state.is_terminal() && !state.consumed_media())
    {
        let closed = abandon_prewrite_attempt(state.database().pool(), attempt_id, worker_id)
            .await
            .map_err(|error| {
                // Retried by the worker. Answering "discard" without closing
                // the attempt would strand the job behind a reassuring reply.
                tracing::error!(error = ?error, "could not close an abandoned attempt");
                unavailable("the abandoned attempt could not be closed")
            })?;
        if closed {
            tracing::warn!(
                worker_id = %worker_id,
                attempt_id = %attempt_id,
                "closed an attempt its worker abandoned before writing; the job has failed \
                 and can be retried"
            );
        }
    }

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
        .route("/worker-enrollments", post(issue_enrollment_token))
        .route(
            "/worker-enrollments/consume",
            post(consume_enrollment_token),
        )
        .route(
            "/workers/{worker_id}/capabilities",
            axum::routing::put(report_capabilities),
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
                tracks: Vec::new(),
            }),
            physical_medium: PhysicalMedium {
                profile: "bd-r-25".to_owned(),
                manufacturer_id: None,
                serial: None,
            },
            failure: None,
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
    fn a_write_that_never_started_records_no_disc() {
        // A preflight that refused the medium, or a cancellation before the
        // laser ran. Recording it as a failed write would invent a disc and
        // put it in the inventory to be hunted for.
        let resolved = resolve(&completion("not_attempted", None));
        assert_eq!(resolved.state, BurnAttemptState::FailedBeforeWrite);
        assert!(!resolved.state.consumed_media());
        assert_eq!(resolved.error_code, Some("WRITE_NOT_ATTEMPTED"));
    }

    fn track(check: &str, outcome: &str) -> TrackVerificationReport {
        TrackVerificationReport {
            number: 1,
            check: check.to_owned(),
            outcome: outcome.to_owned(),
            sectors_expected: 300,
            sectors_read: 300,
            expected_sha256: None,
            observed_sha256: None,
        }
    }

    #[test]
    fn a_partial_verification_is_a_verified_disc_recorded_as_partial() {
        // Data compared, audio only measured. A good disc, and the
        // record says exactly how it was checked.
        let resolved = resolve(&completion("success", Some("partial")));
        assert_eq!(resolved.state, BurnAttemptState::Verified);
        assert_eq!(resolved.copy_status, "verified");
        assert_eq!(resolved.verification_result, "partial");
        assert_eq!(resolved.error_code, None);
    }

    #[test]
    fn a_match_that_only_measured_a_track_is_recorded_as_partial_anyway() {
        // The server keeps the rule, not each worker: a report claiming a
        // match while a track was only read back and measured is downgraded.
        let mut request = completion("success", Some("match"));
        if let Some(report) = request.verification_report.as_mut() {
            report.tracks = vec![
                track("byte_compare", "match"),
                track("length_and_readable", "match"),
            ];
        }
        assert_eq!(resolve(&request).verification_result, "partial");

        let mut compared = completion("success", Some("match"));
        if let Some(report) = compared.verification_report.as_mut() {
            report.tracks = vec![track("byte_compare", "match")];
        }
        assert_eq!(
            resolve(&compared).verification_result,
            "passed",
            "every track compared is a pass"
        );
    }

    #[test]
    fn a_write_that_never_started_is_not_rescued_by_a_verification_report() {
        // Whatever else the worker sends, no write means no disc.
        for verification in [None, Some("match"), Some("mismatch")] {
            let resolved = resolve(&completion("not_attempted", verification));
            assert!(
                !resolved.state.consumed_media(),
                "{verification:?} must not conjure a disc"
            );
        }
    }

    #[test]
    fn the_eject_decision_follows_the_operator_s_policy() {
        // The default keeps a disc that failed verification in the drive.
        assert!(EjectPolicy::EjectOnSuccess.ejects_after(true));
        assert!(!EjectPolicy::EjectOnSuccess.ejects_after(false));
        // An unknown policy falls back to the default rather than ejecting a
        // bad disc.
        assert_eq!(
            "melting".parse::<EjectPolicy>().unwrap_or_default(),
            EjectPolicy::EjectOnSuccess
        );
    }

    #[test]
    fn an_oversized_failure_message_is_truncated_rather_than_refused() {
        // Completion is reported after the disc is burned. Failing it over a
        // long message would lose the record of a disc that exists.
        let long = "x".repeat(MAX_ERROR_DETAIL + 500);
        assert_eq!(bounded(&long, MAX_ERROR_DETAIL).len(), MAX_ERROR_DETAIL);
        assert_eq!(bounded("short", MAX_ERROR_DETAIL), "short");
    }

    #[test]
    fn truncation_does_not_split_a_character() {
        // A message in any language must not be cut mid-codepoint, which
        // would make the stored text invalid rather than merely shorter.
        let text = "é".repeat(10);
        let cut = bounded(&text, 5);
        assert!(text.starts_with(&cut));
        assert!(cut.len() <= 5);
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
        for write in ["success", "failed", "not_attempted"] {
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
