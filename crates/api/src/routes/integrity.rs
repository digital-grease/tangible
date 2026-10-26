// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Integrity checks (docs/plans/storage-integrity.md): re-reading stored
//! objects against the digests recorded for them, for one artifact or for
//! the whole library.
//!
//! Asking returns at once; the server's integrity runner does the reading
//! and the check's view shows how far it has got. Asking again while one is
//! under way answers that one. Nothing a check finds is repaired.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tangible_db::integrity::{
    IntegrityCheckRecord, Requested, cancel_check, get_check, list_checks, request_check,
};
use tangible_domain::IntegrityCheckId;
use time::format_description::well_known::Rfc3339;
use utoipa::ToSchema;

use crate::auth::SignedIn;
use crate::pagination::{Page, PageQuery};
use crate::problem::{ErrorCode, Problem};
use crate::state::ApiState;

/// A problem a check found.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct IntegrityProblemView {
    /// `missing`, `corrupt`, `size_mismatch`, `unreadable`,
    /// `manifest_mismatch` or `manifest_unreadable`.
    pub kind: String,
    /// The object, for a problem with one.
    pub sha256: Option<String>,
    /// What was found, in words.
    pub detail: String,
}

/// An integrity check.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct IntegrityCheckView {
    /// Its id.
    pub id: String,
    /// `artifact` or `all`.
    pub scope: String,
    /// The artifact, for a check of one.
    pub artifact_id: Option<String>,
    /// `queued`, `running`, `complete`, `failed` or `canceled`.
    pub state: String,
    /// How many objects it reads, once counted.
    pub objects_total: Option<i64>,
    /// How many bytes it reads, once counted.
    pub bytes_total: Option<i64>,
    /// How many objects it has read.
    pub objects_checked: i64,
    /// How many bytes it has read.
    pub bytes_checked: i64,
    /// What it found, the first thousand.
    pub problems: Vec<IntegrityProblemView>,
    /// How many problems it found in all.
    pub problem_count: i32,
    /// Why it failed, for one that did.
    pub error_code: Option<String>,
    /// Why, in words.
    pub error_detail: Option<String>,
    /// Who asked: a username, or `scheduler`.
    pub created_by: String,
    /// When it was asked for.
    pub created_at: String,
    /// When it started.
    pub started_at: Option<String>,
    /// When it ended.
    pub completed_at: Option<String>,
}

fn rfc3339(at: time::OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap_or_default()
}

