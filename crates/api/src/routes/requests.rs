// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Requests for items (ADR 0021, docs/plans/requests.md Q1).
//!
//! Anybody signed in may ask for an item, attesting on every request that
//! they own it and want a digital image of it; the attestation is checked
//! and not stored (the owner's choice). An operator or administrator
//! approves or declines; their own requests are approved as made. Nothing is
//! searched for or downloaded here: an approved request's releases are
//! searched for, and one grabbed, in `request_releases` (Q3).
//!
//! A viewer sees and withdraws only their own requests; another's is
//! answered as not found, so a viewer cannot learn what others asked for.

use std::collections::BTreeMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tangible_db::requests::{
    Decision, NewRequest, RequestFilter, RequestRecord, RequestRefusal, cancel_request,
    create_request, decide_request, get_request, list_requests,
};
use tangible_domain::auth::Permission;
use tangible_domain::{RequestId, RequestKind, RequestState};
use time::format_description::well_known::Rfc3339;
use utoipa::{IntoParams, ToSchema};

use crate::auth::{CurrentUser, SignedIn};
use crate::pagination::{DEFAULT_LIMIT, MAX_LIMIT, decode_cursor, encode_cursor};
use crate::problem::{ErrorCode, Problem};
use crate::state::ApiState;

/// Longest title, in characters.
pub const MAX_TITLE_CHARS: usize = 300;
/// Most details a request carries.
pub const MAX_DETAILS: usize = 8;
/// Longest detail name, in characters.
pub const MAX_DETAIL_NAME_CHARS: usize = 40;
/// Longest detail value, in characters.
pub const MAX_DETAIL_VALUE_CHARS: usize = 200;
/// Most references a request carries.
pub const MAX_REFS: usize = 16;
/// Longest external identifier, in characters.
pub const MAX_EXTERNAL_ID_CHARS: usize = 100;
/// Longest note or decline reason, in characters.
pub const MAX_NOTE_CHARS: usize = 2000;
/// The most bytes the details, or the references, may take as compact
/// JSON. The database allows 4096 as PostgreSQL writes them, with a space
/// after every colon and comma; this leaves room for those, so a request
/// within the bounds here is never refused as a database error.
pub const MAX_JSON_BYTES: usize = 3584;

/// Where an item's metadata came from: the providers of ADR 0021.
pub const PROVIDERS: [&str; 5] = ["dat", "musicbrainz", "tmdb", "igdb", "imdb"];

/// The wording the request form shows beside its checkbox (ADR 0021).
pub const ATTESTATION: &str = "I legally own this item and want a digital image of it.";

/// Where an item was found.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestRefView {
    /// `dat`, `musicbrainz`, `tmdb`, `igdb` or `imdb`.
    pub provider: String,
    /// The item's identifier there, such as `tt0083658` for IMDb.
    pub external_id: String,
}

/// A request for an item.
// The `can_*` flags are what the caller may do now, each decided on the
// server (AGENTS.md 9): independent answers, not states of one machine.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RequestView {
    /// Its id.
    pub id: String,
    /// Who asked.
    pub requested_by: String,
    /// `game`, `music`, `movie`, `television`, `software` or `other`.
    pub kind: String,
    /// The item, as a person sees it.
    pub title: String,
    /// Platform, year, region, edition and the like.
    pub details: BTreeMap<String, String>,
    /// Where the item was found.
    pub refs: Vec<RequestRefView>,
    /// What the requester added.
    pub note: Option<String>,
    /// `pending_approval`, `approved`, `declined`, `canceled`,
    /// `downloading`, `fulfilled` or `failed`.
    pub state: String,
    /// Who approved or declined it.
    pub decided_by: Option<String>,
    /// When.
    pub decided_at: Option<String>,
    /// Why it was declined.
    pub decline_reason: Option<String>,
    /// The download client a release went to.
    pub integration_id: Option<String>,
    /// The import following the download.
    pub import_id: Option<String>,
    /// What fulfilled it.
    pub artifact_id: Option<String>,
    /// Why the last download failed, as a stable code.
    pub error_code: Option<String>,
    /// And in words.
    pub error_detail: Option<String>,
    /// When it was asked for.
    pub created_at: String,
    /// When it last changed.
    pub updated_at: String,
    /// Whether the caller may approve it now.
    pub can_approve: bool,
    /// Whether the caller may decline it now.
    pub can_decline: bool,
    /// Whether the caller may withdraw it now.
    pub can_cancel: bool,
    /// Whether the caller may search for its releases, and grab one, now:
    /// an operator or administrator, once it is approved or its download
    /// failed.
    pub can_search: bool,
}

