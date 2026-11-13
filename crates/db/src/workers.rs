// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Workers as people manage them (docs/plans/worker-admin.md): seen, drained,
//! enabled again, revoked, and asked to rotate their credential
//! (docs/plans/credential-rotation.md).
//!
//! What a worker does for itself (enrolling, reporting, heartbeats, claims)
//! is in [`crate::repositories`]. Here is what an administrator does to one,
//! each change audited in the same transaction. A drain or a revocation
//! takes the worker's row lock, so it waits for a claim the worker has under
//! way and the next claim sees it.

use sqlx::{PgPool, Postgres, Row as _, Transaction};
use tangible_domain::{DriveId, WorkerId};
use time::OffsetDateTime;

use crate::DbError;

/// A worker, as the list of workers shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerRecord {
    /// The worker.
    pub id: WorkerId,
    /// Its name.
    pub name: String,
    /// `pending`, `online`, `offline`, `draining`, `revoked` or
    /// `incompatible`.
    pub status: String,
    /// The Tangible version it runs.
    pub software_version: String,
    /// The protocol version it speaks.
    pub protocol_version: String,
    /// When it last reported in.
    pub last_seen_at: Option<OffsetDateTime>,
    /// When it enrolled.
    pub created_at: OffsetDateTime,
    /// When it was revoked.
    pub revoked_at: Option<OffsetDateTime>,
    /// When the credential it uses was issued.
    pub credential_issued_at: OffsetDateTime,
    /// When an administrator asked for that credential to be rotated, until
    /// it is.
    pub credential_rotation_requested_at: Option<OffsetDateTime>,
    /// Its drives, by name.
    pub drives: Vec<(DriveId, String)>,
}

/// What a change asked of a worker came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerChange {
    /// Done, or already so: the worker as it now is.
    Done(WorkerRecord),
    /// No such worker.
    NotFound,
    /// It is revoked or incompatible, which this change cannot apply to; its
    /// status.
    Refused(String),
}

const WORKER_COLUMNS: &str = "w.id, w.name, w.status, w.software_version, w.protocol_version, \
     w.last_seen_at, w.created_at, w.revoked_at, \
     w.credential_issued_at, w.credential_rotation_requested_at, \
     COALESCE((SELECT jsonb_agg(jsonb_build_array(d.id, d.configured_name) \
                       ORDER BY d.configured_name, d.id) \
               FROM drives d WHERE d.worker_id = w.id), '[]'::jsonb) AS drives";

fn worker_from_row(row: &sqlx::postgres::PgRow) -> Result<WorkerRecord, DbError> {
    let drives: serde_json::Value = row.try_get("drives").map_err(DbError::Query)?;
    let drives = drives
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|pair| {
            let id = pair.get(0)?.as_str()?.parse::<uuid::Uuid>().ok()?;
            let name = pair.get(1)?.as_str()?.to_owned();
            Some((DriveId::from_uuid(id), name))
        })
        .collect();
    Ok(WorkerRecord {
        id: WorkerId::from_uuid(row.try_get("id").map_err(DbError::Query)?),
        name: row.try_get("name").map_err(DbError::Query)?,
        status: row.try_get("status").map_err(DbError::Query)?,
        software_version: row.try_get("software_version").map_err(DbError::Query)?,
        protocol_version: row.try_get("protocol_version").map_err(DbError::Query)?,
        last_seen_at: row.try_get("last_seen_at").map_err(DbError::Query)?,
        created_at: row.try_get("created_at").map_err(DbError::Query)?,
        revoked_at: row.try_get("revoked_at").map_err(DbError::Query)?,
        credential_issued_at: row
            .try_get("credential_issued_at")
            .map_err(DbError::Query)?,
        credential_rotation_requested_at: row
            .try_get("credential_rotation_requested_at")
            .map_err(DbError::Query)?,
        drives,
    })
}

/// A page of workers, oldest first, after `after`.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn list_workers(
    pool: &PgPool,
    limit: i64,
    after: Option<uuid::Uuid>,
) -> Result<Vec<WorkerRecord>, DbError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {WORKER_COLUMNS} FROM workers w
         WHERE ($1::uuid IS NULL OR w.id > $1)
         ORDER BY w.id
         LIMIT $2"
    )))
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter().map(worker_from_row).collect()
}

/// One worker.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn get_worker(pool: &PgPool, id: WorkerId) -> Result<Option<WorkerRecord>, DbError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {WORKER_COLUMNS} FROM workers w WHERE w.id = $1"
    )))
    .bind(id.as_uuid())
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    row.as_ref().map(worker_from_row).transpose()
}

