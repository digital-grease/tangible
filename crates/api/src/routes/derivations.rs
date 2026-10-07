// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Derivatives: asking for one, following the job, and reading lineage.
//!
//! Asking is idempotent by fingerprint. The same request for the same parent
//! returns the derivative already made, or the job already making it, rather
//! than doing the work twice; a newer tool version is a different request.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tangible_db::derivations::{
    CancelOutcome, DerivationJobRecord, DerivationRecord, DerivationRequestOutcome,
    NewDerivationJob, cancel_derivation_job, derivation_jobs_for, get_derivation_job, lineage,
    request_derivation,
};
use tangible_domain::derivation::ChdOptions;
use tangible_domain::{ArtifactId, ChdCodec, DerivationJobId, Transformation};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use utoipa::ToSchema;

use crate::auth::SignedIn;
use crate::derivation::PlanError;
use crate::problem::{ErrorCode, Problem};
use crate::routes::artifacts::load;
use crate::state::ApiState;

fn rfc3339(value: OffsetDateTime) -> String {
    value.format(&Rfc3339).unwrap_or_default()
}

fn unavailable(what: &str) -> Problem {
    Problem::new(
        ErrorCode::StorageUnavailable,
        format!("{what}; see server logs"),
    )
}

fn parse_artifact(raw: &str) -> Result<ArtifactId, Problem> {
    raw.parse()
        .map_err(|_| Problem::invalid_parameter("artifact_id", "not a valid identifier"))
}

fn parse_job(raw: &str) -> Result<DerivationJobId, Problem> {
    raw.parse()
        .map_err(|_| Problem::invalid_parameter("job_id", "not a valid identifier"))
}

/// A derivation job.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DerivationJobView {
    /// The job.
    pub id: String,
    /// What it derives from.
    pub parent_artifact_id: String,
    /// `chd_create_cd` or `chd_create_dvd`.
    pub transformation: String,
    /// The tool.
    pub tool_name: String,
    /// The tool version the job was fingerprinted for.
    pub tool_version: String,
    /// The options, every default written out.
    #[schema(value_type = Object)]
    pub options: serde_json::Value,
    /// The fingerprint that makes a repeated request the same request.
    pub fingerprint: String,
    /// `queued`, `running`, `complete`, `failed_retryable`, `failed_terminal`
    /// or `canceled`.
    pub state: String,
    /// The derivative, once made.
    pub child_artifact_id: Option<String>,
    /// How many times a worker has taken it.
    pub attempts: i32,
    /// Why it last failed.
    pub error_code: Option<String>,
    /// Detail of the last failure.
    pub error_detail: Option<String>,
    /// Who asked.
    pub created_by: String,
    /// When it was asked for.
    pub created_at: String,
    /// When a worker first took it.
    pub started_at: Option<String>,
    /// When it finished.
    pub completed_at: Option<String>,
}

impl From<DerivationJobRecord> for DerivationJobView {
    fn from(job: DerivationJobRecord) -> Self {
        Self {
            id: job.id.to_string(),
            parent_artifact_id: job.parent_artifact_id.to_string(),
            transformation: job.transformation.to_string(),
            tool_name: job.tool_name,
            tool_version: job.tool_version,
            options: job.options,
            fingerprint: job.command_fingerprint,
            state: job.state.to_string(),
            child_artifact_id: job.child_artifact_id.map(|id| id.to_string()),
            attempts: job.attempts,
            error_code: job.error_code,
            error_detail: job.error_detail,
            created_by: job.created_by,
            created_at: rfc3339(job.created_at),
            started_at: job.started_at.map(rfc3339),
            completed_at: job.completed_at.map(rfc3339),
        }
    }
}

/// A derivation: the lineage between a parent and its derivative.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DerivationView {
    /// The parent.
    pub parent_artifact_id: String,
    /// The derivative.
    pub child_artifact_id: String,
    /// What was done.
    pub transformation: String,
    /// The tool.
    pub tool_name: String,
    /// Its version.
    pub tool_version: String,
    /// The options used.
    #[schema(value_type = Object)]
    pub options: serde_json::Value,
    /// The fingerprint.
    pub fingerprint: String,
    /// What the derivative preserved: `bit_exact_repack`,
    /// `structurally_equivalent`, `semantically_equivalent`, `lossy` or
    /// `unknown`.
    pub loss_character: String,
    /// When the work started.
    pub started_at: String,
    /// When it finished.
    pub completed_at: Option<String>,
}

impl From<DerivationRecord> for DerivationView {
    fn from(record: DerivationRecord) -> Self {
        Self {
            parent_artifact_id: record.parent_artifact_id.to_string(),
            child_artifact_id: record.child_artifact_id.to_string(),
            transformation: record.transformation,
            tool_name: record.tool_name,
            tool_version: record.tool_version,
            options: record.options,
            fingerprint: record.command_fingerprint,
            loss_character: record.loss_character.to_string(),
            started_at: rfc3339(record.started_at),
            completed_at: record.completed_at.map(rfc3339),
        }
    }
}

