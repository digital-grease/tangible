// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Reading the audit log (API.md 8.10, docs/plans/audit-log.md).
//!
//! Every module writes its own events, in the transaction of the change it
//! records. This only reads them: newest first, a page at a time, by the
//! event's id, which is a UUIDv7 wherever one is written, so id order is the
//! order they happened in.

use sqlx::{PgPool, Row as _};
use time::OffsetDateTime;

use crate::DbError;

/// One audit event.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditEventRecord {
    /// The event.
    pub id: uuid::Uuid,
    /// When it was written.
    pub occurred_at: OffsetDateTime,
    /// `user`, `worker`, `integration` or `system`.
    pub actor_type: String,
    /// Who: a username, a worker's id, an integration's, or a system task.
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
    pub metadata: serde_json::Value,
}

/// Which events to list: each set field must match exactly, but for
/// `action`, which ending in `.` matches every action of that kind.
#[derive(Debug, Clone, Copy, Default)]
pub struct AuditFilter<'a> {
    /// The action, or a kind of action such as `worker.`.
    pub action: Option<&'a str>,
    /// Who did it.
    pub actor_id: Option<&'a str>,
    /// What kind of thing it was done to.
    pub target_type: Option<&'a str>,
    /// Which one.
    pub target_id: Option<&'a str>,
    /// How it came out.
    pub outcome: Option<&'a str>,
}

const COLUMNS: &str = "id, created_at, actor_type, actor_id, action, target_type, target_id, \
                       outcome, request_id, metadata";

fn event_from_row(row: &sqlx::postgres::PgRow) -> Result<AuditEventRecord, DbError> {
    Ok(AuditEventRecord {
        id: row.try_get("id").map_err(DbError::Query)?,
        occurred_at: row.try_get("created_at").map_err(DbError::Query)?,
        actor_type: row.try_get("actor_type").map_err(DbError::Query)?,
        actor_id: row.try_get("actor_id").map_err(DbError::Query)?,
        action: row.try_get("action").map_err(DbError::Query)?,
        target_type: row.try_get("target_type").map_err(DbError::Query)?,
        target_id: row.try_get("target_id").map_err(DbError::Query)?,
        outcome: row.try_get("outcome").map_err(DbError::Query)?,
        request_id: row.try_get("request_id").map_err(DbError::Query)?,
        metadata: row.try_get("metadata").map_err(DbError::Query)?,
    })
}

/// A page of events, newest first, older than `before`.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn list_audit_events(
    pool: &PgPool,
    filter: AuditFilter<'_>,
    limit: i64,
    before: Option<uuid::Uuid>,
) -> Result<Vec<AuditEventRecord>, DbError> {
    // A kind of action is a prefix; LIKE's own wildcards in it are taken
    // literally, so `_` in `media_erasure.` means an underscore.
    let (action_exact, action_kind) = match filter.action {
        Some(action) if action.ends_with('.') => (None, Some(like_prefix(action))),
        other => (other, None),
    };
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM audit_events
         WHERE ($1::uuid IS NULL OR id < $1)
           AND ($2::text IS NULL OR action = $2)
           AND ($3::text IS NULL OR action LIKE $3 ESCAPE '\\')
           AND ($4::text IS NULL OR actor_id = $4)
           AND ($5::text IS NULL OR target_type = $5)
           AND ($6::text IS NULL OR target_id = $6)
           AND ($7::text IS NULL OR outcome = $7)
         ORDER BY id DESC
         LIMIT $8"
    )))
    .bind(before)
    .bind(action_exact)
    .bind(action_kind)
    .bind(filter.actor_id)
    .bind(filter.target_type)
    .bind(filter.target_id)
    .bind(filter.outcome)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter().map(event_from_row).collect()
}

/// One event.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn get_audit_event(
    pool: &PgPool,
    id: uuid::Uuid,
) -> Result<Option<AuditEventRecord>, DbError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM audit_events WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    row.as_ref().map(event_from_row).transpose()
}

/// A LIKE pattern matching anything that starts with `prefix`, its own
/// wildcards escaped.
fn like_prefix(prefix: &str) -> String {
    let mut pattern = String::with_capacity(prefix.len() + 1);
    for c in prefix.chars() {
        if matches!(c, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push('%');
    pattern
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kind_of_action_is_a_literal_prefix() {
        assert_eq!(like_prefix("worker."), "worker.%");
        assert_eq!(like_prefix("media_erasure."), "media\\_erasure.%");
        assert_eq!(like_prefix("a%b\\"), "a\\%b\\\\%");
    }
}
