// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Rips: reading the disc in a drive into the library (ADR 0013).
//!
//! A rip is tied to its drive, as an erasure is. Only that drive's worker
//! takes it, and the claims for rips, erasures and burns all lock the
//! drive's row and refuse while one of the others holds it, so a drive does
//! one thing at a time.
//!
//! What the worker reads is uploaded into a staging area named by the rip's
//! `staging_id`. When the worker reports that it has sent everything, the
//! rip queues an import job with that same id at `staged`, and the import
//! runner does the rest exactly as for an upload. The rip follows its import
//! to the end.

use serde_json::Value;
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::str::FromStr;
use tangible_domain::rip::{DecryptionRefusal, check_decryption};
use tangible_domain::{DriveId, ImportJobId, RipJobId, RipOriginal, RipState, WorkerId};
use time::OffsetDateTime;

use crate::DbError;

/// A request to rip the disc in a drive.
#[derive(Debug, Clone, PartialEq)]
pub struct RipRecord {
    /// The rip.
    pub id: RipJobId,
    /// The drive it reads.
    pub drive_id: DriveId,
    /// What the drive is called.
    pub drive_name: String,
    /// Where it has got to.
    pub state: RipState,
    /// Who asked.
    pub requested_by: String,
    /// Whether they asked for an encrypted disc to be decrypted.
    pub decrypt: bool,
    /// The original they chose, when they asked to decrypt.
    pub keep: Option<RipOriginal>,
    /// The staging area uploads go to, and the import's id once queued.
    pub staging_id: ImportJobId,
    /// The import, once queued.
    pub import_job_id: Option<ImportJobId>,
    /// The artifact the import produced, once it has.
    pub artifact_id: Option<uuid::Uuid>,
    /// What the worker found in the drive.
    pub medium: Option<Value>,
    /// The tool that read it.
    pub engine: Option<String>,
    /// Its version.
    pub engine_version: Option<String>,
    /// Sectors that could not be read, or needed retries.
    pub read_errors: Option<Value>,
    /// What arrived, by file name, as the server hashed it.
    pub outputs: Value,
    /// Bytes received across every file.
    pub bytes_received: i64,
    /// Why it did not finish, when it did not.
    pub error_code: Option<String>,
    /// More about that.
    pub error_detail: Option<String>,
    /// When it was asked for.
    pub created_at: OffsetDateTime,
    /// When the worker took it.
    pub started_at: Option<OffsetDateTime>,
    /// When it ended.
    pub completed_at: Option<OffsetDateTime>,
}

/// Why a rip could not be requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestOutcome {
    /// There is no such drive.
    DriveMissing,
    /// The drive already has a rip that has not finished reading.
    AlreadyOpen,
}

/// Why a rip could not be withdrawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// There is no such rip.
    NotFound,
    /// Its import has started, or it has ended.
    TooLate(RipState),
}

/// Why a worker could not take or report on a rip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerOutcome {
    /// The drive, or the rip's drive, is not this worker's, or does not
    /// exist. One answer for both, as for erasures.
    NotYours,
    /// The rip is not in a state that accepts this.
    Conflict(RipState),
    /// The worker said it sent files the server does not have, or did not
    /// send ones the rip needs.
    MissingOutputs,
}

/// One file the worker sent, as the server received it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedFile<'a> {
    /// Its name in the staging area.
    pub name: &'a str,
    /// Its SHA-256, as the server computed it.
    pub sha256: &'a str,
    /// Its length.
    pub bytes: i64,
}

/// How a rip ended, as its worker reports it.
#[derive(Debug, Clone, PartialEq)]
pub enum RipResult<'a> {
    /// The disc was read and every file sent.
    Read {
        /// The tool that read it.
        engine: &'a str,
        /// Its version.
        engine_version: &'a str,
        /// What was in the drive.
        medium: Option<&'a Value>,
        /// Sectors that could not be read, or needed retries.
        read_errors: Option<&'a Value>,
        /// The files that make up the image, which must all have arrived.
        outputs: &'a [String],
    },
    /// Nothing was read: no disc, or one this worker will not rip.
    Refused {
        /// Why, as a stable code.
        code: &'a str,
        /// More about that.
        detail: Option<&'a str>,
        /// What was in the drive, if anything.
        medium: Option<&'a Value>,
    },
    /// Reading or sending did not succeed.
    Failed {
        /// Why, as a stable code.
        code: &'a str,
        /// More about that.
        detail: Option<&'a str>,
    },
}

