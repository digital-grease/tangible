// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Drives, and requests to erase the disc in one.
//!
//! An erasure is tied to a drive, not to whichever worker asks first as a
//! burn job is: it destroys what is in that drive, so only that drive's
//! worker may take it. A drive does one thing at a time, so taking an
//! erasure and taking a burn each lock the drive's row and refuse while the
//! other is in progress.

use serde_json::Value;
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::str::FromStr;
use tangible_domain::{DriveId, ErasureId, ErasureMode, ErasureState, WorkerId};
use time::OffsetDateTime;

use crate::DbError;

/// A drive and the worker it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveRecord {
    /// The drive.
    pub id: DriveId,
    /// What the worker calls it.
    pub configured_name: String,
    /// Its device path inside the worker.
    pub device_alias: String,
    /// Reported vendor.
    pub vendor: Option<String>,
    /// Reported model.
    pub model: Option<String>,
    /// Its last reported status.
    pub status: String,
    /// When it was last reported.
    pub last_seen_at: Option<OffsetDateTime>,
    /// The worker.
    pub worker_id: WorkerId,
    /// The worker's name.
    pub worker_name: String,
    /// The worker's status.
    pub worker_status: String,
    /// When the worker last reported in.
    pub worker_last_seen_at: Option<OffsetDateTime>,
}

/// A request to erase a disc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErasureRecord {
    /// The request.
    pub id: ErasureId,
    /// The drive whose disc it erases.
    pub drive_id: DriveId,
    /// What the drive is called.
    pub drive_name: String,
    /// How thoroughly.
    pub mode: ErasureMode,
    /// Where it has got to.
    pub state: ErasureState,
    /// Who asked.
    pub requested_by: String,
    /// What the worker found in the drive.
    pub medium_before: Option<Value>,
    /// Why it did not erase, when it did not.
    pub error_code: Option<String>,
    /// More about that.
    pub error_detail: Option<String>,
    /// How long the erase took.
    pub duration_seconds: Option<i32>,
    /// When it was asked for.
    pub created_at: OffsetDateTime,
    /// When the worker took it.
    pub started_at: Option<OffsetDateTime>,
    /// When it ended.
    pub completed_at: Option<OffsetDateTime>,
}

/// Why an erasure could not be requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestOutcome {
    /// There is no such drive.
    DriveMissing,
    /// The drive already has one queued or erasing.
    AlreadyOpen,
}

/// Why an erasure could not be withdrawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// There is no such erasure.
    NotFound,
    /// It is past the point where it can be withdrawn.
    NotQueued(ErasureState),
}

/// Why a worker could not take or report an erasure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerOutcome {
    /// The drive, or the erasure's drive, is not this worker's, or does not
    /// exist. One answer for both, so a worker learns nothing about drives
    /// that are not its own.
    NotYours,
    /// The erasure is not in a state that accepts this report.
    Conflict(ErasureState),
}

/// What a worker reports at the end of an erasure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErasureResult<'a> {
    /// How it ended: erased, already blank, refused or failed.
    pub state: ErasureState,
    /// What was in the drive.
    pub medium_before: Option<&'a Value>,
    /// Why it did not erase.
    pub error_code: Option<&'a str>,
    /// More about that.
    pub error_detail: Option<&'a str>,
    /// How long it took.
    pub duration_seconds: Option<i32>,
}

const ERASURE_COLUMNS: &str = "e.id, e.drive_id, d.configured_name AS drive_name, e.mode, e.state, \
     e.requested_by, e.medium_before_json, e.error_code, e.error_detail, e.duration_seconds, \
     e.created_at, e.started_at, e.completed_at";

fn parse<T: FromStr>(column: &'static str, value: String) -> Result<T, DbError> {
    T::from_str(&value).map_err(|_| DbError::Enum { column, value })
}

