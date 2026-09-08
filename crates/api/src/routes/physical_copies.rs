// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! The discs that actually exist.
//!
//! Every burn that consumed media produced one of these, including the burns
//! that failed. That is the point: an untracked ruined disc is the one that
//! gets shelved, found later, and trusted.
//!
//! What is editable here and what is not is the whole design. The burn that
//! produced a disc is history and cannot be rewritten. Its condition and its
//! whereabouts are not history (discs rot, get scratched, get lost and get
//! thrown away), so those an operator may record freely.
//!
//! With one exception. `verified` means a disc was read back and matched, and
//! only a recorded check may write it. Letting an operator type the word would
//! make the strongest claim in the system the cheapest one to make.

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tangible_db::repositories::{
    NewPhysicalCopyCheck, PhysicalCopyCheckRecord, PhysicalCopyEdit, PhysicalCopyFilter,
    PhysicalCopyOutcome, PhysicalCopyRecord, get_physical_copy, list_physical_copies,
    list_physical_copy_checks, record_physical_copy_check, update_physical_copy,
};
use tangible_domain::{ArtifactId, DiscId, PhysicalCopyId, PhysicalCopyStatus, VerificationStep};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use utoipa::{IntoParams, ToSchema};

use crate::pagination::{decode_cursor, encode_cursor};
use crate::problem::{ErrorCode, Problem};
use crate::state::ApiState;

/// Largest page a caller may ask for.
const MAX_LIMIT: usize = 500;

/// Default page size.
const DEFAULT_LIMIT: usize = 50;

/// Longest free-text field accepted, matching the columns.
const MAX_TEXT: usize = 500;

/// Longest note accepted, matching the column.
const MAX_NOTES: usize = 10_000;

/// Who a check is recorded as having been performed by.
const CHECKED_BY: &str = "operator";

fn rfc3339(value: OffsetDateTime) -> String {
    value.format(&Rfc3339).unwrap_or_default()
}

// --- views ---------------------------------------------------------------------

/// A disc in the inventory.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PhysicalCopyView {
    /// Opaque identifier.
    pub id: String,
    /// The catalog disc this is a copy of.
    pub disc_id: String,
    /// The artifact written to it.
    pub artifact_id: String,
    /// The attempt that produced it.
    pub burn_attempt_id: String,
    /// Its condition.
    pub status: String,
    /// Whether it should be taken out of circulation.
    ///
    /// Derived from the condition so a client does not have to know which
    /// conditions mean "bin this". It is the question an operator is really
    /// asking when they look at a shelf.
    pub should_be_destroyed: bool,
    /// Whether anything further can be recorded about it.
    pub is_settled: bool,
    /// Media profile of the medium.
    pub media_profile: String,
    /// Manufacturer identifier, when the drive reported one.
    pub manufacturer_id: Option<String>,
    /// Media serial, when the drive reported one.
    pub media_serial: Option<String>,
    /// What is written on the disc itself.
    pub label: Option<String>,
    /// Where it is kept.
    pub storage_location: Option<String>,
    /// What verification ran when it was burned.
    pub verification_level: String,
    /// How that turned out.
    pub verification_result: String,
    /// Operator's notes.
    pub notes: Option<String>,
    /// When it was burned.
    pub created_at: String,
    /// When the record last changed.
    pub updated_at: String,
    /// When it was last read back.
    pub last_checked_at: Option<String>,
    /// How many checks have been recorded.
    pub check_count: i64,
}

impl From<PhysicalCopyRecord> for PhysicalCopyView {
    fn from(record: PhysicalCopyRecord) -> Self {
        Self {
            id: record.id.to_string(),
            disc_id: record.disc_id.to_string(),
            artifact_id: record.artifact_id.to_string(),
            burn_attempt_id: record.burn_attempt_id.to_string(),
            status: record.status.to_string(),
            should_be_destroyed: record.status.should_be_destroyed(),
            is_settled: record.status.is_terminal(),
            media_profile: record.media_profile,
            manufacturer_id: record.manufacturer_id,
            media_serial: record.media_serial,
            label: record.label,
            storage_location: record.storage_location,
            verification_level: record.verification_level,
            verification_result: record.verification_result,
            notes: record.notes,
            created_at: rfc3339(record.created_at),
            updated_at: rfc3339(record.updated_at),
            last_checked_at: record.last_checked_at.map(rfc3339),
            check_count: record.check_count,
        }
    }
}