/// An event a worker reports during a rip.
#[derive(Debug, Clone, PartialEq)]
pub struct RipEvent<'a> {
    /// Its place in the worker's sequence.
    pub sequence: i64,
    /// What kind of event.
    pub event_type: &'a str,
    /// Its stable code.
    pub message_code: &'a str,
    /// How far along, 0 to 1.
    pub progress_fraction: Option<f32>,
    /// Anything else, bounded.
    pub data: &'a Value,
    /// When the worker saw it.
    pub worker_timestamp: OffsetDateTime,
}

/// An event as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct RipEventRecord {
    /// Its place in the sequence.
    pub sequence: i64,
    /// What kind of event.
    pub event_type: String,
    /// Its stable code.
    pub message_code: String,
    /// How far along.
    pub progress_fraction: Option<f32>,
    /// Anything else.
    pub data: Value,
    /// When the worker saw it.
    pub worker_timestamp: OffsetDateTime,
}

const RIP_COLUMNS: &str = "r.id, r.drive_id, d.configured_name AS drive_name, r.state, \
     r.requested_by, r.decrypt, r.keep, r.staging_id, r.import_job_id, i.artifact_id, r.medium_json, r.engine, \
     r.engine_version, r.read_errors_json, r.outputs_json, r.bytes_received, r.error_code, \
     r.error_detail, r.created_at, r.started_at, r.completed_at";

const RIP_FROM: &str = "rip_jobs r JOIN drives d ON d.id = r.drive_id \
     LEFT JOIN import_jobs i ON i.id = r.import_job_id";

fn parse<T: FromStr>(column: &'static str, value: String) -> Result<T, DbError> {
    T::from_str(&value).map_err(|_| DbError::Enum { column, value })
}

fn rip_from_row(row: &sqlx::postgres::PgRow) -> Result<RipRecord, DbError> {
    let get = DbError::Query;
    Ok(RipRecord {
        id: RipJobId::from_uuid(row.try_get("id").map_err(get)?),
        drive_id: DriveId::from_uuid(row.try_get("drive_id").map_err(get)?),
        drive_name: row.try_get("drive_name").map_err(get)?,
        state: parse("rip_jobs.state", row.try_get("state").map_err(get)?)?,
        requested_by: row.try_get("requested_by").map_err(get)?,
        decrypt: row.try_get("decrypt").map_err(get)?,
        keep: row
            .try_get::<Option<String>, _>("keep")
            .map_err(get)?
            .map(|keep| parse("rip_jobs.keep", keep))
            .transpose()?,
        staging_id: ImportJobId::from_uuid(row.try_get("staging_id").map_err(get)?),
        import_job_id: row
            .try_get::<Option<uuid::Uuid>, _>("import_job_id")
            .map_err(get)?
            .map(ImportJobId::from_uuid),
        artifact_id: row.try_get("artifact_id").map_err(get)?,
        medium: row.try_get("medium_json").map_err(get)?,
        engine: row.try_get("engine").map_err(get)?,
        engine_version: row.try_get("engine_version").map_err(get)?,
        read_errors: row.try_get("read_errors_json").map_err(get)?,
        outputs: row.try_get("outputs_json").map_err(get)?,
        bytes_received: row.try_get("bytes_received").map_err(get)?,
        error_code: row.try_get("error_code").map_err(get)?,
        error_detail: row.try_get("error_detail").map_err(get)?,
        created_at: row.try_get("created_at").map_err(get)?,
        started_at: row.try_get("started_at").map_err(get)?,
        completed_at: row.try_get("completed_at").map_err(get)?,
    })
}

