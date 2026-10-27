// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Removing an import's source once the library has it
//! (docs/plans/source-retention.md, ADR 0017): the rows
//! `complete_import_job` writes for an integration that removes, claimed
//! with a lease by the remover, moved only as
//! [`SourceRemovalState::can_become`] allows, and ended with an audit event
//! when something was removed.

use sqlx::{PgPool, Row};
use tangible_domain::SourceRemovalState;
use time::OffsetDateTime;

use crate::DbError;

/// A removal as stored, with what the remover needs of its import.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceRemovalRecord {
    /// The removal.
    pub id: uuid::Uuid,
    /// The import whose source it removes.
    pub import_job_id: uuid::Uuid,
    /// The integration the source is in.
    pub integration_id: uuid::Uuid,
    /// The artifact the import made, which must be whole first.
    pub artifact_id: uuid::Uuid,
    /// The integrity check asked for, once asked.
    pub check_id: Option<uuid::Uuid>,
    /// Where it stands.
    pub state: SourceRemovalState,
    /// Why it waits, or how it ended, in words.
    pub detail: Option<String>,
    /// Files removed, where counted.
    pub files_removed: Option<i32>,
    /// Bytes removed, where known.
    pub bytes_removed: Option<i64>,
    /// Files left because they were not the library's.
    pub files_kept: Option<i32>,
    /// Tries at the source that failed.
    pub attempts: i32,
    /// When it is next looked at, if held off.
    pub next_attempt_at: Option<OffsetDateTime>,
    /// When it was asked for: when the import completed.
    pub requested_at: OffsetDateTime,
    /// When it ended.
    pub ended_at: Option<OffsetDateTime>,
    /// The import's source kind, such as `qbittorrent_job`.
    pub source_kind: String,
    /// The import's source descriptor.
    pub source_descriptor: serde_json::Value,
    /// The client's id for the job (an infohash, an `nzo_id`, an NZBID),
    /// for a download client's import: from the import's descriptor, as the
    /// runner reads it (a Usenet job's `integration_jobs.client_job_id` is
    /// Tangible's own, the NZB's SHA-256).
    pub client_job_id: Option<String>,
    /// The path beneath the client's download folder the import was copied
    /// from, once recorded.
    pub content_path: Option<String>,
}

const COLUMNS: &str = "r.id, r.import_job_id, r.integration_id, r.artifact_id, r.check_id, \
     r.state, r.detail, r.files_removed, r.bytes_removed, r.files_kept, r.attempts, \
     r.next_attempt_at, r.requested_at, r.ended_at, i.source_kind, \
     i.source_descriptor_json AS source_descriptor, \
     COALESCE(i.source_descriptor_json->>'client_job_id', \
              i.source_descriptor_json->>'infohash', j.client_job_id) AS client_job_id, \
     j.content_path";

const FROM: &str = "source_removals r JOIN import_jobs i ON i.id = r.import_job_id \
     LEFT JOIN integration_jobs j ON j.import_job_id = r.import_job_id";

fn from_row(row: &sqlx::postgres::PgRow) -> Result<SourceRemovalRecord, DbError> {
    let get = |error| DbError::Query(error);
    let state: String = row.try_get("state").map_err(get)?;
    Ok(SourceRemovalRecord {
        id: row.try_get("id").map_err(get)?,
        import_job_id: row.try_get("import_job_id").map_err(get)?,
        integration_id: row.try_get("integration_id").map_err(get)?,
        artifact_id: row.try_get("artifact_id").map_err(get)?,
        check_id: row.try_get("check_id").map_err(get)?,
        state: state
            .parse()
            .map_err(|error| DbError::Query(sqlx::Error::Decode(Box::new(error))))?,
        detail: row.try_get("detail").map_err(get)?,
        files_removed: row.try_get("files_removed").map_err(get)?,
        bytes_removed: row.try_get("bytes_removed").map_err(get)?,
        files_kept: row.try_get("files_kept").map_err(get)?,
        attempts: row.try_get("attempts").map_err(get)?,
        next_attempt_at: row.try_get("next_attempt_at").map_err(get)?,
        requested_at: row.try_get("requested_at").map_err(get)?,
        ended_at: row.try_get("ended_at").map_err(get)?,
        source_kind: row.try_get("source_kind").map_err(get)?,
        source_descriptor: row.try_get("source_descriptor").map_err(get)?,
        client_job_id: row.try_get("client_job_id").map_err(get)?,
        content_path: row.try_get("content_path").map_err(get)?,
    })
}