pub(crate) fn rfc3339(at: time::OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap_or_default()
}

impl RequestView {
    /// The request as `viewer` sees it, with what they may do to it.
    pub(crate) fn of(record: RequestRecord, viewer: &CurrentUser) -> Self {
        let decides = viewer.role.allows(Permission::DecideRequests);
        let own = record.requester_id == viewer.user_id;
        let details = record
            .details
            .as_object()
            .map(|details| {
                details
                    .iter()
                    .filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        let refs = serde_json::from_value(record.refs).unwrap_or_default();
        Self {
            id: record.id.to_string(),
            requested_by: record.requested_by,
            kind: record.kind.as_str().to_owned(),
            title: record.title,
            details,
            refs,
            note: record.note,
            state: record.state.as_str().to_owned(),
            decided_by: record.decided_by,
            decided_at: record.decided_at.map(rfc3339),
            decline_reason: record.decline_reason,
            integration_id: record.integration_id.map(|id| id.to_string()),
            import_id: record.import_id.map(|id| id.to_string()),
            artifact_id: record.artifact_id.map(|id| id.to_string()),
            error_code: record.error_code,
            error_detail: record.error_detail,
            created_at: rfc3339(record.created_at),
            updated_at: rfc3339(record.updated_at),
            can_approve: decides && record.state.can_become(RequestState::Approved),
            can_decline: decides && record.state.can_become(RequestState::Declined),
            can_cancel: (decides || own) && record.state.can_become(RequestState::Canceled),
            can_search: decides && record.state.may_grab(),
        }
    }
}

/// A page of requests, newest first.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RequestPage {
    /// The requests.
    pub items: Vec<RequestView>,
    /// Cursor for the next page, or null when this is the last.
    pub next_cursor: Option<String>,
}

/// What a person sends to ask for an item.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateItemRequest {
    /// `game`, `music`, `movie`, `television`, `software` or `other`.
    pub kind: String,
    /// The item, as a person sees it: up to 300 characters.
    pub title: String,
    /// Platform, year, region, edition and the like: up to eight, each a
    /// name of lowercase letters, digits and underscores, and a value of up
    /// to 200 characters.
    #[serde(default)]
    pub details: BTreeMap<String, String>,
    /// Where the item was found, up to sixteen.
    #[serde(default)]
    pub refs: Vec<RequestRefView>,
    /// Anything else, up to 2000 characters.
    #[serde(default)]
    pub note: Option<String>,
    /// That the requester legally owns the item and wants a digital image
    /// of it. Must be true; it is checked and not stored.
    #[serde(default)]
    pub attests_ownership: bool,
}

/// Why a request is declined.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeclineItemRequest {
    /// Shown to the requester: up to 2000 characters.
    pub reason: String,
}

/// Query parameters for the listing.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct RequestQuery {
    /// Maximum requests to return. Clamped.
    pub limit: Option<usize>,
    /// Opaque cursor from a previous response's `next_cursor`.
    pub cursor: Option<String>,
    /// Only requests in this state.
    pub state: Option<String>,
}