async fn rip_in(
    tx: &mut Transaction<'_, Postgres>,
    id: RipJobId,
) -> Result<Option<RipRecord>, DbError> {
    let row = sqlx::query(&format!(
        "SELECT {RIP_COLUMNS} FROM {RIP_FROM} WHERE r.id = $1"
    ))
    .bind(id.as_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    row.as_ref().map(rip_from_row).transpose()
}

async fn rip_required(
    tx: &mut Transaction<'_, Postgres>,
    id: RipJobId,
) -> Result<RipRecord, DbError> {
    rip_in(tx, id).await?.ok_or(DbError::Enum {
        column: "rip_jobs.id",
        value: id.to_string(),
    })
}

/// Lock a rip's row for its own drive's worker and read its state.
async fn lock_for_worker(
    tx: &mut Transaction<'_, Postgres>,
    worker_id: WorkerId,
    id: RipJobId,
) -> Result<Option<RipState>, DbError> {
    let state: Option<String> = sqlx::query_scalar(
        "SELECT r.state FROM rip_jobs r JOIN drives d ON d.id = r.drive_id
         WHERE r.id = $1 AND d.worker_id = $2
         FOR UPDATE OF r",
    )
    .bind(id.as_uuid())
    .bind(worker_id.as_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    state
        .map(|state| parse("rip_jobs.state", state))
        .transpose()
}

/// Whether a drive is in the middle of a rip: reading, or sending what it
/// read.
pub(crate) async fn drive_is_ripping(
    tx: &mut Transaction<'_, Postgres>,
    drive_id: DriveId,
) -> Result<bool, DbError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM rip_jobs
                        WHERE drive_id = $1 AND state IN ('reading', 'uploading'))",
    )
    .bind(drive_id.as_uuid())
    .fetch_one(&mut **tx)
    .await
    .map_err(DbError::Query)
}

/// The rip engines a drive's worker reported, if it reported any.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn drive_rip_engines(pool: &PgPool, drive_id: DriveId) -> Result<Option<Value>, DbError> {
    let engines: Option<Option<Value>> = sqlx::query_scalar(
        "SELECT w.capabilities_json -> 'rip' -> 'engines'
         FROM drives d JOIN workers w ON w.id = d.worker_id WHERE d.id = $1",
    )
    .bind(drive_id.as_uuid())
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(engines
        .flatten()
        .filter(|engines| engines.as_array().is_some_and(|list| !list.is_empty())))
}

/// Whether a drive's worker reported that its image can decrypt a DVD.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn drive_can_decrypt(pool: &PgPool, drive_id: DriveId) -> Result<bool, DbError> {
    let can: Option<bool> = sqlx::query_scalar(&format!(
        "SELECT {CAN_DECRYPT} FROM drives d JOIN workers w ON w.id = d.worker_id WHERE d.id = $1"
    ))
    .bind(drive_id.as_uuid())
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(can.unwrap_or(false))
}

/// Whether the worker `w` reported DVD decryption, as SQL.
const CAN_DECRYPT: &str = "COALESCE(jsonb_typeof(w.capabilities_json -> 'rip' -> 'decryption' -> 'css') = 'object', false)";

/// What an operator chose about decryption for one rip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RipOptions {
    /// Decrypt an encrypted disc.
    pub decrypt: bool,
    /// The original to keep, when decrypting. Ignored otherwise.
    pub keep: RipOriginal,
}