/// A page of discs.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PhysicalCopyPage {
    /// The discs in this page, newest first.
    pub items: Vec<PhysicalCopyView>,
    /// Cursor for the next page, or null when this is the last.
    pub next_cursor: Option<String>,
}

/// One check performed on a disc.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CheckView {
    /// Opaque identifier.
    pub id: String,
    /// What was done.
    pub method: String,
    /// What was found.
    pub result: String,
    /// The digest read back, when the method produces one.
    pub observed_sha256: Option<String>,
    /// How much was read.
    pub bytes_read: Option<i64>,
    /// Which drive read it.
    pub checked_with: Option<String>,
    /// Operator's notes.
    pub notes: Option<String>,
    /// Who checked.
    pub checked_by: String,
    /// When.
    pub checked_at: String,
}

impl From<PhysicalCopyCheckRecord> for CheckView {
    fn from(record: PhysicalCopyCheckRecord) -> Self {
        Self {
            id: record.id.to_string(),
            method: record.method,
            result: record.result,
            observed_sha256: record.observed_sha256,
            bytes_read: record.bytes_read,
            checked_with: record.checked_with,
            notes: record.notes,
            checked_by: record.checked_by,
            checked_at: rfc3339(record.checked_at),
        }
    }
}

/// A disc with its check history.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PhysicalCopyDetail {
    /// Summary fields.
    #[serde(flatten)]
    pub copy: PhysicalCopyView,
    /// Every check recorded, newest first.
    pub checks: Vec<CheckView>,
}

// --- requests ------------------------------------------------------------------

/// What an operator may change about a disc.
///
/// Every field is optional, and absent means "leave it alone", so a client
/// editing a label cannot blank a storage location it never saw.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct UpdatePhysicalCopyRequest {
    /// What is written on the disc.
    #[serde(default)]
    pub label: Option<String>,
    /// Where it is kept.
    #[serde(default)]
    pub storage_location: Option<String>,
    /// Operator's notes.
    #[serde(default)]
    pub notes: Option<String>,
    /// Its condition.
    ///
    /// `verified` and `verification_failed` are refused: those words describe
    /// what a read-back found, and recording a check is how they are written.
    #[serde(default)]
    pub status: Option<String>,
}

/// What a check found.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RecordCheckRequest {
    /// What was done, from the verification vocabulary.
    pub method: String,
    /// `passed`, `failed`, `partial`, or `not_performed`.
    pub result: String,
    /// The digest read back, when the method produces one.
    #[serde(default)]
    pub observed_sha256: Option<String>,
    /// How much was read.
    #[serde(default)]
    pub bytes_read: Option<i64>,
    /// Which drive read it.
    #[serde(default)]
    pub checked_with: Option<String>,
    /// Operator's notes.
    #[serde(default)]
    pub notes: Option<String>,
}

/// Query parameters for the listing.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct PhysicalCopyQuery {
    /// Maximum discs to return. Clamped.
    pub limit: Option<usize>,
    /// Opaque cursor from a previous response's `next_cursor`.
    pub cursor: Option<String>,
    /// Only copies of this catalog disc.
    pub disc_id: Option<String>,
    /// Only copies of this artifact.
    pub artifact_id: Option<String>,
    /// Only discs in this condition.
    pub status: Option<String>,
}

/// Results a check may report.
const CHECK_RESULTS: &[&str] = &["not_performed", "passed", "failed", "partial"];

fn limit_of(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

fn as_i64(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::try_from(DEFAULT_LIMIT).unwrap_or(50))
}

fn unavailable(detail: &str) -> Problem {
    Problem::new(ErrorCode::StorageUnavailable, detail)
}

fn parse_copy(raw: &str) -> Result<PhysicalCopyId, Problem> {
    raw.parse::<PhysicalCopyId>()
        .map_err(|_| Problem::invalid_parameter("copy_id", "not a valid identifier"))
}

/// Check the text fields against the limits the columns carry.
fn bounded(value: Option<&String>, name: &str, limit: usize) -> Result<(), Problem> {
    if let Some(value) = value
        && value.len() > limit
    {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            format!("{name} must be at most {limit} characters"),
        ));
    }
    Ok(())
}

