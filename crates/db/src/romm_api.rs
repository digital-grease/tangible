// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! What RomM's API is owed and was asked (docs/plans/romm-api.md M2): the
//! platform folders the export changed since RomM last scanned them, and
//! each scan Tangible asked for, with what became of it.

use std::str::FromStr;

use sqlx::{PgPool, Row};
use time::OffsetDateTime;

use crate::DbError;

/// Most platforms one scan is asked for.
pub const MAX_SCAN_PLATFORMS: usize = 256;

/// A platform folder the export changed since RomM last scanned it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwedScan {
    /// RomM's folder for the platform.
    pub platform: String,
    /// When the export last changed a folder under it.
    pub owed_at: OffsetDateTime,
}

/// Where a scan Tangible asked for stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanState {
    /// RomM did not queue it.
    Refused,
    /// Queued in RomM, not started.
    Queued,
    /// Running.
    Running,
    /// Done.
    Finished,
    /// It failed in RomM.
    Failed,
    /// Stopped or canceled in RomM.
    Canceled,
    /// RomM no longer has the task.
    Lost,
    /// It had not ended when Tangible stopped waiting.
    TimedOut,
}

impl ScanState {
    /// The stored word.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Refused => "refused",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::Lost => "lost",
            Self::TimedOut => "timed_out",
        }
    }

    /// Whether it is still going in RomM, as far as Tangible knows.
    #[must_use]
    pub const fn open(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

impl FromStr for ScanState {
    type Err = ();

    fn from_str(word: &str) -> Result<Self, Self::Err> {
        Ok(match word {
            "refused" => Self::Refused,
            "queued" => Self::Queued,
            "running" => Self::Running,
            "finished" => Self::Finished,
            "failed" => Self::Failed,
            "canceled" => Self::Canceled,
            "lost" => Self::Lost,
            "timed_out" => Self::TimedOut,
            _ => return Err(()),
        })
    }
}

/// What a scan counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanCounts {
    /// ROMs in the platforms scanned.
    pub total_roms: i64,
    /// ROMs it looked at.
    pub scanned_roms: i64,
    /// ROMs it made.
    pub new_roms: i64,
    /// ROMs it changed.
    pub updated_roms: i64,
}

/// A scan Tangible asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanRecord {
    /// Its id.
    pub id: uuid::Uuid,
    /// The RomM API integration that asked.
    pub integration_id: uuid::Uuid,
    /// The platform folders scanned.
    pub platforms: Vec<String>,
    /// RomM's id for the task, when it queued one.
    pub task_id: Option<String>,
    /// Where it stands.
    pub state: ScanState,
    /// Why it was refused, or what RomM last said of it.
    pub detail: Option<String>,
    /// What it counted, once it had.
    pub counts: Option<ScanCounts>,
    /// When it was asked for.
    pub requested_at: OffsetDateTime,
    /// When it ended, as far as Tangible knows.
    pub ended_at: Option<OffsetDateTime>,
}

/// A scan to record, as RomM answered the request for it.
#[derive(Debug, Clone, Copy)]
pub struct NewScan<'a> {
    /// The RomM API integration asking.
    pub integration_id: uuid::Uuid,
    /// The platform folders.
    pub platforms: &'a [String],
    /// RomM's id for the task; none when it was refused.
    pub task_id: Option<&'a str>,
    /// Where it stands.
    pub state: ScanState,
    /// Why it was refused.
    pub detail: Option<&'a str>,
    /// What it counted, if it had already.
    pub counts: Option<ScanCounts>,
}

const SCAN_COLUMNS: &str = "id, integration_id, platforms, task_id, state, detail, total_roms,
     scanned_roms, new_roms, updated_roms, requested_at, ended_at";