/// Ask for the disc in a drive to be ripped.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure. A missing drive, or one with a
/// rip still reading, is a [`RequestOutcome`].
pub async fn request_rip(
    pool: &PgPool,
    drive_id: DriveId,
    requested_by: &str,
    options: RipOptions,
) -> Result<Result<RipRecord, RequestOutcome>, DbError> {
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

    let id = RipJobId::generate();
    let inserted = sqlx::query(
        "INSERT INTO rip_jobs (id, drive_id, requested_by, staging_id, decrypt, keep)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (drive_id) WHERE state IN ('queued', 'reading', 'uploading') DO NOTHING",
    )
    .bind(id.as_uuid())
    .bind(drive_id.as_uuid())
    .bind(requested_by)
    .bind(ImportJobId::generate().as_uuid())
    .bind(options.decrypt)
    .bind(options.decrypt.then(|| options.keep.as_str()))
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
        "rip.requested",
        id,
        "success",
        serde_json::json!({
            "drive_id": drive_id.to_string(),
            "decrypt": options.decrypt,
            "keep": options.decrypt.then(|| options.keep.as_str()),
        }),
    )
    .await?;
    let record = rip_required(&mut tx, id).await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(Ok(record))
}

/// Rips, newest first, after the one named by `before` if given.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn list_rips(
    pool: &PgPool,
    limit: i64,
    before: Option<RipJobId>,
) -> Result<Vec<RipRecord>, DbError> {
    let rows = sqlx::query(&format!(
        "SELECT {RIP_COLUMNS} FROM {RIP_FROM}
         WHERE $2::uuid IS NULL
            OR (r.created_at, r.id) < (SELECT created_at, id FROM rip_jobs WHERE id = $2)
         ORDER BY r.created_at DESC, r.id DESC LIMIT $1"
    ))
    .bind(limit)
    .bind(before.map(|id| *id.as_uuid()))
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter().map(rip_from_row).collect()
}

/// One rip.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn get_rip(pool: &PgPool, id: RipJobId) -> Result<Option<RipRecord>, DbError> {
    let row = sqlx::query(&format!(
        "SELECT {RIP_COLUMNS} FROM {RIP_FROM} WHERE r.id = $1"
    ))
    .bind(id.as_uuid())
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    row.as_ref().map(rip_from_row).transpose()
}

/// Withdraw a rip before its import starts. Reading changes nothing on the
/// disc, so this is allowed while the worker reads or sends; the worker
/// learns of it the next time it reports and stops.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn cancel_rip(
    pool: &PgPool,
    id: RipJobId,
    actor: &str,
) -> Result<Result<RipRecord, CancelOutcome>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM rip_jobs WHERE id = $1 FOR UPDATE")
            .bind(id.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(DbError::Query)?;
    let Some(state) = state else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(CancelOutcome::NotFound));
    };
    let state: RipState = parse("rip_jobs.state", state)?;
    if !state.can_cancel() || state.transition_to(RipState::Canceled).is_err() {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(CancelOutcome::TooLate(state)));
    }

    sqlx::query("UPDATE rip_jobs SET state = 'canceled', completed_at = now() WHERE id = $1")
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    audit(
        &mut tx,
        "user",
        actor,
        "rip.canceled",
        id,
        "success",
        serde_json::json!({ "from": state.as_str() }),
    )
    .await?;
    let record = rip_required(&mut tx, id).await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(Ok(record))
}