/// An artifact's lineage, and what could still be derived from it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LineageView {
    /// How it was made, when it is itself a derivative.
    pub derived_from: Option<DerivationView>,
    /// What has been derived from it.
    pub derivatives: Vec<DerivationView>,
    /// Derivation jobs asked of it, newest first.
    pub jobs: Vec<DerivationJobView>,
    /// The transformation that suits it, when this server can run one.
    pub suggested_transformation: Option<String>,
}

/// A request for a derivative.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestDerivation {
    /// `chd_create_cd` or `chd_create_dvd`. Omitted, the one that suits the
    /// artifact.
    pub transformation: Option<String>,
    /// Codecs, in the order chdman should try them. Omitted, chdman's
    /// defaults for the transformation.
    pub compression: Option<Vec<String>>,
    /// Bytes per hunk. Omitted, chdman's default.
    pub hunk_bytes: Option<u32>,
}

/// What a request came to.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DerivationRequestView {
    /// `queued` for a new job, `pending` when the same work is already
    /// queued or running, `exists` when the derivative was already made.
    pub result: String,
    /// The job, when one is queued or running.
    pub job: Option<DerivationJobView>,
    /// The derivation, when it already exists.
    pub derivation: Option<DerivationView>,
}

fn plan_problem(error: &PlanError) -> Problem {
    Problem::new(ErrorCode::ValidationFailed, error.to_string())
}

/// Ask for a derivative of an artifact.
///
/// # Errors
///
/// `NOT_FOUND` for an unknown artifact, `VALIDATION_FAILED` when nothing
/// applies or the options are refused, `CONFLICT` when this server runs no
/// derivation engine.
#[utoipa::path(
    post,
    path = "/api/v1/artifacts/{artifact_id}/derivatives",
    tag = "library",
    description = "Ask for a derivative, such as a CHD of a disc image. Idempotent: the same \
                   request for the same artifact returns the derivative already made (200) \
                   or the job already making it (202). The original is never changed.",
    params(("artifact_id" = String, Path, description = "Artifact identifier")),
    request_body = RequestDerivation,
    responses(
        (status = 200, description = "Already made", body = DerivationRequestView),
        (status = 202, description = "Queued, or already being made", body = DerivationRequestView),
        (status = 404, description = "No such artifact", body = Problem),
        (status = 409, description = "No derivation engine is configured", body = Problem),
        (status = 422, description = "Nothing applies, or the options are refused", body = Problem),
    ),
)]
pub async fn request_derivative(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(artifact_id): Path<String>,
    Json(request): Json<RequestDerivation>,
) -> Result<(StatusCode, Json<DerivationRequestView>), Problem> {
    let id = parse_artifact(&artifact_id)?;
    let Some(derivations) = state.derivations() else {
        return Err(Problem::new(
            ErrorCode::Conflict,
            "this server runs no derivation engine; see TANGIBLE_DERIVATION_ENGINE",
        ));
    };
    let parent = load(&state, id).await?;

    let transformation = request
        .transformation
        .as_deref()
        .map(|t| {
            t.parse::<Transformation>()
                .map_err(|_| Problem::invalid_parameter("transformation", "not a transformation"))
        })
        .transpose()?;
    let options = match (&request.compression, request.hunk_bytes) {
        (None, None) => None,
        (compression, hunk) => {
            let base = ChdOptions::defaults(
                transformation
                    .or_else(|| {
                        Transformation::suited_to(
                            parent.classification.format,
                            parent.classification.media_family.unwrap_or_default(),
                        )
                    })
                    .unwrap_or(Transformation::ChdCreateCd),
            );
            let compression = match compression {
                Some(list) => list
                    .iter()
                    .map(|c| c.parse::<ChdCodec>())
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| Problem::invalid_parameter("compression", "not a CHD codec"))?,
                None => base.compression,
            };
            Some(ChdOptions {
                compression,
                hunk_bytes: hunk.unwrap_or(base.hunk_bytes),
            })
        }
    };

    let pipeline = derivations.pipeline();
    let planned = pipeline
        .plan(&parent, transformation, options)
        .map_err(|e| plan_problem(&e))?;
    let outcome = request_derivation(
        state.database().pool(),
        &NewDerivationJob {
            parent_artifact_id: id,
            transformation: planned.spec.transformation,
            options: &planned.options_json,
            tool_name: &planned.spec.tool.name,
            tool_version: &planned.spec.tool.version,
            command_fingerprint: &planned.fingerprint,
            created_by: &user.username,
        },
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not queue a derivation");
        unavailable("the derivation could not be queued")
    })?;

    Ok(match outcome {
        DerivationRequestOutcome::Exists(record) => (
            StatusCode::OK,
            Json(DerivationRequestView {
                result: "exists".to_owned(),
                job: None,
                derivation: Some(record.into()),
            }),
        ),
        DerivationRequestOutcome::Pending(job) => (
            StatusCode::ACCEPTED,
            Json(DerivationRequestView {
                result: "pending".to_owned(),
                job: Some(job.into()),
                derivation: None,
            }),
        ),
        DerivationRequestOutcome::Queued(job) => {
            derivations.nudge();
            (
                StatusCode::ACCEPTED,
                Json(DerivationRequestView {
                    result: "queued".to_owned(),
                    job: Some(job.into()),
                    derivation: None,
                }),
            )
        }
    })
}

