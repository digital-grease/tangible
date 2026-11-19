// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! What copies of the database ask of the database itself (ADR 0026): its
//! server's version, which the dump tools must not be older than, and the
//! audit trail of who took, fetched, added and removed a copy.
//!
//! The copies are files in the backups volume, not rows: a row would be
//! rolled back with the rest by restoring the copy that holds it.

use sqlx::PgPool;

use crate::DbError;

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
