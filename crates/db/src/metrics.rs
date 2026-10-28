// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! What the metrics listener reports (docs/plans/metrics.md), read from the
//! catalogue at scrape time: the database is what every process shares, so
//! nothing here is counted in memory and nothing is lost at a restart.
//!
//! Every figure is grouped by a closed set (a state, a format, a kind) or by
//! worker, never by artifact or job, so the number of series stays small.

use sqlx::PgPool;

use crate::DbError;

/// Everything one scrape reports.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MetricsSnapshot {
    /// Artifacts, by format.
    pub artifacts_by_format: Vec<(String, i64)>,
    /// Stored objects, and their bytes.
    pub stored_objects: i64,
    /// Bytes stored, every object once.
    pub stored_bytes: i64,
    /// Jobs of each kind, by state.
    pub jobs: Vec<(&'static str, String, i64)>,
    /// The age in seconds of the oldest job of each kind still waiting to
    /// start, for the kinds with one waiting.
    pub oldest_waiting: Vec<(&'static str, f64)>,
    /// Burn attempts, by state.
    pub burn_attempts: Vec<(String, i64)>,
    /// The seconds finished burn attempts took, in all, and how many.
    pub burn_attempt_seconds: (f64, i64),
    /// Workers, by status.
    pub workers: Vec<(String, i64)>,
    /// Seconds since each worker that has been seen was last seen, revoked
    /// ones aside.
    pub worker_last_seen: Vec<(String, f64)>,
    /// Drives, by status.
    pub drives: Vec<(String, i64)>,
    /// Artifacts, by what the last integrity check covering them found.
    pub integrity: Vec<(String, i64)>,
    /// When the last check of the whole library completed, in Unix seconds.
    pub last_full_check: Option<f64>,
    /// Artifacts, by the state of their preservation hashes.
    pub preservation_hashes: Vec<(String, i64)>,
    /// DATs loaded.
    pub dats: i64,
    /// DAT matches, by kind.
    pub dat_matches: Vec<(String, i64)>,
    /// Integrations, by kind and status.
    pub integrations: Vec<(String, String, i64)>,
}

/// The job tables, what each is called in a metric, and the state a job
/// waits in before anything has picked it up.
const JOB_TABLES: [(&str, &str, &str); 8] = [
    ("import", "import_jobs", "requested"),
    ("burn", "burn_jobs", "queued"),
    ("rip", "rip_jobs", "queued"),
    ("erasure", "media_erasures", "queued"),
    ("derivation", "derivation_jobs", "queued"),
    ("integrity_check", "integrity_checks", "queued"),
    ("source_removal", "source_removals", "verifying"),
    ("upload", "uploads", "open"),
];

async fn grouped(pool: &PgPool, sql: &str) -> Result<Vec<(String, i64)>, DbError> {
    sqlx::query_as::<_, (String, i64)>(sql)
        .fetch_all(pool)
        .await
        .map_err(DbError::Query)
}

/// Read one snapshot.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn snapshot(pool: &PgPool) -> Result<MetricsSnapshot, DbError> {
    let mut snapshot = MetricsSnapshot {
        artifacts_by_format: grouped(
            pool,
            "SELECT format, count(*) FROM artifacts GROUP BY format ORDER BY format",
        )
        .await?,
        ..MetricsSnapshot::default()
    };
    let (objects, bytes): (i64, i64) =
        sqlx::query_as("SELECT count(*), coalesce(sum(size_bytes), 0)::bigint FROM cas_objects")
            .fetch_one(pool)
            .await
            .map_err(DbError::Query)?;
    snapshot.stored_objects = objects;
    snapshot.stored_bytes = bytes;

    for (kind, table, waiting) in JOB_TABLES {
        // The names are this file's own constants, never input.
        for (state, count) in grouped(
            pool,
            &format!("SELECT state, count(*) FROM {table} GROUP BY state ORDER BY state"),
        )
        .await?
        {
            snapshot.jobs.push((kind, state, count));
        }
        let oldest: Option<f64> = sqlx::query_scalar(&format!(
            "SELECT extract(epoch FROM now() - min(created_at))::float8 \
             FROM {table} WHERE state = '{waiting}'"
        ))
        .fetch_one(pool)
        .await
        .map_err(DbError::Query)?;
        if let Some(seconds) = oldest {
            snapshot.oldest_waiting.push((kind, seconds.max(0.0)));
        }
    }

    snapshot.burn_attempts = grouped(
        pool,
        "SELECT state, count(*) FROM burn_attempts GROUP BY state ORDER BY state",
    )
    .await?;
    snapshot.burn_attempt_seconds = sqlx::query_as(
        "SELECT coalesce(sum(extract(epoch FROM ended_at - started_at)), 0)::float8, count(*)
         FROM burn_attempts WHERE ended_at IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .map_err(DbError::Query)?;

    snapshot.workers = grouped(
        pool,
        "SELECT status, count(*) FROM workers GROUP BY status ORDER BY status",
    )
    .await?;
    snapshot.worker_last_seen = sqlx::query_as(
        "SELECT name, greatest(extract(epoch FROM now() - last_seen_at), 0)::float8
         FROM workers WHERE status <> 'revoked' AND last_seen_at IS NOT NULL ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    snapshot.drives = grouped(
        pool,
        "SELECT status, count(*) FROM drives GROUP BY status ORDER BY status",
    )
    .await?;

    snapshot.integrity = grouped(
        pool,
        "SELECT integrity_state, count(*) FROM artifacts GROUP BY integrity_state \
         ORDER BY integrity_state",
    )
    .await?;
    snapshot.last_full_check = sqlx::query_scalar(
        "SELECT extract(epoch FROM max(completed_at))::float8 FROM integrity_checks
         WHERE scope = 'all' AND state = 'complete'",
    )
    .fetch_one(pool)
    .await
    .map_err(DbError::Query)?;

    snapshot.preservation_hashes = grouped(
        pool,
        "SELECT hashes_state, count(*) FROM artifacts GROUP BY hashes_state ORDER BY hashes_state",
    )
    .await?;
    snapshot.dats = sqlx::query_scalar("SELECT count(*) FROM dat_files")
        .fetch_one(pool)
        .await
        .map_err(DbError::Query)?;
    snapshot.dat_matches = grouped(
        pool,
        "SELECT kind, count(*) FROM artifact_dat_matches GROUP BY kind ORDER BY kind",
    )
    .await?;
    snapshot.integrations = sqlx::query_as(
        "SELECT kind, status, count(*) FROM integrations GROUP BY kind, status ORDER BY kind, status",
    )
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(snapshot)
}