/// An artifact's lineage.
///
/// # Errors
///
/// `NOT_FOUND` for an unknown artifact.
#[utoipa::path(
    get,
    path = "/api/v1/artifacts/{artifact_id}/lineage",
    tag = "library",
    params(("artifact_id" = String, Path, description = "Artifact identifier")),
    responses(
        (status = 200, description = "Lineage", body = LineageView),
        (status = 404, description = "No such artifact", body = Problem),
    ),
)]
pub async fn artifact_lineage(
    State(state): State<ApiState>,
    Path(artifact_id): Path<String>,
) -> Result<Json<LineageView>, Problem> {
    let id = parse_artifact(&artifact_id)?;
    let manifest = load(&state, id).await?;
    let pool = state.database().pool();
    let read_failed = |error| {
        tracing::error!(error = ?error, "could not read lineage");
        unavailable("the lineage could not be read")
    };
    let found = lineage(pool, id).await.map_err(read_failed)?;
    let jobs = derivation_jobs_for(pool, id).await.map_err(read_failed)?;
    let suggested_transformation = state.derivations().and_then(|_| {
        Transformation::suited_to(
            manifest.classification.format,
            manifest.classification.media_family.unwrap_or_default(),
        )
        .map(|t| t.to_string())
    });
    Ok(Json(LineageView {
        derived_from: found.derived_from.map(Into::into),
        derivatives: found.derivatives.into_iter().map(Into::into).collect(),
        jobs: jobs.into_iter().map(Into::into).collect(),
        suggested_transformation,
    }))
}

/// One derivation job.
///
/// # Errors
///
/// `NOT_FOUND` for an unknown job.
#[utoipa::path(
    get,
    path = "/api/v1/derivation-jobs/{job_id}",
    tag = "library",
    params(("job_id" = String, Path, description = "Derivation job identifier")),
    responses(
        (status = 200, description = "The job", body = DerivationJobView),
        (status = 404, description = "No such job", body = Problem),
    ),
)]
pub async fn get_derivation_job_route(
    State(state): State<ApiState>,
    Path(job_id): Path<String>,
) -> Result<Json<DerivationJobView>, Problem> {
    let id = parse_job(&job_id)?;
    get_derivation_job(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read a derivation job");
            unavailable("the job could not be read")
        })?
        .map(|job| Json(job.into()))
        .ok_or_else(|| Problem::not_found("derivation job", &id.to_string()))
}

/// Withdraw a derivation job that has not started.
///
/// # Errors
///
/// `NOT_FOUND` for an unknown job, `CONFLICT` for one that is running or
/// finished.
#[utoipa::path(
    post,
    path = "/api/v1/derivation-jobs/{job_id}/cancel",
    tag = "library",
    params(("job_id" = String, Path, description = "Derivation job identifier")),
    responses(
        (status = 200, description = "Cancelled", body = DerivationJobView),
        (status = 404, description = "No such job", body = Problem),
        (status = 409, description = "Running or finished", body = Problem),
    ),
)]
pub async fn cancel_derivation_job_route(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(job_id): Path<String>,
) -> Result<Json<DerivationJobView>, Problem> {
    let id = parse_job(&job_id)?;
    match cancel_derivation_job(state.database().pool(), id, &user.username)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not cancel a derivation job");
            unavailable("the job could not be cancelled")
        })? {
        CancelOutcome::Canceled(job) => Ok(Json((*job).into())),
        CancelOutcome::NotFound => Err(Problem::not_found("derivation job", &id.to_string())),
        CancelOutcome::Refused(state) => Err(Problem::new(
            ErrorCode::Conflict,
            format!("a {state} job cannot be cancelled"),
        )),
    }
}

/// The derivation routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route(
            "/artifacts/{artifact_id}/derivatives",
            post(request_derivative),
        )
        .route("/artifacts/{artifact_id}/lineage", get(artifact_lineage))
        .route("/derivation-jobs/{job_id}", get(get_derivation_job_route))
        .route(
            "/derivation-jobs/{job_id}/cancel",
            post(cancel_derivation_job_route),
        )
}