pub(crate) fn unavailable(error: &tangible_db::DbError) -> Problem {
    tracing::error!(error = ?error, "requests could not be read or written");
    Problem::new(
        ErrorCode::StorageUnavailable,
        "requests could not be read or written; see server logs",
    )
}

pub(crate) fn parse_id(raw: &str) -> Result<RequestId, Problem> {
    raw.parse()
        .map_err(|_| Problem::invalid_parameter("request_id", "not a valid identifier"))
}

pub(crate) fn invalid(detail: impl Into<String>) -> Problem {
    Problem::new(ErrorCode::ValidationFailed, detail)
}

/// Whether `text` holds a control character other than those `allowed`.
fn has_control(text: &str, allowed: &[char]) -> bool {
    text.chars()
        .any(|c| c.is_control() && !allowed.contains(&c))
}

/// A short single-line text, trimmed: present, bounded, no control
/// characters.
pub(crate) fn line(field: &str, raw: &str, max: usize) -> Result<String, Problem> {
    let text = raw.trim();
    if text.is_empty() {
        return Err(invalid(format!("{field} must not be empty")));
    }
    if text.chars().count() > max {
        return Err(invalid(format!("{field} is longer than {max} characters")));
    }
    if has_control(text, &[]) {
        return Err(invalid(format!("{field} must be one line of text")));
    }
    Ok(text.to_owned())
}

/// A longer text, trimmed: bounded, lines and tabs allowed, no other
/// control characters. Empty is none.
fn paragraph(field: &str, raw: &str) -> Result<Option<String>, Problem> {
    let text = raw.trim();
    if text.is_empty() {
        return Ok(None);
    }
    if text.chars().count() > MAX_NOTE_CHARS {
        return Err(invalid(format!(
            "{field} is longer than {MAX_NOTE_CHARS} characters"
        )));
    }
    if has_control(text, &['\n', '\r', '\t']) {
        return Err(invalid(format!("{field} holds control characters")));
    }
    Ok(Some(text.to_owned()))
}

/// A request's fields, checked: what the database stores.
#[derive(Debug, PartialEq)]
pub(crate) struct Checked {
    kind: RequestKind,
    title: String,
    details: serde_json::Value,
    refs: serde_json::Value,
    note: Option<String>,
}

/// A request's fields, checked as asking for one checks them.
///
/// # Errors
///
/// The problem asking would answer.
pub(crate) fn check(request: &CreateItemRequest) -> Result<Checked, Problem> {
    if !request.attests_ownership {
        return Err(Problem::new(
            ErrorCode::AttestationRequired,
            format!("a request must attest: \"{ATTESTATION}\""),
        ));
    }
    let kind: RequestKind = request
        .kind
        .parse()
        .map_err(|_| invalid("kind must be game, music, movie, television, software or other"))?;
    let title = line("title", &request.title, MAX_TITLE_CHARS)?;

    if request.details.len() > MAX_DETAILS {
        return Err(invalid(format!("at most {MAX_DETAILS} details")));
    }
    let mut details = serde_json::Map::new();
    for (name, value) in &request.details {
        let good_name = !name.is_empty()
            && name.len() <= MAX_DETAIL_NAME_CHARS
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !good_name {
            return Err(invalid(format!(
                "a detail's name is up to {MAX_DETAIL_NAME_CHARS} lowercase letters, digits \
                 and underscores"
            )));
        }
        let value = line(&format!("detail {name}"), value, MAX_DETAIL_VALUE_CHARS)?;
        details.insert(name.clone(), serde_json::Value::String(value));
    }

    if request.refs.len() > MAX_REFS {
        return Err(invalid(format!("at most {MAX_REFS} references")));
    }
    let mut refs = Vec::with_capacity(request.refs.len());
    for reference in &request.refs {
        if !PROVIDERS.contains(&reference.provider.as_str()) {
            return Err(invalid(format!(
                "a reference's provider is one of {}",
                PROVIDERS.join(", ")
            )));
        }
        let external_id = line("external_id", &reference.external_id, MAX_EXTERNAL_ID_CHARS)?;
        let entry = serde_json::json!({
            "provider": reference.provider,
            "external_id": external_id,
        });
        if !refs.contains(&entry) {
            refs.push(entry);
        }
    }

    let details = serde_json::Value::Object(details);
    let refs = serde_json::Value::Array(refs);
    // Characters bound each field; bytes bound the whole, since one
    // character may take four.
    for (field, json) in [("details", &details), ("refs", &refs)] {
        if json.to_string().len() > MAX_JSON_BYTES {
            return Err(invalid(format!(
                "{field} take more than {MAX_JSON_BYTES} bytes"
            )));
        }
    }

    let note = match &request.note {
        Some(note) => paragraph("note", note)?,
        None => None,
    };
    Ok(Checked {
        kind,
        title,
        details,
        refs,
        note,
    })
}