fn erasure_from_row(row: &sqlx::postgres::PgRow) -> Result<ErasureRecord, DbError> {
    let get = |e| DbError::Query(e);
    Ok(ErasureRecord {
        id: ErasureId::from_uuid(row.try_get("id").map_err(get)?),
        drive_id: DriveId::from_uuid(row.try_get("drive_id").map_err(get)?),
        drive_name: row.try_get("drive_name").map_err(get)?,
        mode: parse("media_erasures.mode", row.try_get("mode").map_err(get)?)?,
        state: parse("media_erasures.state", row.try_get("state").map_err(get)?)?,
        requested_by: row.try_get("requested_by").map_err(get)?,
        medium_before: row.try_get("medium_before_json").map_err(get)?,
        error_code: row.try_get("error_code").map_err(get)?,
        error_detail: row.try_get("error_detail").map_err(get)?,
        duration_seconds: row.try_get("duration_seconds").map_err(get)?,
        created_at: row.try_get("created_at").map_err(get)?,
        started_at: row.try_get("started_at").map_err(get)?,
        completed_at: row.try_get("completed_at").map_err(get)?,
    })
}

async fn erasure_in(
    tx: &mut Transaction<'_, Postgres>,
    id: ErasureId,
) -> Result<Option<ErasureRecord>, DbError> {
    let row = sqlx::query(&format!(
        "SELECT {ERASURE_COLUMNS} FROM media_erasures e JOIN drives d ON d.id = e.drive_id
         WHERE e.id = $1"
    ))
    .bind(id.as_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    row.as_ref().map(erasure_from_row).transpose()
}

/// Every drive, with its worker, by worker name then drive name.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn list_drives(pool: &PgPool) -> Result<Vec<DriveRecord>, DbError> {
    let rows = sqlx::query(
        "SELECT d.id, d.configured_name, d.device_alias, d.vendor, d.model, d.status,
                d.last_seen_at, w.id AS worker_id, w.name AS worker_name,
                w.status AS worker_status, w.last_seen_at AS worker_last_seen_at
         FROM drives d JOIN workers w ON w.id = d.worker_id
         ORDER BY w.name, d.configured_name, d.id",
    )
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter()
        .map(|row| {
            let get = |e| DbError::Query(e);
            Ok(DriveRecord {
                id: DriveId::from_uuid(row.try_get("id").map_err(get)?),
                configured_name: row.try_get("configured_name").map_err(get)?,
                device_alias: row.try_get("device_alias").map_err(get)?,
                vendor: row.try_get("vendor").map_err(get)?,
                model: row.try_get("model").map_err(get)?,
                status: row.try_get("status").map_err(get)?,
                last_seen_at: row.try_get("last_seen_at").map_err(get)?,
                worker_id: WorkerId::from_uuid(row.try_get("worker_id").map_err(get)?),
                worker_name: row.try_get("worker_name").map_err(get)?,
                worker_status: row.try_get("worker_status").map_err(get)?,
                worker_last_seen_at: row.try_get("worker_last_seen_at").map_err(get)?,
            })
        })
        .collect()
}

/// Ask for the disc in a drive to be erased.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure. A missing drive or one that
/// already has an open erasure is a [`RequestOutcome`], not an error.
pub async fn request_erasure(
    pool: &PgPool,
    drive_id: DriveId,
    mode: ErasureMode,
    requested_by: &str,
) -> Result<Result<ErasureRecord, RequestOutcome>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM drives WHERE id = $1)")
        .bind(drive_id.as_uuid())
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    if !exists {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(RequestOutcome::DriveMissing));
    }

    let id = ErasureId::generate();
    let inserted = sqlx::query(
        "INSERT INTO media_erasures (id, drive_id, mode, requested_by)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (drive_id) WHERE state IN ('queued', 'erasing') DO NOTHING",
    )
    .bind(id.as_uuid())
    .bind(drive_id.as_uuid())
    .bind(mode.as_str())
    .bind(requested_by)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    if inserted.rows_affected() == 0 {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(RequestOutcome::AlreadyOpen));
    }

    audit(
        &mut tx,
        "user",
        requested_by,
        "media_erasure.requested",
        id,
        serde_json::json!({ "drive_id": drive_id.to_string(), "mode": mode.as_str() }),
    )
    .await?;
    let record = erasure_in(&mut tx, id).await?.ok_or(DbError::Enum {
        column: "media_erasures.id",
        value: id.to_string(),
    })?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(Ok(record))
}