/// Take the next rip queued for a drive, for its worker.
///
/// A worker asks for work only when idle, so a rip still reading or
/// uploading on its drive is one it abandoned by restarting. That is closed
/// as failed first; ripping again is harmless, so the person just asks
/// again. Nothing is taken while a burn attempt or an erasure holds the
/// drive.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn claim_rip(
    pool: &PgPool,
    worker_id: WorkerId,
    drive_id: DriveId,
    decryption_allowed: bool,
) -> Result<Result<Option<RipRecord>, WorkerOutcome>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    if !crate::erasures::lock_own_drive(&mut tx, worker_id, drive_id).await? {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(WorkerOutcome::NotYours));
    }

    let abandoned: Vec<uuid::Uuid> = sqlx::query_scalar(
        "UPDATE rip_jobs
         SET state = 'failed', error_code = 'WORKER_RESTARTED',
             error_detail = 'the worker restarted while ripping; ask for the rip again',
             completed_at = now()
         WHERE drive_id = $1 AND state IN ('reading', 'uploading')
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
            "rip.abandoned",
            RipJobId::from_uuid(id),
            "failure",
            serde_json::json!({}),
        )
        .await?;
    }

    if crate::repositories::drive_has_active_attempt(&mut tx, drive_id).await?
        || crate::erasures::drive_is_erasing(&mut tx, drive_id).await?
    {
        tx.commit().await.map_err(DbError::Query)?;
        return Ok(Ok(None));
    }

    let next: Option<(uuid::Uuid, bool, bool)> = sqlx::query_as(&format!(
        "SELECT r.id, r.decrypt, {CAN_DECRYPT}
         FROM rip_jobs r JOIN drives d ON d.id = r.drive_id JOIN workers w ON w.id = d.worker_id
         WHERE r.drive_id = $1 AND r.state = 'queued'
         ORDER BY r.created_at LIMIT 1 FOR UPDATE OF r"
    ))
    .bind(drive_id.as_uuid())
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let Some((next, decrypt, worker_can)) = next else {
        tx.commit().await.map_err(DbError::Query)?;
        return Ok(Ok(None));
    };
    let id = RipJobId::from_uuid(next);

    // Checked again as the worker takes it: decryption may have been turned
    // off, or the worker rebuilt without it, since the rip was asked for. A
    // decrypting rip is never handed to a worker that cannot, or by a server
    // that does not allow it; it is refused here, before anything is read.
    if let Err(refusal) = check_decryption(decrypt, decryption_allowed, worker_can) {
        refuse_for_decryption(&mut tx, worker_id, id, refusal).await?;
        tx.commit().await.map_err(DbError::Query)?;
        return Ok(Ok(None));
    }

    sqlx::query("UPDATE rip_jobs SET state = 'reading', started_at = now() WHERE id = $1")
        .bind(next)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    audit(
        &mut tx,
        "worker",
        &worker_id.to_string(),
        "rip.started",
        id,
        "success",
        serde_json::json!({}),
    )
    .await?;
    let record = rip_in(&mut tx, id).await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(Ok(record))
}

/// Close a queued rip that may not decrypt as `refused`.
async fn refuse_for_decryption(
    tx: &mut Transaction<'_, Postgres>,
    worker_id: WorkerId,
    id: RipJobId,
    refusal: DecryptionRefusal,
) -> Result<(), DbError> {
    let detail = match refusal {
        DecryptionRefusal::Off => {
            "decryption was turned off on this server after the rip was asked for; ask again \
             without decryption, or have an administrator turn it on"
        }
        DecryptionRefusal::WorkerCannot => {
            "this drive's worker no longer reports that it can decrypt; run it from an image \
             built to decrypt, or ask again without decryption"
        }
    };
    sqlx::query(
        "UPDATE rip_jobs SET state = 'refused', error_code = $2, error_detail = $3,
             completed_at = now()
         WHERE id = $1",
    )
    .bind(id.as_uuid())
    .bind(refusal.code())
    .bind(detail)
    .execute(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    audit(
        tx,
        "worker",
        &worker_id.to_string(),
        "rip.refused",
        id,
        "failure",
        serde_json::json!({ "code": refusal.code() }),
    )
    .await
}

/// A rip, if it is on one of this worker's drives.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn get_rip_for_worker(
    pool: &PgPool,
    worker_id: WorkerId,
    id: RipJobId,
) -> Result<Option<RipRecord>, DbError> {
    let row = sqlx::query(&format!(
        "SELECT {RIP_COLUMNS} FROM {RIP_FROM} WHERE r.id = $1 AND d.worker_id = $2"
    ))
    .bind(id.as_uuid())
    .bind(worker_id.as_uuid())
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    row.as_ref().map(rip_from_row).transpose()
}