pub(crate) fn rate_limited(wait: std::time::Duration) -> Response {
    let mut response = Problem::new(
        ErrorCode::RateLimited,
        "too many requests from this account for now; try again later",
    )
    .into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(&wait.as_secs().max(1).to_string()) {
        response
            .headers_mut()
            .insert(axum::http::header::RETRY_AFTER, value);
    }
    response
}

/// Whose requests `user` may see or withdraw: their own, or everybody's for
/// somebody who decides them.
pub(crate) fn scope_of(user: &CurrentUser) -> Option<uuid::Uuid> {
    (!user.role.allows(Permission::DecideRequests)).then_some(user.user_id)
}

pub(crate) fn refused(refusal: RequestRefusal, id: RequestId, doing: &str) -> Problem {
    match refusal {
        RequestRefusal::NotFound => Problem::not_found("request", &id.to_string()),
        RequestRefusal::Conflict(state) => Problem::new(
            ErrorCode::Conflict,
            format!(
                "this request is {}, so it cannot be {doing}",
                state.as_str()
            ),
        ),
    }
}

/// Ask for an item.
///
/// # Errors
///
/// `ATTESTATION_REQUIRED` without the attestation; `VALIDATION_FAILED` for
/// a field out of bounds; `RATE_LIMITED` past 30 an hour.
#[utoipa::path(
    post,
    path = "/api/v1/requests",
    tag = "requests",
    description = "Ask for an item. The requester attests, on every request, that they legally \
                   own the item and want a digital image of it (`attests_ownership: true`); the \
                   attestation is checked and not stored. A viewer's request waits for an \
                   operator or administrator to approve it; theirs is approved as made. At most \
                   30 an hour per account.",
    request_body = CreateItemRequest,
    responses(
        (status = 201, description = "Asked for", body = RequestView),
        (status = 422, description = "Not attested (ATTESTATION_REQUIRED), or a field out of \
                                      bounds (VALIDATION_FAILED)", body = Problem),
        (status = 429, description = "Too many from this account for now", body = Problem),
    ),
)]
pub async fn create_one_request(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Json(request): Json<CreateItemRequest>,
) -> Result<Response, Problem> {
    let checked = check(&request)?;
    if let Err(wait) = state.request_limiter().try_acquire(&user.username) {
        return Ok(rate_limited(wait));
    }
    let record = create_request(
        state.database().pool(),
        &NewRequest {
            requester_id: user.user_id,
            requested_by: &user.username,
            requester_decides: user.role.allows(Permission::DecideRequests),
            kind: checked.kind,
            title: &checked.title,
            details: &checked.details,
            refs: &checked.refs,
            note: checked.note.as_deref(),
        },
    )
    .await
    .map_err(|error| unavailable(&error))?;
    tracing::info!(
        request_id = %record.id,
        kind = %record.kind.as_str(),
        state = %record.state.as_str(),
        by = %user.username,
        "item requested"
    );
    Ok((StatusCode::CREATED, Json(RequestView::of(record, &user))).into_response())
}

