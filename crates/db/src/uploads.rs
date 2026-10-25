// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Uploads sent in parts (docs/plans/acquisition.md A3).
//!
//! An upload belongs to the account that started it: nobody else sees it or
//! adds to it, and every lookup here says "not found" to anyone else rather
//! than "not yours". Its id is its staging area's and, once complete, its
//! import's.

use sqlx::{PgPool, Row};
use tangible_domain::upload::PartPlan;
use time::OffsetDateTime;

use crate::DbError;
use crate::repositories::{IMPORT_JOB_COLUMNS, ImportJobRecord, import_job_from_row};

/// How long an upload may sit with no part arriving before it is abandoned.
pub const IDLE_HOURS: i32 = 24;

/// An upload, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadRecord {
    /// Its id, also its staging area's and its import's.
    pub id: uuid::Uuid,
    /// The file's name.
    pub filename: String,
    /// The whole file's length.
    pub total_bytes: u64,
    /// Every part's length but the last's.
    pub part_size: u64,
    /// The whole file's SHA-256, when the uploader gave it.
    pub expected_sha256: Option<String>,
    /// `open`, `completed` or `abandoned`.
    pub state: String,
    /// Who started it.
    pub created_by: String,
    /// The parts that have arrived, in order.
    pub received_parts: Vec<u64>,
    /// Bytes in those parts.
    pub bytes_received: u64,
    /// When it was started.
    pub created_at: OffsetDateTime,
    /// When it is abandoned if nothing more arrives.
    pub expires_at: OffsetDateTime,
}

impl UploadRecord {
    /// How it divides into parts.
    #[must_use]
    pub fn plan(&self) -> Option<PartPlan> {
        PartPlan::new(self.total_bytes, self.part_size)
    }
}

/// What a caller asked for that the upload's state does not allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadOutcome {
    /// No such upload, or not this account's.
    NotFound,
    /// It is completed or abandoned.
    NotOpen(String),
    /// Parts are missing, or a part has the wrong length.
    Incomplete {
        /// The parts still to send.
        missing: Vec<u64>,
    },
}

/// A new upload.
#[derive(Debug, Clone, Copy)]
pub struct NewUpload<'a> {
    /// Its id, chosen by the caller, who has already made its staging area.
    pub id: uuid::Uuid,
    /// The file's name, already checked.
    pub filename: &'a str,
    /// The whole file's length.
    pub total_bytes: u64,
    /// The part size.
    pub part_size: u64,
    /// The whole file's SHA-256, lowercase hex, when given.
    pub expected_sha256: Option<&'a str>,
    /// Who is starting it.
    pub created_by: &'a str,
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

async fn load(pool: &PgPool, id: uuid::Uuid) -> Result<Option<UploadRecord>, DbError> {
    let Some(row) = sqlx::query(
        "SELECT id, filename, total_bytes, part_size, expected_sha256, state, created_by,
                created_at, expires_at
         FROM uploads WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?
    else {
        return Ok(None);
    };
    let parts = sqlx::query(
        "SELECT part_number, bytes FROM upload_parts WHERE upload_id = $1 ORDER BY part_number",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    let get = DbError::Query;
    let mut received_parts = Vec::with_capacity(parts.len());
    let mut bytes_received = 0_u64;
    for part in &parts {
        let number: i32 = part.try_get("part_number").map_err(get)?;
        let bytes: i64 = part.try_get("bytes").map_err(get)?;
        received_parts.push(u64::try_from(number).unwrap_or(0));
        bytes_received = bytes_received.saturating_add(to_u64(bytes));
    }
    Ok(Some(UploadRecord {
        id: row.try_get("id").map_err(get)?,
        filename: row.try_get("filename").map_err(get)?,
        total_bytes: to_u64(row.try_get("total_bytes").map_err(get)?),
        part_size: to_u64(row.try_get("part_size").map_err(get)?),
        expected_sha256: row.try_get("expected_sha256").map_err(get)?,
        state: row.try_get("state").map_err(get)?,
        created_by: row.try_get("created_by").map_err(get)?,
        received_parts,
        bytes_received,
        created_at: row.try_get("created_at").map_err(get)?,
        expires_at: row.try_get("expires_at").map_err(get)?,
    }))
}

/// Record a new upload, open for parts.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn create_upload(pool: &PgPool, new: NewUpload<'_>) -> Result<UploadRecord, DbError> {
    sqlx::query(
        "INSERT INTO uploads
             (id, filename, total_bytes, part_size, expected_sha256, created_by, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(hours => $7))",
    )
    .bind(new.id)
    .bind(new.filename)
    .bind(to_i64(new.total_bytes))
    .bind(to_i64(new.part_size))
    .bind(new.expected_sha256)
    .bind(new.created_by)
    .bind(IDLE_HOURS)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    load(pool, new.id).await?.ok_or(DbError::Enum {
        column: "uploads.id",
        value: new.id.to_string(),
    })
}

/// An upload, if it is `user`'s.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn get_upload(
    pool: &PgPool,
    id: uuid::Uuid,
    user: &str,
) -> Result<Option<UploadRecord>, DbError> {
    Ok(load(pool, id)
        .await?
        .filter(|upload| upload.created_by == user))
}