/// Record events a worker reports. Idempotent by sequence: one already
/// stored is skipped. Returns the rip's state, which is how a worker learns
/// that a person withdrew the rip.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn record_rip_events(
    pool: &PgPool,
    worker_id: WorkerId,
    id: RipJobId,
    events: &[RipEvent<'_>],
) -> Result<Result<RipState, WorkerOutcome>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let Some(state) = lock_for_worker(&mut tx, worker_id, id).await? else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(WorkerOutcome::NotYours));
    };
    if !state.holds_the_drive() {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(WorkerOutcome::Conflict(state)));
    }
    for event in events {
        sqlx::query(
            "INSERT INTO rip_events
                 (rip_id, sequence, event_type, message_code, progress_fraction, data_json,
                  worker_timestamp)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (rip_id, sequence) DO NOTHING",
        )
        .bind(id.as_uuid())
        .bind(event.sequence)
        .bind(event.event_type)
        .bind(event.message_code)
        .bind(event.progress_fraction)
        .bind(event.data)
        .bind(event.worker_timestamp)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    }
    tx.commit().await.map_err(DbError::Query)?;
    Ok(Ok(state))
}

/// A rip's events, in sequence, after `after` if given.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn list_rip_events(
    pool: &PgPool,
    id: RipJobId,
    after: Option<i64>,
    limit: i64,
) -> Result<Vec<RipEventRecord>, DbError> {
    let rows = sqlx::query(
        "SELECT sequence, event_type, message_code, progress_fraction, data_json,
                worker_timestamp
         FROM rip_events WHERE rip_id = $1 AND sequence > $2
         ORDER BY sequence LIMIT $3",
    )
    .bind(id.as_uuid())
    .bind(after.unwrap_or(-1))
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter()
        .map(|row| {
            let get = DbError::Query;
            Ok(RipEventRecord {
                sequence: row.try_get("sequence").map_err(get)?,
                event_type: row.try_get("event_type").map_err(get)?,
                message_code: row.try_get("message_code").map_err(get)?,
                progress_fraction: row.try_get("progress_fraction").map_err(get)?,
                data: row.try_get("data_json").map_err(get)?,
                worker_timestamp: row.try_get("worker_timestamp").map_err(get)?,
            })
        })
        .collect()
}

/// Whether this worker may send files for a rip now. The caller streams the
/// file into the staging area only after this says yes, then records it
/// with [`record_upload`].
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn check_upload(
    pool: &PgPool,
    worker_id: WorkerId,
    id: RipJobId,
) -> Result<Result<RipRecord, WorkerOutcome>, DbError> {
    let Some(record) = get_rip_for_worker(pool, worker_id, id).await? else {
        return Ok(Err(WorkerOutcome::NotYours));
    };
    if !record.state.accepts_uploads() {
        return Ok(Err(WorkerOutcome::Conflict(record.state)));
    }
    Ok(Ok(record))
}

/// Record a file the server has received and hashed. The first one moves
/// the rip from reading to uploading; sending a file again replaces what
/// was recorded for it.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn record_upload(
    pool: &PgPool,
    worker_id: WorkerId,
    id: RipJobId,
    file: &ReceivedFile<'_>,
) -> Result<Result<RipRecord, WorkerOutcome>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let Some(state) = lock_for_worker(&mut tx, worker_id, id).await? else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(WorkerOutcome::NotYours));
    };
    if !state.accepts_uploads() {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(WorkerOutcome::Conflict(state)));
    }
    let next = if state == RipState::Reading {
        RipState::Uploading
    } else {
        state
    };
    sqlx::query(
        "UPDATE rip_jobs
         SET state = $2,
             outputs_json = outputs_json || jsonb_build_object($3::text,
                 jsonb_build_object('sha256', $4::text, 'bytes', $5::bigint))
         WHERE id = $1",
    )
    .bind(id.as_uuid())
    .bind(next.as_str())
    .bind(file.name)
    .bind(file.sha256)
    .bind(file.bytes)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    sqlx::query(
        "UPDATE rip_jobs SET bytes_received = COALESCE((
             SELECT sum((value ->> 'bytes')::bigint) FROM jsonb_each(outputs_json)), 0)
         WHERE id = $1",
    )
    .bind(id.as_uuid())
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let record = rip_required(&mut tx, id).await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(Ok(record))
}