impl From<IntegrityCheckRecord> for IntegrityCheckView {
    fn from(record: IntegrityCheckRecord) -> Self {
        let problems = record
            .problems
            .as_array()
            .map(|problems| {
                problems
                    .iter()
                    .filter_map(|problem| {
                        Some(IntegrityProblemView {
                            kind: problem.get("kind")?.as_str()?.to_owned(),
                            sha256: problem
                                .get("sha256")
                                .and_then(|value| value.as_str())
                                .map(str::to_owned),
                            detail: problem
                                .get("detail")
                                .and_then(|value| value.as_str())
                                .unwrap_or_default()
                                .to_owned(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            id: record.id.to_string(),
            scope: record.scope.as_str().to_owned(),
            artifact_id: record.artifact_id.map(|id| id.to_string()),
            state: record.state.as_str().to_owned(),
            objects_total: record.objects_total,
            bytes_total: record.bytes_total,
            objects_checked: record.objects_checked,
            bytes_checked: record.bytes_checked,
            problems,
            problem_count: record.problem_count,
            error_code: record.error_code,
            error_detail: record.error_detail,
            created_by: record.created_by,
            created_at: rfc3339(record.created_at),
            started_at: record.started_at.map(rfc3339),
            completed_at: record.completed_at.map(rfc3339),
        }
    }
}

/// A page of checks, newest first.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct IntegrityCheckPage {
    /// The checks.
    pub items: Vec<IntegrityCheckView>,
    /// Cursor for the next page, or null when this is the last.
    pub next_cursor: Option<String>,
}

/// What an administrator sends to check the whole library.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestIntegrityCheck {
    /// `all`: every object the library holds. A check of one artifact is
    /// asked for with `POST /artifacts/{id}/validate`.
    pub scope: String,
}

fn unavailable(error: &tangible_db::DbError) -> Problem {
    tracing::error!(error = ?error, "integrity checks could not be read or written");
    Problem::new(
        ErrorCode::StorageUnavailable,
        "integrity checks could not be read or written",
    )
}

fn parse_id(raw: &str, what: &'static str) -> Result<uuid::Uuid, Problem> {
    uuid::Uuid::parse_str(raw)
        .map_err(|_| Problem::invalid_parameter(what, "not a valid identifier"))
}

/// Answer a request: 202 for a check queued now, 200 for one already under
/// way. A check queued now wakes the runner.
fn answered(
    state: &ApiState,
    requested: Requested,
    artifact: Option<uuid::Uuid>,
) -> Result<axum::response::Response, Problem> {
    match requested {
        Requested::Queued(check) => {
            if let Some(integrity) = state.integrity() {
                integrity.nudge();
            }
            tracing::info!(check_id = %check.id, scope = %check.scope, "integrity check queued");
            Ok((StatusCode::ACCEPTED, Json(IntegrityCheckView::from(check))).into_response())
        }
        Requested::AlreadyActive(check) => {
            Ok((StatusCode::OK, Json(IntegrityCheckView::from(check))).into_response())
        }
        Requested::NoSuchArtifact => Err(Problem::not_found(
            "artifact",
            &artifact.map(|id| id.to_string()).unwrap_or_default(),
        )),
    }
}

/// Check one artifact: every object it names read back against its digest,
/// and its manifest against the catalogue.
///
/// # Errors
///
/// `INVALID_PARAMETER` for a malformed id; `NOT_FOUND` for no such artifact
/// in the catalogue.
#[utoipa::path(
    post,
    path = "/api/v1/artifacts/{artifact_id}/validate",
    tag = "library",
    description = "Check an artifact's stored bytes: every object it names is read back and \\
                   compared with its digest, and its manifest with the catalogue. Returns \\
                   the check at once; the server reads in the background. Nothing found is \\
                   repaired.",
    params(("artifact_id" = String, Path, description = "Artifact identifier")),
    responses(
        (status = 202, description = "Queued", body = IntegrityCheckView),
        (status = 200, description = "A check of it is already under way", body = IntegrityCheckView),
        (status = 404, description = "No such artifact", body = Problem),
    ),
)]
pub async fn validate_artifact(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(artifact_id): Path<String>,
) -> Result<axum::response::Response, Problem> {
    let id = parse_id(&artifact_id, "artifact_id")?;
    let requested = request_check(state.database().pool(), Some(id), &user.username)
        .await
        .map_err(|error| unavailable(&error))?;
    answered(&state, requested, Some(id))
}

/// Check the whole library: every object read back against its digest.
///
/// # Errors
///
/// `VALIDATION_FAILED` for a scope other than `all`.
#[utoipa::path(
    post,
    path = "/api/v1/integrity-checks",
    tag = "library",
    description = "Check every stored object against its digest: a full scrub, reading every \\
                   byte the library holds. One runs at a time; asking while one is queued or \\
                   running answers that one.",
    request_body = RequestIntegrityCheck,
    responses(
        (status = 202, description = "Queued", body = IntegrityCheckView),
        (status = 200, description = "A full scrub is already under way", body = IntegrityCheckView),
        (status = 422, description = "A scope other than all", body = Problem),
    ),
)]
pub async fn request_scrub(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Json(request): Json<RequestIntegrityCheck>,
) -> Result<axum::response::Response, Problem> {
    if request.scope != "all" {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            "scope must be all; check one artifact with POST /artifacts/{id}/validate",
        ));
    }
    let requested = request_check(state.database().pool(), None, &user.username)
        .await
        .map_err(|error| unavailable(&error))?;
    answered(&state, requested, None)
}

