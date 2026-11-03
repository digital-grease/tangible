// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Reading the audit log (API.md 8.10, docs/plans/audit-log.md).
//!
//! Administrators only (`read_audit`), and read only: nothing here changes
//! or removes an event. Metadata is shown as it was stored, which is after
//! redaction (SECURITY.md).

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tangible_db::audit::{AuditEventRecord, AuditFilter, get_audit_event, list_audit_events};
use time::OffsetDateTime;
use utoipa::{IntoParams, ToSchema};

use crate::pagination::{Page, PageQuery};
use crate::problem::{ErrorCode, Problem};
use crate::state::ApiState;

/// Longest filter value accepted: the longest any column holds.
const MAX_FILTER_CHARS: usize = 200;

fn rfc3339(value: OffsetDateTime) -> String {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn unavailable(detail: &str) -> Problem {
    Problem::new(ErrorCode::StorageUnavailable, detail)
}

/// One audit event.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AuditEventView {
    /// Opaque identifier.
    pub id: String,
    /// When it was written.
    pub occurred_at: String,
    /// `user`, `worker`, `integration` or `system`.
    pub actor_type: String,
    /// Who: a username, a worker's or integration's identifier, or the name
    /// of a system task.
    pub actor_id: String,
    /// What, such as `worker.revoked`.
    pub action: String,
    /// What kind of thing it was done to.
    pub target_type: String,
    /// Which one.
    pub target_id: Option<String>,
    /// `success`, `failure` or `denied`.
    pub outcome: String,
    /// The request it was part of, when one was recorded.
    pub request_id: Option<String>,
    /// What else was kept, redacted before it was written.
    #[schema(value_type = Object)]
    pub metadata: serde_json::Value,
}

impl From<AuditEventRecord> for AuditEventView {
    fn from(record: AuditEventRecord) -> Self {
        Self {
            id: record.id.to_string(),
            occurred_at: rfc3339(record.occurred_at),
            actor_type: record.actor_type,
            actor_id: record.actor_id,
            action: record.action,
            target_type: record.target_type,
            target_id: record.target_id,
            outcome: record.outcome,
            request_id: record.request_id,
            metadata: record.metadata,
        }
    }
}

/// A page of audit events.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AuditEventList {
    /// The events in this page, newest first.
    pub items: Vec<AuditEventView>,
    /// Cursor for the next, older, page, or null when this is the last.
    pub next_cursor: Option<String>,
}

/// Which events to list. Each given must match exactly.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct AuditQuery {
    /// An action, such as `worker.revoked`; ending in `.`, every action of
    /// that kind, such as `worker.`.
    pub action: Option<String>,
    /// Who did it.
    pub actor_id: Option<String>,
    /// What kind of thing it was done to, such as `worker`.
    pub target_type: Option<String>,
    /// Which one.
    pub target_id: Option<String>,
    /// `success`, `failure` or `denied`.
    pub outcome: Option<String>,
}

/// A filter value, if given, as long as a column holds and free of control
/// characters.
fn bounded<'a>(name: &'static str, value: Option<&'a String>) -> Result<Option<&'a str>, Problem> {
    match value {
        None => Ok(None),
        Some(value)
            if value.is_empty()
                || value.chars().count() > MAX_FILTER_CHARS
                || value.chars().any(char::is_control) =>
        {
            Err(Problem::invalid_parameter(
                name,
                "must be 1 to 200 characters, none of them control characters",
            ))
        }
        Some(value) => Ok(Some(value.as_str())),
    }
}

/// A page of audit events, newest first.
///
/// # Errors
///
/// `INVALID_PARAMETER` for a filter that will not do, `INVALID_CURSOR` for a
/// cursor this server did not issue, `STORAGE_UNAVAILABLE` if the log
/// cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/audit-events",
    tag = "audit",
    description = "List audit events, newest first: who did what, to what, and how it came \
                   out. Filter by action (or a kind of action, ending in `.`), actor, \
                   target and outcome. Use `next_cursor` for older events. Needs an \
                   administrator.",
    params(PageQuery, AuditQuery),
    responses(
        (status = 200, description = "A page of events", body = AuditEventList),
        (status = 400, description = "A filter or cursor that will not do", body = Problem),
        (status = 403, description = "Not an administrator", body = Problem),
    ),
)]
pub async fn list_audit_log(
    State(state): State<ApiState>,
    Query(page): Query<PageQuery>,
    Query(query): Query<AuditQuery>,
) -> Result<Json<AuditEventList>, Problem> {
    let limit = page.limit();
    let before = page
        .after()?
        .map(|after| {
            uuid::Uuid::parse_str(&after)
                .map_err(|_| Problem::new(ErrorCode::InvalidCursor, "the cursor is not usable"))
        })
        .transpose()?;
    let filter = AuditFilter {
        action: bounded("action", query.action.as_ref())?,
        actor_id: bounded("actor_id", query.actor_id.as_ref())?,
        target_type: bounded("target_type", query.target_type.as_ref())?,
        target_id: bounded("target_id", query.target_id.as_ref())?,
        outcome: bounded("outcome", query.outcome.as_ref())?,
    };
    let events = list_audit_events(
        state.database().pool(),
        filter,
        i64::try_from(limit).unwrap_or(i64::MAX),
        before,
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not read the audit log");
        unavailable("the audit log could not be read")
    })?;
    let items: Vec<AuditEventView> = events.into_iter().map(AuditEventView::from).collect();
    let page = Page::new(items, limit, |event| event.id.clone());
    Ok(Json(AuditEventList {
        items: page.items,
        next_cursor: page.next_cursor,
    }))
}

/// One audit event.
///
/// # Errors
///
/// `NOT_FOUND` for an unknown event.
#[utoipa::path(
    get,
    path = "/api/v1/audit-events/{event_id}",
    tag = "audit",
    params(("event_id" = String, Path, description = "Audit event identifier")),
    responses(
        (status = 200, description = "The event", body = AuditEventView),
        (status = 403, description = "Not an administrator", body = Problem),
        (status = 404, description = "No such event", body = Problem),
    ),
)]
pub async fn get_audit_log_event(
    State(state): State<ApiState>,
    Path(event_id): Path<String>,
) -> Result<Json<AuditEventView>, Problem> {
    let id = uuid::Uuid::parse_str(&event_id)
        .map_err(|_| Problem::invalid_parameter("event_id", "not a valid identifier"))?;
    get_audit_event(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read an audit event");
            unavailable("the audit event could not be read")
        })?
        .map(|record| Json(record.into()))
        .ok_or_else(|| Problem::not_found("audit event", &event_id))
}

/// The audit routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/audit-events", get(list_audit_log))
        .route("/audit-events/{event_id}", get(get_audit_log_event))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_filter_is_bounded_and_plain() {
        let ok = "worker.".to_owned();
        assert_eq!(bounded("action", Some(&ok)).unwrap(), Some("worker."));
        assert_eq!(bounded("action", None).unwrap(), None);
        for bad in [String::new(), "x".repeat(201), "a\nb".to_owned()] {
            assert!(bounded("action", Some(&bad)).is_err(), "{bad:?}");
        }
    }
}