/// Record how a rip ended, as its worker reports it.
///
/// A rip that was read queues its import, at `staged`, with the staging
/// area's id, and moves to registering; the import runner takes it from
/// there. Every file the worker names must have arrived, and nothing else
/// may have. The same ending reported again, after a lost response, is
/// accepted and changes nothing.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn complete_rip(
    pool: &PgPool,
    worker_id: WorkerId,
    id: RipJobId,
    result: &RipResult<'_>,
) -> Result<Result<RipRecord, WorkerOutcome>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let Some(state) = lock_for_worker(&mut tx, worker_id, id).await? else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(WorkerOutcome::NotYours));
    };
    let target = match result {
        RipResult::Read { .. } => RipState::Registering,
        RipResult::Refused { .. } => RipState::Refused,
        RipResult::Failed { .. } => RipState::Failed,
    };
    // A repeat: the same ending, already recorded.
    let repeated = match result {
        RipResult::Read { .. } => matches!(state, RipState::Registering | RipState::Complete),
        _ => state == target,
    };
    if repeated {
        let record = rip_required(&mut tx, id).await?;
        tx.commit().await.map_err(DbError::Query)?;
        return Ok(Ok(record));
    }
    if state.transition_to(target).is_err() {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(WorkerOutcome::Conflict(state)));
    }

    let closed = match result {
        RipResult::Read {
            engine,
            engine_version,
            medium,
            read_errors,
            outputs,
        } => {
            queue_import(
                &mut tx,
                worker_id,
                id,
                &ReadReport {
                    engine,
                    engine_version,
                    medium: *medium,
                    read_errors: *read_errors,
                    outputs,
                },
            )
            .await?
        }
        RipResult::Refused {
            code,
            detail,
            medium,
        } => {
            close(
                &mut tx,
                worker_id,
                id,
                RipState::Refused,
                code,
                *detail,
                *medium,
            )
            .await?;
            true
        }
        RipResult::Failed { code, detail } => {
            close(
                &mut tx,
                worker_id,
                id,
                RipState::Failed,
                code,
                *detail,
                None,
            )
            .await?;
            true
        }
    };
    if !closed {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(WorkerOutcome::MissingOutputs));
    }
    let record = rip_required(&mut tx, id).await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(Ok(record))
}

/// What a worker reported about a rip that was read.
struct ReadReport<'a> {
    engine: &'a str,
    engine_version: &'a str,
    medium: Option<&'a Value>,
    read_errors: Option<&'a Value>,
    outputs: &'a [String],
}