/// The most recent erasures, newest first.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn list_erasures(pool: &PgPool, limit: i64) -> Result<Vec<ErasureRecord>, DbError> {
    let rows = sqlx::query(&format!(
        "SELECT {ERASURE_COLUMNS} FROM media_erasures e JOIN drives d ON d.id = e.drive_id
         ORDER BY e.created_at DESC, e.id DESC LIMIT $1"
    ))
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter().map(erasure_from_row).collect()
}

/// One erasure.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn get_erasure(pool: &PgPool, id: ErasureId) -> Result<Option<ErasureRecord>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let record = erasure_in(&mut tx, id).await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(record)
}

/// Withdraw an erasure the worker has not taken yet.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn cancel_erasure(
    pool: &PgPool,
    id: ErasureId,
    actor: &str,
) -> Result<Result<ErasureRecord, CancelOutcome>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM media_erasures WHERE id = $1 FOR UPDATE")
            .bind(id.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(DbError::Query)?;
    let Some(state) = state else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(CancelOutcome::NotFound));
    };
    let state: ErasureState = parse("media_erasures.state", state)?;
    if state.transition_to(ErasureState::Canceled).is_err() {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(CancelOutcome::NotQueued(state)));
    }

    sqlx::query("UPDATE media_erasures SET state = 'canceled', completed_at = now() WHERE id = $1")
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    audit(
        &mut tx,
        "user",
        actor,
        "media_erasure.canceled",
        id,
        serde_json::json!({}),
    )
    .await?;
    let record = erasure_in(&mut tx, id).await?.ok_or(DbError::Enum {
        column: "media_erasures.id",
        value: id.to_string(),
    })?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(Ok(record))
}

/// Lock a drive's row, if it belongs to `worker_id`.
///
/// Taken by both an erasure claim and a burn claim, so the two cannot start
/// on one drive at once.
pub(crate) async fn lock_own_drive(
    tx: &mut Transaction<'_, Postgres>,
    worker_id: WorkerId,
    drive_id: DriveId,
) -> Result<bool, DbError> {
    let found: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT id FROM drives WHERE id = $1 AND worker_id = $2 FOR UPDATE")
            .bind(drive_id.as_uuid())
            .bind(worker_id.as_uuid())
            .fetch_optional(&mut **tx)
            .await
            .map_err(DbError::Query)?;
    Ok(found.is_some())
}

/// Whether a drive is erasing.
pub(crate) async fn drive_is_erasing(
    tx: &mut Transaction<'_, Postgres>,
    drive_id: DriveId,
) -> Result<bool, DbError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM media_erasures WHERE drive_id = $1 AND state = 'erasing')",
    )
    .bind(drive_id.as_uuid())
    .fetch_one(&mut **tx)
    .await
    .map_err(DbError::Query)
}

