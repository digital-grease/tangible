// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! What the dashboard shows (UX.md 3, docs/plans/dashboard.md): counts of
//! what needs doing or is under way, and the few latest burns.
//!
//! Each count is one aggregate over an indexed state column, as the metrics
//! snapshot's are; the lists are bounded. Nothing here returns a file name
//! or an error's detail text: a disc is named by its title and number, and
//! a failure by its code.

use sqlx::{PgPool, Row as _};
use time::OffsetDateTime;

use crate::DbError;

/// How many burns and jobs needing attention are listed.
const LISTED: i64 = 5;

/// The dashboard's numbers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DashboardRecord {
    /// Imports still under way.
    pub imports_active: i64,
    /// Imports that failed since the cut-off.
    pub imports_failed: i64,
    /// Burn jobs waiting for a worker.
    pub burns_queued: i64,
    /// Burn jobs waiting for a disc in the drive.
    pub burns_waiting_for_media: i64,
    /// Burn jobs a worker holds, staging to verifying.
    pub burns_active: i64,
    /// Burn jobs needing somebody to look.
    pub burns_needing_attention: i64,
    /// Workers online and heard from lately.
    pub workers_online: i64,
    /// Workers online whose heartbeats have stopped.
    pub workers_silent: i64,
    /// Workers draining.
    pub workers_draining: i64,
    /// Workers enrolled and never heard from.
    pub workers_pending: i64,
    /// Workers this server cannot talk to.
    pub workers_incompatible: i64,
    /// Artifacts an integrity check found damaged.
    pub artifacts_damaged: i64,
    /// Artifacts no integrity check has covered.
    pub artifacts_unchecked: i64,
    /// Stored objects.
    pub objects: i64,
    /// Their bytes.
    pub stored_bytes: i64,
    /// The latest ended burn attempts, newest first.
    pub recent_burns: Vec<BurnSummary>,
    /// The latest burn jobs needing attention, newest first.
    pub attention: Vec<BurnSummary>,
}

/// One burn, as the dashboard lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BurnSummary {
    /// The job.
    pub burn_job_id: uuid::Uuid,
    /// Its disc.
    pub disc_id: uuid::Uuid,
    /// The disc's title.
    pub title: String,
    /// Which disc of its set.
    pub disc_number: i16,
    /// The attempt's state (for a recent burn) or the job's (for one needing
    /// attention).
    pub state: String,
    /// The failure's code, when there was one.
    pub error_code: Option<String>,
    /// When it ended, or last changed.
    pub at: OffsetDateTime,
}

fn summary_from_row(row: &sqlx::postgres::PgRow) -> Result<BurnSummary, DbError> {
    Ok(BurnSummary {
        burn_job_id: row.try_get("burn_job_id").map_err(DbError::Query)?,
        disc_id: row.try_get("disc_id").map_err(DbError::Query)?,
        title: row.try_get("title").map_err(DbError::Query)?,
        disc_number: row.try_get("disc_number").map_err(DbError::Query)?,
        state: row.try_get("state").map_err(DbError::Query)?,
        error_code: row.try_get("error_code").map_err(DbError::Query)?,
        at: row.try_get("at").map_err(DbError::Query)?,
    })
}

/// Where a burn job's disc and title are, for a list.
const DISC_OF_JOB: &str = "JOIN discs d ON d.id = j.disc_id
     JOIN disc_sets s ON s.id = d.disc_set_id
     JOIN editions e ON e.id = s.edition_id
     JOIN titles t ON t.id = e.title_id";