/// Requests, newest first.
///
/// # Errors
///
/// `INVALID_PARAMETER` for an unknown state; `INVALID_CURSOR`.
#[utoipa::path(
    get,
    path = "/api/v1/requests",
    tag = "requests",
    description = "Requests, newest first: a viewer's own, or everybody's for an operator or \
                   administrator.",
    params(RequestQuery),
    responses(
        (status = 200, description = "A page of requests", body = RequestPage),
        (status = 400, description = "Malformed filter or cursor", body = Problem),
    ),
)]
pub async fn list_all_requests(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Query(query): Query<RequestQuery>,
) -> Result<Json<RequestPage>, Problem> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let before = match &query.cursor {
        None => None,
        Some(cursor) => Some(
            *decode_cursor(cursor)?
                .parse::<RequestId>()
                .map_err(|_| Problem::new(ErrorCode::InvalidCursor, "the cursor is not usable"))?
                .as_uuid(),
        ),
    };
    let state_filter = match &query.state {
        None => None,
        Some(raw) => Some(raw.parse::<RequestState>().map_err(|_| {
            Problem::invalid_parameter("state", "not a request state this server knows")
        })?),
    };
    let records = list_requests(
        state.database().pool(),
        RequestFilter {
            requester_id: scope_of(&user),
            state: state_filter,
        },
        before,
        i64::try_from(limit).unwrap_or(i64::MAX),
    )
    .await
    .map_err(|error| unavailable(&error))?;
    let next_cursor = if records.len() < limit {
        None
    } else {
        records
            .last()
            .map(|record| encode_cursor(&record.id.to_string()))
    };
    Ok(Json(RequestPage {
        items: records
            .into_iter()
            .map(|record| RequestView::of(record, &user))
            .collect(),
        next_cursor,
    }))
}

/// One request.
///
/// # Errors
///
/// `NOT_FOUND`, also for another viewer's.
#[utoipa::path(
    get,
    path = "/api/v1/requests/{request_id}",
    tag = "requests",
    description = "One request. A viewer is answered only about their own.",
    params(("request_id" = String, Path, description = "Request identifier")),
    responses(
        (status = 200, description = "The request", body = RequestView),
        (status = 404, description = "No such request", body = Problem),
    ),
)]
pub async fn get_one_request(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(request_id): Path<String>,
) -> Result<Json<RequestView>, Problem> {
    let id = parse_id(&request_id)?;
    let record = get_request(state.database().pool(), id)
        .await
        .map_err(|error| unavailable(&error))?
        .filter(|record| scope_of(&user).is_none_or(|own| record.requester_id == own))
        .ok_or_else(|| Problem::not_found("request", &id.to_string()))?;
    Ok(Json(RequestView::of(record, &user)))
}

/// Approve a request, so a release for it may be searched for.
///
/// # Errors
///
/// `NOT_FOUND`; `CONFLICT` for one past waiting.
#[utoipa::path(
    post,
    path = "/api/v1/requests/{request_id}/approve",
    tag = "requests",
    description = "Approve a request that waits, so a release for it may be searched for. \
                   Operators and administrators.",
    params(("request_id" = String, Path, description = "Request identifier")),
    responses(
        (status = 200, description = "Approved", body = RequestView),
        (status = 404, description = "No such request", body = Problem),
        (status = 409, description = "It does not wait for approval", body = Problem),
    ),
)]
pub async fn approve_one_request(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(request_id): Path<String>,
) -> Result<Json<RequestView>, Problem> {
    let id = parse_id(&request_id)?;
    let record = decide_request(
        state.database().pool(),
        id,
        &user.username,
        Decision::Approve,
    )
    .await
    .map_err(|error| unavailable(&error))?
    .map_err(|refusal| refused(refusal, id, "approved"))?;
    tracing::info!(request_id = %id, by = %user.username, "request approved");
    Ok(Json(RequestView::of(record, &user)))
}