fn bounded(detail: &str) -> String {
    detail.chars().take(2000).collect()
}

/// Take the next removal that is due, with a lease of `lease_seconds`.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn claim_next_removal(
    pool: &PgPool,
    owner: &str,
    lease_seconds: i64,
) -> Result<Option<SourceRemovalRecord>, DbError> {
    let claimed: Option<uuid::Uuid> = sqlx::query_scalar(
        "UPDATE source_removals SET
             leased_by = $1, leased_until = now() + ($2::bigint * interval '1 second')
         WHERE id = (
             SELECT id FROM source_removals
             WHERE state IN ('verifying', 'removing')
               AND (next_attempt_at IS NULL OR next_attempt_at <= now())
               AND (leased_until IS NULL OR leased_until < now())
             ORDER BY next_attempt_at NULLS FIRST, id
             LIMIT 1
             FOR UPDATE SKIP LOCKED
         )
         RETURNING id",
    )
    .bind(owner)
    .bind(lease_seconds)
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    match claimed {
        Some(id) => get_removal(pool, id).await,
        None => Ok(None),
    }
}

/// The removal `id`.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn get_removal(
    pool: &PgPool,
    id: uuid::Uuid,
) -> Result<Option<SourceRemovalRecord>, DbError> {
    let row = sqlx::query(&format!("SELECT {COLUMNS} FROM {FROM} WHERE r.id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(DbError::Query)?;
    row.as_ref().map(from_row).transpose()
}

/// The removals of `import_ids`, those that have one.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn removals_for_imports(
    pool: &PgPool,
    import_ids: &[uuid::Uuid],
) -> Result<Vec<SourceRemovalRecord>, DbError> {
    if import_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM {FROM} WHERE r.import_job_id = ANY($1)"
    ))
    .bind(import_ids)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter().map(from_row).collect()
}

/// Hold a removal held by `owner` until `wait_seconds` from now, saying why
/// (or keeping what it said), with the check it waits on and whether the
/// wait is for a failed try at the source. Answers whether it was held.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn hold_removal(
    pool: &PgPool,
    id: uuid::Uuid,
    owner: &str,
    hold: Hold<'_>,
) -> Result<bool, DbError> {
    let done = sqlx::query(
        "UPDATE source_removals SET
             check_id = CASE WHEN $3 THEN $4 ELSE check_id END,
             detail = CASE WHEN $5 THEN $6 ELSE detail END,
             attempts = attempts + CASE WHEN $7 THEN 1 ELSE 0 END,
             next_attempt_at = now() + ($8::bigint * interval '1 second'),
             leased_by = NULL, leased_until = NULL
         WHERE id = $1 AND leased_by = $2 AND state IN ('verifying', 'removing')",
    )
    .bind(id)
    .bind(owner)
    .bind(hold.check.is_some())
    .bind(hold.check.flatten())
    .bind(hold.detail.is_some())
    .bind(hold.detail.flatten().map(bounded))
    .bind(hold.failed_try)
    .bind(hold.wait_seconds)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(done.rows_affected() > 0)
}

/// How a removal is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Hold<'a> {
    /// The check to wait on: `Some(None)` forgets the one asked for, `None`
    /// keeps it.
    pub check: Option<Option<uuid::Uuid>>,
    /// Why it waits: `Some(None)` clears it, `None` keeps it.
    pub detail: Option<Option<&'a str>>,
    /// Whether a try at the source failed, counted for the backoff.
    pub failed_try: bool,
    /// How long until it is looked at again.
    pub wait_seconds: i64,
}