/// Record that part `number` arrived whole, replacing any earlier copy, and
/// put off the upload's expiry.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure. An upload that is not `user`'s,
/// or not open, is an [`UploadOutcome`].
pub async fn record_part(
    pool: &PgPool,
    id: uuid::Uuid,
    user: &str,
    number: u64,
    bytes: u64,
    sha256: &str,
) -> Result<Result<UploadRecord, UploadOutcome>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let state: Option<(String, String)> =
        sqlx::query_as("SELECT state, created_by FROM uploads WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(DbError::Query)?;
    match state {
        Some((_, owner)) if owner != user => return Ok(Err(UploadOutcome::NotFound)),
        None => return Ok(Err(UploadOutcome::NotFound)),
        Some((state, _)) if state != "open" => return Ok(Err(UploadOutcome::NotOpen(state))),
        Some(_) => {}
    }
    sqlx::query(
        "INSERT INTO upload_parts (upload_id, part_number, bytes, sha256)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (upload_id, part_number)
         DO UPDATE SET bytes = EXCLUDED.bytes, sha256 = EXCLUDED.sha256, updated_at = now()",
    )
    .bind(id)
    .bind(i32::try_from(number).unwrap_or(i32::MAX))
    .bind(to_i64(bytes))
    .bind(sha256)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    sqlx::query(
        "UPDATE uploads SET updated_at = now(),
                expires_at = now() + make_interval(hours => $2)
         WHERE id = $1",
    )
    .bind(id)
    .bind(IDLE_HOURS)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(load(pool, id).await?.ok_or(UploadOutcome::NotFound))
}

/// Close an upload whose every part has arrived, and queue its import, staged
/// already, with `descriptor`. Repeating it answers the same import.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure. An upload that is not `user`'s,
/// not open, or missing parts, is an [`UploadOutcome`].
pub async fn complete_upload(
    pool: &PgPool,
    id: uuid::Uuid,
    user: &str,
    descriptor: &serde_json::Value,
) -> Result<Result<ImportJobRecord, UploadOutcome>, DbError> {
    let Some(upload) = get_upload(pool, id, user).await? else {
        return Ok(Err(UploadOutcome::NotFound));
    };
    if upload.state == "completed" {
        let row = sqlx::query(&format!(
            "SELECT {IMPORT_JOB_COLUMNS} FROM import_jobs WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(DbError::Query)?;
        return match row {
            Some(row) => Ok(Ok(import_job_from_row(&row)?)),
            None => Ok(Err(UploadOutcome::NotOpen(upload.state))),
        };
    }
    if upload.state != "open" {
        return Ok(Err(UploadOutcome::NotOpen(upload.state)));
    }

    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM uploads WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(DbError::Query)?;
    if state.as_deref() != Some("open") {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(UploadOutcome::NotOpen(state.unwrap_or_default())));
    }
    // Every part, each the length its place in the file says.
    let parts: Vec<(i32, i64)> =
        sqlx::query_as("SELECT part_number, bytes FROM upload_parts WHERE upload_id = $1")
            .bind(id)
            .fetch_all(&mut *tx)
            .await
            .map_err(DbError::Query)?;
    let Some(plan) = upload.plan() else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(UploadOutcome::NotOpen("broken".to_owned())));
    };
    let whole: Vec<u64> = parts
        .iter()
        .filter(|(number, bytes)| {
            u64::try_from(*number)
                .ok()
                .and_then(|number| plan.part(number))
                .is_some_and(|(_, length)| length == to_u64(*bytes))
        })
        .filter_map(|(number, _)| u64::try_from(*number).ok())
        .collect();
    let missing = plan.missing(&whole);
    if !missing.is_empty() {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(UploadOutcome::Incomplete { missing }));
    }

    let row = sqlx::query(&format!(
        "INSERT INTO import_jobs
             (id, source_kind, source_descriptor_json, state, bytes_expected,
              bytes_received, created_by)
         VALUES ($1, 'upload', $2, 'staged', $3, $3, $4)
         RETURNING {IMPORT_JOB_COLUMNS}"
    ))
    .bind(id)
    .bind(descriptor)
    .bind(to_i64(upload.total_bytes))
    .bind(user)
    .fetch_one(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    sqlx::query("UPDATE uploads SET state = 'completed', updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(Ok(import_job_from_row(&row)?))
}

/// Abandon an open upload of `user`'s. The caller removes its staging area.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure. One that is not `user`'s, or
/// not open, is an [`UploadOutcome`].
pub async fn abandon_upload(
    pool: &PgPool,
    id: uuid::Uuid,
    user: &str,
) -> Result<Result<(), UploadOutcome>, DbError> {
    let Some(upload) = get_upload(pool, id, user).await? else {
        return Ok(Err(UploadOutcome::NotFound));
    };
    let updated = sqlx::query(
        "UPDATE uploads SET state = 'abandoned', updated_at = now()
         WHERE id = $1 AND state = 'open'",
    )
    .bind(id)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    if updated.rows_affected() == 0 {
        return Ok(Err(UploadOutcome::NotOpen(upload.state)));
    }
    Ok(Ok(()))
}

/// Abandon every open upload left idle past its expiry, answering their ids
/// so the caller can remove their staging areas.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn expire_uploads(pool: &PgPool) -> Result<Vec<uuid::Uuid>, DbError> {
    sqlx::query_scalar(
        "UPDATE uploads SET state = 'abandoned', updated_at = now()
         WHERE state = 'open' AND expires_at < now()
         RETURNING id",
    )
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)
}