/// Turn a refused update into a problem that explains itself.
fn refusal(id: PhysicalCopyId, outcome: PhysicalCopyOutcome) -> Problem {
    match outcome {
        PhysicalCopyOutcome::NotFound => Problem::not_found("physical copy", &id.to_string()),
        PhysicalCopyOutcome::Destroyed => Problem::new(
            ErrorCode::Conflict,
            "this disc has been destroyed; there is nothing left to record",
        ),
        PhysicalCopyOutcome::ConditionRefused { requested } => Problem::new(
            ErrorCode::ValidationFailed,
            format!(
                "{requested} describes what a read-back found; record a check instead of \
                 setting it by hand"
            ),
        ),
    }
}

// --- routes --------------------------------------------------------------------

/// List the discs this system has produced.
///
/// # Errors
///
/// `INVALID_PARAMETER` for a malformed filter or cursor.
#[utoipa::path(
    get,
    path = "/api/v1/physical-copies",
    tag = "physical-copies",
    description = "List discs, newest first. Filter by catalog disc, artifact, \
                   or condition.",
    params(PhysicalCopyQuery),
    responses(
        (status = 200, description = "A page of discs", body = PhysicalCopyPage),
        (status = 400, description = "Malformed filter or cursor", body = Problem),
    ),
)]
pub async fn list_copies(
    State(state): State<ApiState>,
    Query(query): Query<PhysicalCopyQuery>,
) -> Result<Json<PhysicalCopyPage>, Problem> {
    let limit = limit_of(query.limit);
    let before = match &query.cursor {
        None => None,
        Some(cursor) => Some(
            *decode_cursor(cursor)?
                .parse::<PhysicalCopyId>()
                .map_err(|_| Problem::new(ErrorCode::InvalidCursor, "the cursor is not usable"))?
                .as_uuid(),
        ),
    };

    let filter = PhysicalCopyFilter {
        disc_id: match &query.disc_id {
            None => None,
            Some(raw) => Some(
                *raw.parse::<DiscId>()
                    .map_err(|_| Problem::invalid_parameter("disc_id", "not a valid identifier"))?
                    .as_uuid(),
            ),
        },
        artifact_id: match &query.artifact_id {
            None => None,
            Some(raw) => Some(
                *raw.parse::<ArtifactId>()
                    .map_err(|_| {
                        Problem::invalid_parameter("artifact_id", "not a valid identifier")
                    })?
                    .as_uuid(),
            ),
        },
        status: match &query.status {
            None => None,
            Some(raw) => Some(raw.parse::<PhysicalCopyStatus>().map_err(|_| {
                Problem::invalid_parameter("status", "not a condition this server knows")
            })?),
        },
    };

    let copies = list_physical_copies(state.database().pool(), filter, as_i64(limit), before)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not list physical copies");
            unavailable("the disc inventory could not be read")
        })?;

    let next_cursor = if copies.len() < limit {
        None
    } else {
        copies
            .last()
            .map(|copy| encode_cursor(&copy.id.to_string()))
    };

    Ok(Json(PhysicalCopyPage {
        items: copies.into_iter().map(PhysicalCopyView::from).collect(),
        next_cursor,
    }))
}

async fn load(state: &ApiState, id: PhysicalCopyId) -> Result<PhysicalCopyRecord, Problem> {
    get_physical_copy(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read a physical copy");
            unavailable("the disc could not be read")
        })?
        .ok_or_else(|| Problem::not_found("physical copy", &id.to_string()))
}

async fn detail(state: &ApiState, id: PhysicalCopyId) -> Result<PhysicalCopyDetail, Problem> {
    let copy = load(state, id).await?;
    let checks = list_physical_copy_checks(state.database().pool(), id, 100)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read check history");
            unavailable("the check history could not be read")
        })?;
    Ok(PhysicalCopyDetail {
        copy: copy.into(),
        checks: checks.into_iter().map(CheckView::from).collect(),
    })
}

/// One disc, with every check recorded for it.
///
/// # Errors
///
/// `NOT_FOUND` if no such disc exists.
#[utoipa::path(
    get,
    path = "/api/v1/physical-copies/{copy_id}",
    tag = "physical-copies",
    description = "Fetch one disc and its check history, newest check first.",
    params(("copy_id" = String, Path, description = "Physical copy identifier")),
    responses(
        (status = 200, description = "The disc", body = PhysicalCopyDetail),
        (status = 404, description = "No such disc", body = Problem),
    ),
)]
pub async fn get_copy(
    State(state): State<ApiState>,
    Path(copy_id): Path<String>,
) -> Result<Json<PhysicalCopyDetail>, Problem> {
    let id = parse_copy(&copy_id)?;
    Ok(Json(detail(&state, id).await?))
}