/// Take the next erasure queued for a drive, for its worker.
///
/// A worker asks for work only when it is doing nothing, so an erasure still
/// marked as erasing on its drive is one it abandoned, by restarting
/// mid-erase. That is closed as failed first: the disc may be partly erased,
/// and an operator needs to know to erase it again. Nothing is taken while a
/// burn attempt holds the drive.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn claim_erasure(
    pool: &PgPool,
    worker_id: WorkerId,
    drive_id: DriveId,
) -> Result<Result<Option<ErasureRecord>, WorkerOutcome>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    if !lock_own_drive(&mut tx, worker_id, drive_id).await? {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(WorkerOutcome::NotYours));
    }

    let abandoned: Vec<uuid::Uuid> = sqlx::query_scalar(
        "UPDATE media_erasures
         SET state = 'failed', error_code = 'WORKER_RESTARTED',
             error_detail = 'the worker restarted while erasing; the disc may be partly erased \
                             and should be erased again',
             completed_at = now()
         WHERE drive_id = $1 AND state = 'erasing'
         RETURNING id",
    )
    .bind(drive_id.as_uuid())
    .fetch_all(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    for id in abandoned {
        audit(
            &mut tx,
            "worker",
            &worker_id.to_string(),
            "media_erasure.abandoned",
            ErasureId::from_uuid(id),
            serde_json::json!({}),
        )
        .await?;
    }

    if crate::repositories::drive_has_active_attempt(&mut tx, drive_id).await? {
        tx.commit().await.map_err(DbError::Query)?;
        return Ok(Ok(None));
    }

    let next: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT id FROM media_erasures WHERE drive_id = $1 AND state = 'queued'
         ORDER BY created_at LIMIT 1 FOR UPDATE",
    )
    .bind(drive_id.as_uuid())
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let Some(next) = next else {
        tx.commit().await.map_err(DbError::Query)?;
        return Ok(Ok(None));
    };
    let id = ErasureId::from_uuid(next);

    sqlx::query("UPDATE media_erasures SET state = 'erasing', started_at = now() WHERE id = $1")
        .bind(next)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    audit(
        &mut tx,
        "worker",
        &worker_id.to_string(),
        "media_erasure.started",
        id,
        serde_json::json!({}),
    )
    .await?;
    let record = erasure_in(&mut tx, id).await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(Ok(record))
}

/// Record how an erasure ended.
///
/// Idempotent: the same ending reported again, after a lost response, is
/// accepted and changes nothing.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn complete_erasure(
    pool: &PgPool,
    worker_id: WorkerId,
    id: ErasureId,
    result: &ErasureResult<'_>,
) -> Result<Result<ErasureRecord, WorkerOutcome>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let row = sqlx::query(
        "SELECT e.state FROM media_erasures e JOIN drives d ON d.id = e.drive_id
         WHERE e.id = $1 AND d.worker_id = $2
         FOR UPDATE OF e",
    )
    .bind(id.as_uuid())
    .bind(worker_id.as_uuid())
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let Some(row) = row else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(WorkerOutcome::NotYours));
    };
    let state: ErasureState = parse(
        "media_erasures.state",
        row.try_get("state").map_err(DbError::Query)?,
    )?;

    if state == result.state && state.is_terminal() {
        let record = erasure_in(&mut tx, id).await?;
        tx.commit().await.map_err(DbError::Query)?;
        return record.map(Ok).ok_or(DbError::Enum {
            column: "media_erasures.id",
            value: id.to_string(),
        });
    }
    if result.state.is_open() || state.transition_to(result.state).is_err() {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(WorkerOutcome::Conflict(state)));
    }

    sqlx::query(
        "UPDATE media_erasures
         SET state = $2, medium_before_json = $3, error_code = $4, error_detail = $5,
             duration_seconds = $6, completed_at = now()
         WHERE id = $1",
    )
    .bind(id.as_uuid())
    .bind(result.state.as_str())
    .bind(result.medium_before)
    .bind(result.error_code)
    .bind(result.error_detail)
    .bind(result.duration_seconds)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    audit(
        &mut tx,
        "worker",
        &worker_id.to_string(),
        "media_erasure.completed",
        id,
        serde_json::json!({ "state": result.state.as_str(), "error_code": result.error_code }),
    )
    .await?;
    let record = erasure_in(&mut tx, id).await?.ok_or(DbError::Enum {
        column: "media_erasures.id",
        value: id.to_string(),
    })?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(Ok(record))
}

async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    actor_type: &str,
    actor: &str,
    action: &str,
    id: ErasureId,
    metadata: Value,
) -> Result<(), DbError> {
    let outcome = if action == "media_erasure.abandoned" {
        "failure"
    } else {
        "success"
    };
    sqlx::query(
        "INSERT INTO audit_events
             (id, actor_type, actor_id, action, target_type, target_id, outcome, metadata)
         VALUES ($1, $2, $3, $4, 'media_erasure', $5, $6, $7)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(actor_type)
    .bind(actor)
    .bind(action)
    .bind(id.to_string())
    .bind(outcome)
    .bind(metadata)
    .execute(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}
