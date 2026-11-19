// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! What copies of the database ask of the database itself (ADR 0026): its
//! server's version, which the dump tools must not be older than, when
//! copies are taken by themselves, and the audit trail of who took,
//! fetched, added and removed a copy.
//!
//! The copies are files in the backups volume, not rows: a row would be
//! rolled back with the rest by restoring the copy that holds it.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::DbError;

/// The time copies are taken each day unless an administrator chose
/// another: 03:00 UTC (owner, 2026-11-18).
pub const DEFAULT_MINUTE_OF_DAY: u16 = 3 * 60;

/// When the server takes copies by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    /// Whether it does.
    pub enabled: bool,
    /// At what time each day, in minutes past midnight UTC.
    pub minute_of_day: u16,
    /// Who last changed it; `None` while it is the default.
    pub updated_by: Option<String>,
    /// When.
    pub updated_at: Option<OffsetDateTime>,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            enabled: true,
            minute_of_day: DEFAULT_MINUTE_OF_DAY,
            updated_by: None,
            updated_at: None,
        }
    }
}

/// When copies are taken: the row, or the default without one.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn schedule(pool: &PgPool) -> Result<Schedule, DbError> {
    let row: Option<(bool, i16, String, OffsetDateTime)> = sqlx::query_as(
        "SELECT enabled, minute_of_day, updated_by, updated_at FROM backup_schedule",
    )
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(match row {
        Some((enabled, minute, updated_by, updated_at)) => Schedule {
            enabled,
            minute_of_day: u16::try_from(minute).unwrap_or(DEFAULT_MINUTE_OF_DAY),
            updated_by: Some(updated_by),
            updated_at: Some(updated_at),
        },
        None => Schedule::default(),
    })
}

/// Set when copies are taken, and audit it as `backup.schedule_changed`.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure, including a minute past the
/// day's last, which the table refuses.
pub async fn set_schedule(
    pool: &PgPool,
    enabled: bool,
    minute_of_day: u16,
    by: &str,
) -> Result<Schedule, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let minute = i16::try_from(minute_of_day).unwrap_or(i16::MAX);
    let (updated_at,): (OffsetDateTime,) = sqlx::query_as(
        "INSERT INTO backup_schedule (enabled, minute_of_day, updated_by)
         VALUES ($1, $2, $3)
         ON CONFLICT (singleton) DO UPDATE
            SET enabled = EXCLUDED.enabled,
                minute_of_day = EXCLUDED.minute_of_day,
                updated_by = EXCLUDED.updated_by
         RETURNING updated_at",
    )
    .bind(enabled)
    .bind(minute)
    .bind(by)
    .fetch_one(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    sqlx::query(
        "INSERT INTO audit_events
             (id, actor_type, actor_id, action, target_type, target_id, outcome, metadata)
         VALUES ($1, 'user', $2, 'backup.schedule_changed', 'backup', 'schedule', 'success', $3)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(by)
    .bind(serde_json::json!({ "enabled": enabled, "minute_of_day": minute_of_day }))
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(Schedule {
        enabled,
        minute_of_day,
        updated_by: Some(by.to_owned()),
        updated_at: Some(updated_at),
    })
}

/// Who did something to a copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor<'a> {
    /// An administrator, by username.
    User(&'a str),
    /// The schedule (docs/plans/backups-ui.md, D5).
    Schedule,
}

/// The major version of the PostgreSQL server: 17 for 17.12.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure, or a version that does not
/// read as one.
pub async fn server_major_version(pool: &PgPool) -> Result<u32, DbError> {
    let number: String = sqlx::query_scalar("SHOW server_version_num")
        .fetch_one(pool)
        .await
        .map_err(DbError::Query)?;
    number
        .trim()
        .parse::<u32>()
        .map(|number| number / 10_000)
        .map_err(|_| DbError::Enum {
            column: "server_version_num",
            value: number,
        })
}

/// Record something done to a copy: `backup.taken`, `backup.downloaded`,
/// `backup.uploaded` or `backup.deleted`, naming the copy.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn audit(
    pool: &PgPool,
    actor: Actor<'_>,
    action: &str,
    copy: &str,
    succeeded: bool,
    metadata: serde_json::Value,
) -> Result<(), DbError> {
    let (actor_type, actor_id) = match actor {
        Actor::User(username) => ("user", username),
        Actor::Schedule => ("system", "schedule"),
    };
    sqlx::query(
        "INSERT INTO audit_events
             (id, actor_type, actor_id, action, target_type, target_id, outcome, metadata)
         VALUES ($1, $2, $3, $4, 'backup', $5, $6, $7)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(actor_type)
    .bind(actor_id)
    .bind(action)
    .bind(copy)
    .bind(if succeeded { "success" } else { "failure" })
    .bind(metadata)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}