/// The dashboard's numbers: workers silent when not heard from for
/// `silent_after_seconds`, imports failed since `failed_since`.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn summary(
    pool: &PgPool,
    silent_after_seconds: i64,
    failed_since: OffsetDateTime,
) -> Result<DashboardRecord, DbError> {
    let mut record = DashboardRecord::default();
    let count = |sql: &'static str| async move {
        sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(pool)
            .await
            .map_err(DbError::Query)
    };

    let imports = sqlx::query(
        "SELECT
             count(*) FILTER (WHERE state IN ('requested', 'acquiring', 'staged', 'hashing',
                                               'inspecting', 'registering')) AS active,
             count(*) FILTER (WHERE state IN ('failed_retryable', 'failed_terminal')
                                AND updated_at >= $1) AS failed
         FROM import_jobs",
    )
    .bind(failed_since)
    .fetch_one(pool)
    .await
    .map_err(DbError::Query)?;
    record.imports_active = imports.try_get("active").map_err(DbError::Query)?;
    record.imports_failed = imports.try_get("failed").map_err(DbError::Query)?;

    let burns = sqlx::query(
        "SELECT
             count(*) FILTER (WHERE state = 'queued') AS queued,
             count(*) FILTER (WHERE state = 'waiting_for_media') AS waiting,
             count(*) FILTER (WHERE state IN ('leased', 'staging', 'preflighting', 'ready',
                                               'writing', 'finalizing', 'verifying')) AS active,
             count(*) FILTER (WHERE state = 'needs_attention') AS attention
         FROM burn_jobs",
    )
    .fetch_one(pool)
    .await
    .map_err(DbError::Query)?;
    record.burns_queued = burns.try_get("queued").map_err(DbError::Query)?;
    record.burns_waiting_for_media = burns.try_get("waiting").map_err(DbError::Query)?;
    record.burns_active = burns.try_get("active").map_err(DbError::Query)?;
    record.burns_needing_attention = burns.try_get("attention").map_err(DbError::Query)?;

    let workers = sqlx::query(
        "SELECT
             count(*) FILTER (WHERE status = 'online'
                                AND last_seen_at >= now() - make_interval(secs => $1))
                 AS online,
             count(*) FILTER (WHERE status = 'online'
                                AND (last_seen_at IS NULL
                                     OR last_seen_at < now() - make_interval(secs => $1)))
                 AS silent,
             count(*) FILTER (WHERE status = 'draining') AS draining,
             count(*) FILTER (WHERE status = 'pending') AS pending,
             count(*) FILTER (WHERE status = 'incompatible') AS incompatible
         FROM workers",
    )
    .bind(f64::from(
        i32::try_from(silent_after_seconds).unwrap_or(i32::MAX),
    ))
    .fetch_one(pool)
    .await
    .map_err(DbError::Query)?;
    record.workers_online = workers.try_get("online").map_err(DbError::Query)?;
    record.workers_silent = workers.try_get("silent").map_err(DbError::Query)?;
    record.workers_draining = workers.try_get("draining").map_err(DbError::Query)?;
    record.workers_pending = workers.try_get("pending").map_err(DbError::Query)?;
    record.workers_incompatible = workers.try_get("incompatible").map_err(DbError::Query)?;

    record.artifacts_damaged = count(
        "SELECT count(*) FROM artifacts WHERE integrity_state = 'damaged' AND deleted_at IS NULL",
    )
    .await?;
    record.artifacts_unchecked = count(
        "SELECT count(*) FROM artifacts WHERE integrity_state = 'unchecked' AND deleted_at IS NULL",
    )
    .await?;

    let stored = sqlx::query(
        "SELECT count(*) AS objects, coalesce(sum(size_bytes), 0)::bigint AS bytes
         FROM cas_objects WHERE removed_at IS NULL",
    )
    .fetch_one(pool)
    .await
    .map_err(DbError::Query)?;
    record.objects = stored.try_get("objects").map_err(DbError::Query)?;
    record.stored_bytes = stored.try_get("bytes").map_err(DbError::Query)?;

    (record.recent_burns, record.attention) = listed_burns(pool).await?;
    Ok(record)
}

/// The latest ended burn attempts, and the latest jobs needing attention.
async fn listed_burns(pool: &PgPool) -> Result<(Vec<BurnSummary>, Vec<BurnSummary>), DbError> {
    let recent = sqlx::query(&format!(
        "SELECT j.id AS burn_job_id, d.id AS disc_id, t.display_title AS title,
                d.sequence_number AS disc_number, a.state, a.error_code, a.ended_at AS at
         FROM burn_attempts a
         JOIN burn_jobs j ON j.id = a.burn_job_id
         {DISC_OF_JOB}
         WHERE a.ended_at IS NOT NULL
         ORDER BY a.ended_at DESC, a.id DESC
         LIMIT $1"
    ))
    .bind(LISTED)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;

    let attention = sqlx::query(&format!(
        "SELECT j.id AS burn_job_id, d.id AS disc_id, t.display_title AS title,
                d.sequence_number AS disc_number, j.state,
                (SELECT a.error_code FROM burn_attempts a WHERE a.burn_job_id = j.id
                 ORDER BY a.attempt_number DESC LIMIT 1) AS error_code,
                j.updated_at AS at
         FROM burn_jobs j
         {DISC_OF_JOB}
         WHERE j.state = 'needs_attention'
         ORDER BY j.updated_at DESC, j.id DESC
         LIMIT $1"
    ))
    .bind(LISTED)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    Ok((
        recent
            .iter()
            .map(summary_from_row)
            .collect::<Result<_, _>>()?,
        attention
            .iter()
            .map(summary_from_row)
            .collect::<Result<_, _>>()?,
    ))
}