/// Move a removal held by `owner` from `verifying` to `removing`, its copy
/// whole, for the remover to go straight on to the source. Answers whether
/// it moved.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn begin_removing(pool: &PgPool, id: uuid::Uuid, owner: &str) -> Result<bool, DbError> {
    let done = sqlx::query(
        "UPDATE source_removals SET state = 'removing', detail = NULL, attempts = 0,
             next_attempt_at = NULL
         WHERE id = $1 AND leased_by = $2 AND state = 'verifying'",
    )
    .bind(id)
    .bind(owner)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(done.rows_affected() > 0)
}

/// How a removal ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ending<'a> {
    /// The state it ends in, one that may follow the one it is in.
    pub state: SourceRemovalState,
    /// Why, or what was removed, in words.
    pub detail: &'a str,
    /// Files removed, where counted.
    pub files_removed: Option<i32>,
    /// Bytes removed, where known.
    pub bytes_removed: Option<i64>,
    /// Files left because they were not the library's.
    pub files_kept: Option<i32>,
    /// What was removed, for the audit event, when something was.
    pub removed: Option<serde_json::Value>,
}

/// End a removal held by `owner`, and audit what was removed, together.
/// Answers whether it ended: not when it was not held, or not in a state
/// `ending.state` may follow.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure; [`DbError::Query`] too for an
/// ending that is not one (an open state).
pub async fn end_removal(
    pool: &PgPool,
    id: uuid::Uuid,
    owner: &str,
    ending: &Ending<'_>,
) -> Result<bool, DbError> {
    if ending.state.is_open() {
        return Err(DbError::Query(sqlx::Error::Protocol(format!(
            "{} is not an ending",
            ending.state
        ))));
    }
    let from: Vec<&str> = [SourceRemovalState::Verifying, SourceRemovalState::Removing]
        .into_iter()
        .filter(|from| from.can_become(ending.state))
        .map(|from| from.as_str())
        .collect();
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let ended: Option<(uuid::Uuid, uuid::Uuid)> = sqlx::query_as(
        "UPDATE source_removals SET
             state = $3, detail = $4, files_removed = $5, bytes_removed = $6, files_kept = $7,
             ended_at = now(), next_attempt_at = NULL, leased_by = NULL, leased_until = NULL
         WHERE id = $1 AND leased_by = $2 AND state = ANY($8)
         RETURNING integration_id, import_job_id",
    )
    .bind(id)
    .bind(owner)
    .bind(ending.state.as_str())
    .bind(bounded(ending.detail))
    .bind(ending.files_removed)
    .bind(ending.bytes_removed)
    .bind(ending.files_kept)
    .bind(&from)
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let Some((integration_id, import_id)) = ended else {
        return Ok(false);
    };
    if let Some(removed) = &ending.removed {
        sqlx::query(
            "INSERT INTO audit_events
                 (id, actor_type, actor_id, action, target_type, target_id, outcome, metadata)
             VALUES ($1, 'system', 'source-removal', 'integration.source_deleted',
                     'integration', $2, 'success', $3)",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(integration_id.to_string())
        .bind(serde_json::json!({
            "removal_id": id,
            "import_id": import_id,
            "removed": removed,
        }))
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    }
    tx.commit().await.map_err(DbError::Query)?;
    Ok(true)
}

/// The SHA-256 and length of every component of `artifact_id`: what the
/// library holds of it, which a file must be to be deleted at its source.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn artifact_contents(
    pool: &PgPool,
    artifact_id: uuid::Uuid,
) -> Result<Vec<(String, i64)>, DbError> {
    sqlx::query_as(
        "SELECT sha256, length_bytes FROM artifact_components WHERE artifact_id = $1
         ORDER BY sha256",
    )
    .bind(artifact_id)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)
}