/// Integrity checks, newest first.
///
/// # Errors
///
/// `INVALID_CURSOR` for a cursor this server did not issue.
#[utoipa::path(
    get,
    path = "/api/v1/integrity-checks",
    tag = "library",
    params(PageQuery),
    responses((status = 200, description = "Checks, newest first", body = IntegrityCheckPage)),
)]
pub async fn list_all_checks(
    State(state): State<ApiState>,
    Query(query): Query<PageQuery>,
) -> Result<Json<IntegrityCheckPage>, Problem> {
    let before = query
        .after()?
        .map(|cursor| {
            uuid::Uuid::parse_str(&cursor).map_err(|_| {
                Problem::new(ErrorCode::InvalidCursor, "that cursor is not one of these")
            })
        })
        .transpose()?;
    let limit = query.limit();
    let checks = list_checks(
        state.database().pool(),
        before,
        i64::try_from(limit).unwrap_or(i64::MAX),
    )
    .await
    .map_err(|error| unavailable(&error))?;
    let page = Page::new(
        checks.into_iter().map(IntegrityCheckView::from).collect(),
        limit,
        |check: &IntegrityCheckView| check.id.clone(),
    );
    Ok(Json(IntegrityCheckPage {
        items: page.items,
        next_cursor: page.next_cursor,
    }))
}

/// One integrity check, and how far it has got.
///
/// # Errors
///
/// `NOT_FOUND`.
#[utoipa::path(
    get,
    path = "/api/v1/integrity-checks/{check_id}",
    tag = "library",
    params(("check_id" = String, Path, description = "Check identifier")),
    responses(
        (status = 200, description = "The check", body = IntegrityCheckView),
        (status = 404, description = "No such check", body = Problem),
    ),
)]
pub async fn get_one_check(
    State(state): State<ApiState>,
    Path(check_id): Path<String>,
) -> Result<Json<IntegrityCheckView>, Problem> {
    let id = IntegrityCheckId::from_uuid(parse_id(&check_id, "check_id")?);
    let check = get_check(state.database().pool(), id)
        .await
        .map_err(|error| unavailable(&error))?
        .ok_or_else(|| Problem::not_found("integrity check", &id.to_string()))?;
    Ok(Json(IntegrityCheckView::from(check)))
}

/// Withdraw a check. One already ended is answered as it is.
///
/// # Errors
///
/// `NOT_FOUND`.
#[utoipa::path(
    post,
    path = "/api/v1/integrity-checks/{check_id}/cancel",
    tag = "library",
    params(("check_id" = String, Path, description = "Check identifier")),
    responses(
        (status = 200, description = "Withdrawn, or already ended", body = IntegrityCheckView),
        (status = 404, description = "No such check", body = Problem),
    ),
)]
pub async fn cancel_one_check(
    State(state): State<ApiState>,
    Path(check_id): Path<String>,
) -> Result<Json<IntegrityCheckView>, Problem> {
    let id = IntegrityCheckId::from_uuid(parse_id(&check_id, "check_id")?);
    let check = cancel_check(state.database().pool(), id)
        .await
        .map_err(|error| unavailable(&error))?
        .ok_or_else(|| Problem::not_found("integrity check", &id.to_string()))?;
    Ok(Json(IntegrityCheckView::from(check)))
}

/// The integrity routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/artifacts/{artifact_id}/validate", post(validate_artifact))
        .route(
            "/integrity-checks",
            get(list_all_checks).post(request_scrub),
        )
        .route("/integrity-checks/{check_id}", get(get_one_check))
        .route(
            "/integrity-checks/{check_id}/cancel",
            post(cancel_one_check),
        )
}