/// Decline a request, saying why.
///
/// # Errors
///
/// `VALIDATION_FAILED` without a reason; `NOT_FOUND`; `CONFLICT` for one
/// already downloading or ended.
#[utoipa::path(
    post,
    path = "/api/v1/requests/{request_id}/decline",
    tag = "requests",
    description = "Decline a request, waiting or approved, saying why; the requester sees the \
                   reason. Operators and administrators.",
    params(("request_id" = String, Path, description = "Request identifier")),
    request_body = DeclineItemRequest,
    responses(
        (status = 200, description = "Declined", body = RequestView),
        (status = 404, description = "No such request", body = Problem),
        (status = 409, description = "It is downloading or has ended", body = Problem),
        (status = 422, description = "No reason, or too long a one", body = Problem),
    ),
)]
pub async fn decline_one_request(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(request_id): Path<String>,
    Json(request): Json<DeclineItemRequest>,
) -> Result<Json<RequestView>, Problem> {
    let id = parse_id(&request_id)?;
    let reason = paragraph("reason", &request.reason)?
        .ok_or_else(|| invalid("a declined request says why"))?;
    let record = decide_request(
        state.database().pool(),
        id,
        &user.username,
        Decision::Decline(&reason),
    )
    .await
    .map_err(|error| unavailable(&error))?
    .map_err(|refusal| refused(refusal, id, "declined"))?;
    tracing::info!(request_id = %id, by = %user.username, "request declined");
    Ok(Json(RequestView::of(record, &user)))
}

/// Withdraw a request.
///
/// # Errors
///
/// `NOT_FOUND`, also for another viewer's; `CONFLICT` for one already
/// downloading or ended.
#[utoipa::path(
    post,
    path = "/api/v1/requests/{request_id}/cancel",
    tag = "requests",
    description = "Withdraw a request before its download starts, or after one failed. A viewer \
                   withdraws their own; operators and administrators, anybody's.",
    params(("request_id" = String, Path, description = "Request identifier")),
    responses(
        (status = 200, description = "Withdrawn", body = RequestView),
        (status = 404, description = "No such request", body = Problem),
        (status = 409, description = "It is downloading or has ended", body = Problem),
    ),
)]
pub async fn cancel_one_request(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(request_id): Path<String>,
) -> Result<Json<RequestView>, Problem> {
    let id = parse_id(&request_id)?;
    let record = cancel_request(state.database().pool(), id, &user.username, scope_of(&user))
        .await
        .map_err(|error| unavailable(&error))?
        .map_err(|refusal| refused(refusal, id, "withdrawn"))?;
    tracing::info!(request_id = %id, by = %user.username, "request withdrawn");
    Ok(Json(RequestView::of(record, &user)))
}