/// Queue the import of what a rip read, and move it to registering. Returns
/// false, changing nothing, when the files the worker names are not exactly
/// the files that arrived.
async fn queue_import(
    tx: &mut Transaction<'_, Postgres>,
    worker_id: WorkerId,
    id: RipJobId,
    report: &ReadReport<'_>,
) -> Result<bool, DbError> {
    let row = sqlx::query(
        "SELECT staging_id, requested_by, outputs_json, bytes_received
         FROM rip_jobs WHERE id = $1",
    )
    .bind(id.as_uuid())
    .fetch_one(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    let get = DbError::Query;
    let staging_id: uuid::Uuid = row.try_get("staging_id").map_err(get)?;
    let requested_by: String = row.try_get("requested_by").map_err(get)?;
    let received: Value = row.try_get("outputs_json").map_err(get)?;
    let bytes: i64 = row.try_get("bytes_received").map_err(get)?;

    let mut named: Vec<&str> = report.outputs.iter().map(String::as_str).collect();
    named.sort_unstable();
    named.dedup();
    let mut arrived: Vec<&str> = received
        .as_object()
        .map(|files| files.keys().map(String::as_str).collect())
        .unwrap_or_default();
    arrived.sort_unstable();
    if named.is_empty() || named != arrived {
        return Ok(false);
    }

    let descriptor = serde_json::json!({
        "kind": "physical_drive",
        "rip_id": id.to_string(),
        "reference": format!("rip {id}"),
    });
    sqlx::query(
        "INSERT INTO import_jobs
             (id, source_kind, source_descriptor_json, state, bytes_expected,
              bytes_received, created_by)
         VALUES ($1, 'physical_drive', $2, 'staged', $3, $3, $4)",
    )
    .bind(staging_id)
    .bind(&descriptor)
    .bind(bytes)
    .bind(&requested_by)
    .execute(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    sqlx::query(
        "UPDATE rip_jobs
         SET state = 'registering', import_job_id = staging_id, engine = $2,
             engine_version = $3, medium_json = COALESCE($4, medium_json),
             read_errors_json = $5
         WHERE id = $1",
    )
    .bind(id.as_uuid())
    .bind(report.engine)
    .bind(report.engine_version)
    .bind(report.medium)
    .bind(report.read_errors)
    .execute(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    audit(
        tx,
        "worker",
        &worker_id.to_string(),
        "rip.read",
        id,
        "success",
        serde_json::json!({ "engine": report.engine, "import_id": staging_id.to_string() }),
    )
    .await?;
    Ok(true)
}

/// End a rip as refused or failed, with why.
async fn close(
    tx: &mut Transaction<'_, Postgres>,
    worker_id: WorkerId,
    id: RipJobId,
    state: RipState,
    code: &str,
    detail: Option<&str>,
    medium: Option<&Value>,
) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE rip_jobs
         SET state = $2, error_code = $3, error_detail = $4,
             medium_json = COALESCE($5, medium_json), completed_at = now()
         WHERE id = $1",
    )
    .bind(id.as_uuid())
    .bind(state.as_str())
    .bind(code)
    .bind(detail)
    .bind(medium)
    .execute(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    audit(
        tx,
        "worker",
        &worker_id.to_string(),
        if state == RipState::Refused {
            "rip.refused"
        } else {
            "rip.failed"
        },
        id,
        "failure",
        serde_json::json!({ "error_code": code }),
    )
    .await
}

/// Move rips whose import has ended: complete when the import completed,
/// failed when it failed for good or was canceled. An import that will be
/// retried is waited for. Returns the rips that moved.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn settle_rips(pool: &PgPool) -> Result<Vec<RipJobId>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let completed: Vec<uuid::Uuid> = sqlx::query_scalar(
        "UPDATE rip_jobs r SET state = 'complete', completed_at = now()
         FROM import_jobs i
         WHERE i.id = r.import_job_id AND r.state = 'registering' AND i.state = 'complete'
         RETURNING r.id",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let failed: Vec<uuid::Uuid> = sqlx::query_scalar(
        "UPDATE rip_jobs r
         SET state = 'failed', error_code = 'IMPORT_FAILED',
             error_detail = 'the import of what was read ended as ' || i.state
                 || COALESCE(' (' || i.error_code || ')', ''),
             completed_at = now()
         FROM import_jobs i
         WHERE i.id = r.import_job_id AND r.state = 'registering'
           AND i.state IN ('failed_terminal', 'canceled', 'quarantined')
         RETURNING r.id",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    for (ids, action, outcome) in [
        (&completed, "rip.completed", "success"),
        (&failed, "rip.import_failed", "failure"),
    ] {
        for id in ids {
            audit(
                &mut tx,
                "system",
                "import-runner",
                action,
                RipJobId::from_uuid(*id),
                outcome,
                serde_json::json!({}),
            )
            .await?;
        }
    }
    tx.commit().await.map_err(DbError::Query)?;
    Ok(completed
        .into_iter()
        .chain(failed)
        .map(RipJobId::from_uuid)
        .collect())
}

async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    actor_type: &str,
    actor: &str,
    action: &str,
    id: RipJobId,
    outcome: &str,
    metadata: Value,
) -> Result<(), DbError> {
    sqlx::query(
        "INSERT INTO audit_events
             (id, actor_type, actor_id, action, target_type, target_id, outcome, metadata)
         VALUES ($1, $2, $3, $4, 'rip', $5, $6, $7)",
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