fn scan_from_row(row: &sqlx::postgres::PgRow) -> Result<ScanRecord, DbError> {
    let get = DbError::Query;
    let state: String = row.try_get("state").map_err(get)?;
    let total: Option<i64> = row.try_get("total_roms").map_err(get)?;
    Ok(ScanRecord {
        id: row.try_get("id").map_err(get)?,
        integration_id: row.try_get("integration_id").map_err(get)?,
        platforms: row.try_get("platforms").map_err(get)?,
        task_id: row.try_get("task_id").map_err(get)?,
        state: ScanState::from_str(&state).map_err(|()| DbError::Enum {
            column: "romm_scans.state",
            value: state.clone(),
        })?,
        detail: row.try_get("detail").map_err(get)?,
        counts: match total {
            None => None,
            Some(total_roms) => Some(ScanCounts {
                total_roms,
                scanned_roms: row
                    .try_get::<Option<i64>, _>("scanned_roms")
                    .map_err(get)?
                    .unwrap_or(0),
                new_roms: row
                    .try_get::<Option<i64>, _>("new_roms")
                    .map_err(get)?
                    .unwrap_or(0),
                updated_roms: row
                    .try_get::<Option<i64>, _>("updated_roms")
                    .map_err(get)?
                    .unwrap_or(0),
            }),
        },
        requested_at: row.try_get("requested_at").map_err(get)?,
        ended_at: row.try_get("ended_at").map_err(get)?,
    })
}

fn bounded(detail: Option<&str>) -> Option<String> {
    detail.map(|text| text.chars().take(2000).collect())
}

/// RomM's counts are its own; a negative one is stored as nothing.
fn count(value: Option<i64>) -> Option<i64> {
    value.filter(|count| *count >= 0)
}

/// Record that the export changed folders under `platforms`, so RomM owes
/// them a scan.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure, including a platform that is
/// not a folder slug.
pub async fn owe_scans(pool: &PgPool, platforms: &[&str]) -> Result<(), DbError> {
    if platforms.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO romm_owed_scans (platform)
         SELECT DISTINCT unnest($1::text[])
         ON CONFLICT (platform) DO UPDATE SET owed_at = now()",
    )
    .bind(platforms)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}

/// The platforms owed a scan, by name, at most [`MAX_SCAN_PLATFORMS`].
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn owed_scans(pool: &PgPool) -> Result<Vec<OwedScan>, DbError> {
    let rows =
        sqlx::query("SELECT platform, owed_at FROM romm_owed_scans ORDER BY platform LIMIT $1")
            .bind(i64::try_from(MAX_SCAN_PLATFORMS).unwrap_or(i64::MAX))
            .fetch_all(pool)
            .await
            .map_err(DbError::Query)?;
    rows.iter()
        .map(|row| {
            Ok(OwedScan {
                platform: row.try_get("platform").map_err(DbError::Query)?,
                owed_at: row.try_get("owed_at").map_err(DbError::Query)?,
            })
        })
        .collect()
}

/// Settle what a finished scan of `platforms` covered: each no longer
/// owed, unless the export changed it after `cutoff`, the time the scan
/// was known to cover.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn settle_owed_scans(
    pool: &PgPool,
    platforms: &[String],
    cutoff: OffsetDateTime,
) -> Result<u64, DbError> {
    let done =
        sqlx::query("DELETE FROM romm_owed_scans WHERE platform = ANY($1) AND owed_at <= $2")
            .bind(platforms)
            .bind(cutoff)
            .execute(pool)
            .await
            .map_err(DbError::Query)?;
    Ok(done.rows_affected())
}