/// The request routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/requests", get(list_all_requests).post(create_one_request))
        .route("/requests/{request_id}", get(get_one_request))
        .route("/requests/{request_id}/approve", post(approve_one_request))
        .route("/requests/{request_id}/decline", post(decline_one_request))
        .route("/requests/{request_id}/cancel", post(cancel_one_request))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn asked() -> CreateItemRequest {
        CreateItemRequest {
            kind: "game".into(),
            title: "  Some Game  ".into(),
            details: BTreeMap::from([("platform".into(), "Sony PlayStation".into())]),
            refs: vec![RequestRefView {
                provider: "dat".into(),
                external_id: "Some Game (USA)".into(),
            }],
            note: Some("  ".into()),
            attests_ownership: true,
        }
    }

    fn code(result: Result<Checked, Problem>) -> String {
        result.expect_err("refused").code
    }

    #[test]
    fn a_request_is_trimmed_and_kept_to_what_the_database_stores() {
        let checked = check(&asked()).unwrap();
        assert_eq!(checked.kind, RequestKind::Game);
        assert_eq!(checked.title, "Some Game");
        assert_eq!(checked.details["platform"], "Sony PlayStation");
        assert_eq!(checked.refs[0]["provider"], "dat");
        assert_eq!(checked.note, None, "a blank note is none");

        // The same reference twice is kept once.
        let mut twice = asked();
        twice.refs.push(twice.refs[0].clone());
        assert_eq!(check(&twice).unwrap().refs.as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_request_without_the_attestation_is_refused_first() {
        let mut request = asked();
        request.attests_ownership = false;
        // Even with another field wrong, the attestation is what is said.
        request.kind = "book".into();
        assert_eq!(code(check(&request)), "ATTESTATION_REQUIRED");
    }

    #[test]
    fn every_field_is_bounded() {
        let refuse = |change: &dyn Fn(&mut CreateItemRequest)| {
            let mut request = asked();
            change(&mut request);
            code(check(&request))
        };
        assert_eq!(refuse(&|r| r.kind = "book".into()), "VALIDATION_FAILED");
        assert_eq!(refuse(&|r| r.title = " ".into()), "VALIDATION_FAILED");
        assert_eq!(
            refuse(&|r| r.title = "x".repeat(MAX_TITLE_CHARS + 1)),
            "VALIDATION_FAILED"
        );
        assert_eq!(refuse(&|r| r.title = "a\nb".into()), "VALIDATION_FAILED");
        assert_eq!(
            refuse(&|r| {
                r.details = (0..=MAX_DETAILS)
                    .map(|n| (format!("d{n}"), "x".into()))
                    .collect();
            }),
            "VALIDATION_FAILED"
        );
        assert_eq!(
            refuse(&|r| {
                r.details.insert("Platform".into(), "x".into());
            }),
            "VALIDATION_FAILED"
        );
        assert_eq!(
            refuse(&|r| {
                r.details
                    .insert("year".into(), "x".repeat(MAX_DETAIL_VALUE_CHARS + 1));
            }),
            "VALIDATION_FAILED"
        );
        assert_eq!(
            refuse(&|r| r.refs[0].provider = "imdb.com".into()),
            "VALIDATION_FAILED"
        );
        assert_eq!(
            refuse(&|r| r.refs[0].external_id = "x".repeat(MAX_EXTERNAL_ID_CHARS + 1)),
            "VALIDATION_FAILED"
        );
        assert_eq!(
            refuse(&|r| {
                r.refs = (0..=MAX_REFS)
                    .map(|n| RequestRefView {
                        provider: "tmdb".into(),
                        external_id: n.to_string(),
                    })
                    .collect();
            }),
            "VALIDATION_FAILED"
        );
        assert_eq!(
            refuse(&|r| r.note = Some("x".repeat(MAX_NOTE_CHARS + 1))),
            "VALIDATION_FAILED"
        );
        assert_eq!(
            refuse(&|r| r.note = Some("bell\u{7}".into())),
            "VALIDATION_FAILED"
        );
        // Lines in a note are fine.
        let mut lines = asked();
        lines.note = Some("first\nsecond".into());
        assert!(check(&lines).is_ok());
    }

    #[test]
    fn characters_that_take_four_bytes_cannot_overflow_the_database() {
        // Sixteen references at the character bound, each character four
        // bytes, would be over 6000 bytes: refused here, not by the
        // database's CHECK.
        let mut request = asked();
        request.refs = (0..MAX_REFS)
            .map(|n| RequestRefView {
                provider: "musicbrainz".into(),
                external_id: format!("{n:02}{}", "\u{1f4bf}".repeat(MAX_EXTERNAL_ID_CHARS - 2)),
            })
            .collect();
        assert_eq!(code(check(&request)), "VALIDATION_FAILED");
        let mut request = asked();
        request.details = (0..MAX_DETAILS)
            .map(|n| (format!("d{n}"), "\u{1f4bf}".repeat(MAX_DETAIL_VALUE_CHARS)))
            .collect();
        assert_eq!(code(check(&request)), "VALIDATION_FAILED");
    }
}