/// Record what an operator knows about a disc.
///
/// # Errors
///
/// `NOT_FOUND`, `VALIDATION_FAILED` for a condition that only a check may
/// establish, or `CONFLICT` if the disc has been destroyed.
#[utoipa::path(
    patch,
    path = "/api/v1/physical-copies/{copy_id}",
    tag = "physical-copies",
    description = "Record a disc's label, whereabouts, notes or condition. \
                   Absent fields are left alone. `verified` cannot be set by \
                   hand: record a check.",
    params(("copy_id" = String, Path, description = "Physical copy identifier")),
    request_body = UpdatePhysicalCopyRequest,
    responses(
        (status = 200, description = "Recorded", body = PhysicalCopyDetail),
        (status = 404, description = "No such disc", body = Problem),
        (status = 409, description = "The disc has been destroyed", body = Problem),
        (status = 422, description = "That condition needs a check", body = Problem),
    ),
)]
pub async fn update_copy(
    State(state): State<ApiState>,
    Path(copy_id): Path<String>,
    Json(request): Json<UpdatePhysicalCopyRequest>,
) -> Result<Json<PhysicalCopyDetail>, Problem> {
    let id = parse_copy(&copy_id)?;
    bounded(request.label.as_ref(), "label", MAX_TEXT)?;
    bounded(
        request.storage_location.as_ref(),
        "storage_location",
        MAX_TEXT,
    )?;
    bounded(request.notes.as_ref(), "notes", MAX_NOTES)?;

    let status = match &request.status {
        None => None,
        Some(raw) => Some(raw.parse::<PhysicalCopyStatus>().map_err(|_| {
            Problem::new(
                ErrorCode::ValidationFailed,
                "that is not a condition this server knows",
            )
        })?),
    };

    update_physical_copy(
        state.database().pool(),
        id,
        PhysicalCopyEdit {
            label: request.label.as_deref(),
            storage_location: request.storage_location.as_deref(),
            notes: request.notes.as_deref(),
            status,
        },
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not update a physical copy");
        unavailable("the disc could not be updated")
    })?
    .map_err(|outcome| refusal(id, outcome))?;

    tracing::info!(physical_copy_id = %id, "physical copy updated");
    Ok(Json(detail(&state, id).await?))
}

/// Record a check performed on a disc.
///
/// This is the only way a disc becomes `verified`, and the only way it stops
/// being: the condition follows what the check found, in the same transaction,
/// so a check saying the disc failed can never sit next to a record still
/// calling it good.
///
/// # Errors
///
/// `NOT_FOUND`, `VALIDATION_FAILED` for an unknown method or result, or
/// `CONFLICT` if the disc has been destroyed.
#[utoipa::path(
    post,
    path = "/api/v1/physical-copies/{copy_id}/checks",
    tag = "physical-copies",
    description = "Record a check. The disc's condition follows what the check \
                   found: this is the only path to `verified`.",
    params(("copy_id" = String, Path, description = "Physical copy identifier")),
    request_body = RecordCheckRequest,
    responses(
        (status = 200, description = "Recorded", body = PhysicalCopyDetail),
        (status = 404, description = "No such disc", body = Problem),
        (status = 409, description = "The disc has been destroyed", body = Problem),
        (status = 422, description = "Unknown method or result", body = Problem),
    ),
)]
pub async fn record_check(
    State(state): State<ApiState>,
    Path(copy_id): Path<String>,
    Json(request): Json<RecordCheckRequest>,
) -> Result<Json<PhysicalCopyDetail>, Problem> {
    let id = parse_copy(&copy_id)?;

    // The same vocabulary as a burn's verification, because checking a
    // shelved disc and verifying a fresh one are the same act.
    request.method.parse::<VerificationStep>().map_err(|_| {
        Problem::new(
            ErrorCode::ValidationFailed,
            format!("{} is not a check this server knows", request.method),
        )
    })?;
    if !CHECK_RESULTS.contains(&request.result.as_str()) {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            "result must be one of not_performed, passed, failed or partial",
        ));
    }
    if let Some(digest) = &request.observed_sha256
        && !(digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            "observed_sha256 must be a lowercase hexadecimal SHA-256 digest",
        ));
    }
    bounded(request.checked_with.as_ref(), "checked_with", 200)?;
    bounded(request.notes.as_ref(), "notes", MAX_NOTES)?;

    let condition = record_physical_copy_check(
        state.database().pool(),
        id,
        NewPhysicalCopyCheck {
            method: &request.method,
            result: &request.result,
            observed_sha256: request.observed_sha256.as_deref(),
            bytes_read: request.bytes_read,
            checked_with: request.checked_with.as_deref(),
            notes: request.notes.as_deref(),
            checked_by: CHECKED_BY,
        },
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not record a check");
        unavailable("the check could not be recorded")
    })?
    .map_err(|outcome| refusal(id, outcome))?;

    tracing::info!(
        physical_copy_id = %id,
        result = %request.result,
        condition = condition.as_str(),
        "check recorded"
    );
    Ok(Json(detail(&state, id).await?))
}