/// Record a scan asked for, and audit the asking.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure, including a refused scan with
/// a task id or a queued one without.
pub async fn record_scan(pool: &PgPool, scan: NewScan<'_>) -> Result<uuid::Uuid, DbError> {
    let id = uuid::Uuid::now_v7();
    let counts = scan.counts;
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    sqlx::query(
        "INSERT INTO romm_scans
             (id, integration_id, platforms, task_id, state, detail, total_roms, scanned_roms,
              new_roms, updated_roms, ended_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                 CASE WHEN $5 IN ('queued', 'running') THEN NULL ELSE now() END)",
    )
    .bind(id)
    .bind(scan.integration_id)
    .bind(scan.platforms)
    .bind(scan.task_id)
    .bind(scan.state.as_str())
    .bind(bounded(scan.detail))
    .bind(count(counts.map(|c| c.total_roms)))
    .bind(count(counts.map(|c| c.scanned_roms)))
    .bind(count(counts.map(|c| c.new_roms)))
    .bind(count(counts.map(|c| c.updated_roms)))
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    sqlx::query(
        "INSERT INTO audit_events
             (id, actor_type, actor_id, action, target_type, target_id, outcome, metadata)
         VALUES ($1, 'system', 'romm-sync', 'romm.scan_requested', 'integration', $2, $3, $4)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(scan.integration_id.to_string())
    .bind(if scan.state == ScanState::Refused {
        "failure"
    } else {
        "success"
    })
    .bind(serde_json::json!({
        "scan_id": id,
        "platforms": scan.platforms,
        "task_id": scan.task_id,
        "state": scan.state.as_str(),
    }))
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(id)
}

/// Record where an open scan stands now. A scan that has ended is not
/// changed again.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn update_scan(
    pool: &PgPool,
    id: uuid::Uuid,
    state: ScanState,
    detail: Option<&str>,
    counts: Option<ScanCounts>,
) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE romm_scans SET
             state = $2,
             detail = COALESCE($3, detail),
             total_roms = COALESCE($4, total_roms),
             scanned_roms = COALESCE($5, scanned_roms),
             new_roms = COALESCE($6, new_roms),
             updated_roms = COALESCE($7, updated_roms),
             ended_at = CASE WHEN $2 IN ('queued', 'running') THEN NULL ELSE now() END
         WHERE id = $1 AND state IN ('queued', 'running')",
    )
    .bind(id)
    .bind(state.as_str())
    .bind(bounded(detail))
    .bind(count(counts.map(|c| c.total_roms)))
    .bind(count(counts.map(|c| c.scanned_roms)))
    .bind(count(counts.map(|c| c.new_roms)))
    .bind(count(counts.map(|c| c.updated_roms)))
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}

/// The scans an integration asked for that have not ended, oldest first.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure, or [`DbError::Enum`].
pub async fn open_scans(
    pool: &PgPool,
    integration_id: uuid::Uuid,
) -> Result<Vec<ScanRecord>, DbError> {
    let rows = sqlx::query(&format!(
        "SELECT {SCAN_COLUMNS} FROM romm_scans
         WHERE integration_id = $1 AND state IN ('queued', 'running')
         ORDER BY requested_at, id"
    ))
    .bind(integration_id)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter().map(scan_from_row).collect()
}

/// The last scan asked for, of `platform` when one is named.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure, or [`DbError::Enum`].
pub async fn last_scan(
    pool: &PgPool,
    platform: Option<&str>,
) -> Result<Option<ScanRecord>, DbError> {
    let row = sqlx::query(&format!(
        "SELECT {SCAN_COLUMNS} FROM romm_scans
         WHERE $1::text IS NULL OR $1 = ANY(platforms)
         ORDER BY requested_at DESC, id DESC LIMIT 1"
    ))
    .bind(platform)
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    row.as_ref().map(scan_from_row).transpose()
}

/// Remove an integration's ended scans beyond the `keep` most recent.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn prune_scans(
    pool: &PgPool,
    integration_id: uuid::Uuid,
    keep: i64,
) -> Result<u64, DbError> {
    let done = sqlx::query(
        "DELETE FROM romm_scans
         WHERE integration_id = $1 AND state NOT IN ('queued', 'running')
           AND id NOT IN (SELECT id FROM romm_scans WHERE integration_id = $1
                          ORDER BY requested_at DESC, id DESC LIMIT $2)",
    )
    .bind(integration_id)
    .bind(keep)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(done.rows_affected())
}
