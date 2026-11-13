// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! What `tangible restore-check` asks of the catalogue after a restore
//! (STORAGE.md 12): which artifacts it names, and what work it still
//! records as unfinished, which the restored database may hold from before
//! the backup was taken. The objects it names come from
//! [`crate::gc::catalogue_marks`].

use sqlx::PgPool;

use crate::DbError;

/// Each job table, what to call it, and its states that are not an end.
const UNFINISHED: [(&str, &str, &str); 7] = [
    (
        "import",
        "import_jobs",
        "'requested', 'acquiring', 'staged', 'hashing', 'inspecting', 'registering', \
         'failed_retryable'",
    ),
    (
        "burn",
        "burn_jobs",
        "'queued', 'leased', 'staging', 'waiting_for_media', 'preflighting', 'ready', \
         'writing', 'finalizing', 'verifying'",
    ),
    (
        "rip",
        "rip_jobs",
        "'queued', 'reading', 'uploading', 'registering'",
    ),
    ("erasure", "media_erasures", "'queued', 'erasing'"),
    (
        "derivation",
        "derivation_jobs",
        "'queued', 'running', 'failed_retryable'",
    ),
    ("integrity_check", "integrity_checks", "'queued', 'running'"),
    (
        "source_removal",
        "source_removals",
        "'verifying', 'removing'",
    ),
];

/// Every artifact the catalogue names, by id.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn catalogued_artifacts(pool: &PgPool) -> Result<Vec<uuid::Uuid>, DbError> {
    sqlx::query_scalar("SELECT id FROM artifacts ORDER BY id")
        .fetch_all(pool)
        .await
        .map_err(DbError::Query)
}

/// Unfinished jobs, by kind and state, for the kinds and states with any.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn unfinished_jobs(pool: &PgPool) -> Result<Vec<(&'static str, String, i64)>, DbError> {
    let mut found = Vec::new();
    for (kind, table, states) in UNFINISHED {
        // The names and states are this file's own constants, never input.
        let rows: Vec<(String, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT state, count(*) FROM {table} WHERE state IN ({states}) \
             GROUP BY state ORDER BY state"
        )))
        .fetch_all(pool)
        .await
        .map_err(DbError::Query)?;
        found.extend(rows.into_iter().map(|(state, count)| (kind, state, count)));
    }
    Ok(found)
}

/// Record that `artifact` was catalogued again from its manifest by a
/// restore check, which no person signed in to ask for.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn record_recatalogued(pool: &PgPool, artifact: uuid::Uuid) -> Result<(), DbError> {
    sqlx::query(
        "INSERT INTO audit_events (id, actor_type, actor_id, action, target_type, target_id, outcome)
         VALUES ($1, 'system', 'restore-check', 'artifact.recatalogued', 'artifact', $2, 'success')",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(artifact.to_string())
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}