/// Record that a disc has been destroyed.
///
/// Its own route rather than a condition in the update body, because it is the
/// one change that cannot be undone: a destroyed disc accepts nothing further.
/// The record stays so the history of what was burned remains true.
///
/// # Errors
///
/// `NOT_FOUND` if no such disc exists.
#[utoipa::path(
    post,
    path = "/api/v1/physical-copies/{copy_id}/mark-destroyed",
    tag = "physical-copies",
    description = "Record that a disc has been destroyed. Irreversible: the \
                   record remains, but nothing further can be recorded against \
                   it.",
    params(("copy_id" = String, Path, description = "Physical copy identifier")),
    responses(
        (status = 200, description = "Recorded", body = PhysicalCopyDetail),
        (status = 404, description = "No such disc", body = Problem),
    ),
)]
pub async fn mark_destroyed(
    State(state): State<ApiState>,
    Path(copy_id): Path<String>,
) -> Result<Json<PhysicalCopyDetail>, Problem> {
    let id = parse_copy(&copy_id)?;

    let outcome = update_physical_copy(
        state.database().pool(),
        id,
        PhysicalCopyEdit {
            status: Some(PhysicalCopyStatus::Destroyed),
            ..PhysicalCopyEdit::default()
        },
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not mark a disc destroyed");
        unavailable("the disc could not be updated")
    })?;

    // A disc already recorded as destroyed answers the same way as one just
    // recorded: what the operator asked for is true either way, and reporting
    // an error for it would invite them to press again.
    if let Err(other) = outcome
        && other != PhysicalCopyOutcome::Destroyed
    {
        return Err(refusal(id, other));
    }

    tracing::info!(physical_copy_id = %id, "physical copy marked destroyed");
    Ok(Json(detail(&state, id).await?))
}

/// The physical copy routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/physical-copies", get(list_copies))
        .route(
            "/physical-copies/{copy_id}",
            get(get_copy).patch(update_copy),
        )
        .route("/physical-copies/{copy_id}/checks", post(record_check))
        .route(
            "/physical-copies/{copy_id}/mark-destroyed",
            post(mark_destroyed),
        )
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_condition_only_a_check_can_establish_is_refused_with_the_remedy() {
        let problem = refusal(
            PhysicalCopyId::generate(),
            PhysicalCopyOutcome::ConditionRefused {
                requested: PhysicalCopyStatus::Verified,
            },
        );
        assert_eq!(problem.code, "VALIDATION_FAILED");
        assert!(
            problem.detail.contains("record a check"),
            "the refusal must say what to do instead: {}",
            problem.detail
        );
    }

    #[test]
    fn a_destroyed_disc_is_refused_as_a_conflict_rather_than_a_bad_request() {
        // The request was fine. The disc is gone.
        let problem = refusal(PhysicalCopyId::generate(), PhysicalCopyOutcome::Destroyed);
        assert_eq!(problem.code, "CONFLICT");
    }

    #[test]
    fn text_fields_are_bounded_to_what_the_columns_hold() {
        assert!(bounded(Some(&"x".repeat(MAX_TEXT)), "label", MAX_TEXT).is_ok());
        assert!(bounded(Some(&"x".repeat(MAX_TEXT + 1)), "label", MAX_TEXT).is_err());
        assert!(bounded(None, "label", MAX_TEXT).is_ok());
    }

    #[test]
    fn the_check_results_match_the_columns_constraint() {
        // The column has a CHECK listing exactly these. A result it refuses
        // would fail the insert rather than the review.
        assert_eq!(
            CHECK_RESULTS,
            &["not_performed", "passed", "failed", "partial"]
        );
    }

    #[test]
    fn a_limit_is_clamped_rather_than_rejected() {
        assert_eq!(limit_of(None), DEFAULT_LIMIT);
        assert_eq!(limit_of(Some(0)), 1);
        assert_eq!(limit_of(Some(usize::MAX)), MAX_LIMIT);
    }
}