async fn read_in(
    tx: &mut Transaction<'_, Postgres>,
    id: WorkerId,
) -> Result<Option<WorkerRecord>, DbError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {WORKER_COLUMNS} FROM workers w WHERE w.id = $1"
    )))
    .bind(id.as_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    row.as_ref().map(worker_from_row).transpose()
}

/// The status of a worker, its row locked against change until the
/// transaction ends.
async fn lock_status(
    tx: &mut Transaction<'_, Postgres>,
    id: WorkerId,
) -> Result<Option<String>, DbError> {
    sqlx::query_scalar("SELECT status FROM workers WHERE id = $1 FOR UPDATE")
        .bind(id.as_uuid())
        .fetch_optional(&mut **tx)
        .await
        .map_err(DbError::Query)
}

/// Apply `update` to a worker whose locked status `allowed` accepts, audit it
/// as `action` by `actor` when it changed anything, and answer the worker
/// as it now is.
async fn change(
    pool: &PgPool,
    id: WorkerId,
    actor: &str,
    action: &str,
    allowed: fn(&str) -> Option<bool>,
    update: &'static str,
) -> Result<WorkerChange, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let Some(status) = lock_status(&mut tx, id).await? else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(WorkerChange::NotFound);
    };
    // Some(true): change it; Some(false): it is already so; None: refused.
    let Some(changes) = allowed(&status) else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(WorkerChange::Refused(status));
    };
    if changes {
        sqlx::query(update)
            .bind(id.as_uuid())
            .execute(&mut *tx)
            .await
            .map_err(DbError::Query)?;
        crate::accounts::audit(
            &mut tx,
            actor,
            action,
            &id.to_string(),
            serde_json::json!({ "from": status }),
        )
        .await?;
    }
    let record = read_in(&mut tx, id).await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(record.map_or(WorkerChange::NotFound, WorkerChange::Done))
}

/// Drain a worker: it finishes what it holds and takes nothing new.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn drain_worker(
    pool: &PgPool,
    id: WorkerId,
    actor: &str,
) -> Result<WorkerChange, DbError> {
    change(
        pool,
        id,
        actor,
        "worker.drained",
        |status| match status {
            "pending" | "online" | "offline" => Some(true),
            "draining" => Some(false),
            _ => None,
        },
        "UPDATE workers SET status = 'draining', updated_at = now() WHERE id = $1",
    )
    .await
}

/// Undo a drain: online again, or pending if the worker was never seen.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn enable_worker(
    pool: &PgPool,
    id: WorkerId,
    actor: &str,
) -> Result<WorkerChange, DbError> {
    change(
        pool,
        id,
        actor,
        "worker.enabled",
        |status| match status {
            "draining" => Some(true),
            "pending" | "online" | "offline" => Some(false),
            _ => None,
        },
        "UPDATE workers
         SET status = CASE WHEN last_seen_at IS NULL THEN 'pending' ELSE 'online' END,
             updated_at = now()
         WHERE id = $1",
    )
    .await
}

/// Revoke a worker: its credential stops working at once, for good.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn revoke_worker(
    pool: &PgPool,
    id: WorkerId,
    actor: &str,
) -> Result<WorkerChange, DbError> {
    change(
        pool,
        id,
        actor,
        "worker.revoked",
        |status| Some(status != "revoked"),
        "UPDATE workers SET status = 'revoked', revoked_at = now(), updated_at = now()
         WHERE id = $1",
    )
    .await
}

/// Ask a worker to rotate its credential: its next heartbeat says so, and
/// a worker able to rotates before it takes more work. The first request's
/// time is kept until the rotation happens; a revoked worker cannot be
/// asked.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn request_credential_rotation(
    pool: &PgPool,
    id: WorkerId,
    actor: &str,
) -> Result<WorkerChange, DbError> {
    change(
        pool,
        id,
        actor,
        "worker.credential_rotation_requested",
        |status| (status != "revoked").then_some(true),
        "UPDATE workers
         SET credential_rotation_requested_at = COALESCE(credential_rotation_requested_at, now()),
             updated_at = now()
         WHERE id = $1",
    )
    .await
}

/// Whether a worker is to be given no new work: draining, or revoked or
/// incompatible. Read under a share lock, so a drain or revocation waits for
/// the claim that asks, and a claim that was waiting on one sees it: a
/// worker that authenticated just before it was revoked takes nothing.
pub(crate) async fn takes_no_new_work(
    tx: &mut Transaction<'_, Postgres>,
    id: WorkerId,
) -> Result<bool, DbError> {
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM workers WHERE id = $1 FOR SHARE")
            .bind(id.as_uuid())
            .fetch_optional(&mut **tx)
            .await
            .map_err(DbError::Query)?;
    Ok(!matches!(
        status.as_deref(),
        Some("pending" | "online" | "offline")
    ))
}
