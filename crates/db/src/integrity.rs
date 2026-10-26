// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Integrity checks (docs/plans/storage-integrity.md): the queue, progress,
//! what was found, and what it means for each artifact.
//!
//! A check reads objects in digest order and records the last digest it
//! finished, so a check taken over after a lapsed lease resumes rather than
//! starts again. Problems mark the artifacts naming the object damaged as
//! they are found; a finished check marks the artifacts it covered intact
//! unless this check found them damaged.

use serde_json::Value;
use sqlx::{PgPool, Row};
use tangible_domain::{IntegrityCheckId, IntegrityCheckState, IntegrityScope};
use time::OffsetDateTime;

use crate::DbError;

/// How many problems a check keeps; it counts them all.
pub const MAX_KEPT_PROBLEMS: usize = 1000;

/// An integrity check as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct IntegrityCheckRecord {
    /// The check.
    pub id: IntegrityCheckId,
    /// What it reads.
    pub scope: IntegrityScope,
    /// The artifact, for a check of one.
    pub artifact_id: Option<uuid::Uuid>,
    /// Where it is.
    pub state: IntegrityCheckState,
    /// How many objects it will read, once counted.
    pub objects_total: Option<i64>,
    /// How many bytes it will read, once counted.
    pub bytes_total: Option<i64>,
    /// How many objects it has read.
    pub objects_checked: i64,
    /// How many bytes it has read.
    pub bytes_checked: i64,
    /// The last digest it finished.
    pub resume_after: Option<String>,
    /// The first problems it found, as `{sha256?, kind, detail}`.
    pub problems: Value,
    /// How many problems it found in all.
    pub problem_count: i32,
    /// Why it failed, for one that did.
    pub error_code: Option<String>,
    /// Why, in words.
    pub error_detail: Option<String>,
    /// Who asked: a username, or `scheduler`.
    pub created_by: String,
    /// When it was asked for.
    pub created_at: OffsetDateTime,
    /// When it started.
    pub started_at: Option<OffsetDateTime>,
    /// When it ended.
    pub completed_at: Option<OffsetDateTime>,
}

const COLUMNS: &str = "id, scope, artifact_id, state, objects_total, bytes_total, objects_checked,
     bytes_checked, resume_after, problems, problem_count, error_code, error_detail,
     created_by, created_at, started_at, completed_at";

/// A stored enum value, or the drift between column and domain reported.
fn stored<T: std::str::FromStr>(column: &'static str, value: &str) -> Result<T, DbError> {
    value.parse().map_err(|_| DbError::Enum {
        column,
        value: value.to_owned(),
    })
}

fn from_row(row: &sqlx::postgres::PgRow) -> Result<IntegrityCheckRecord, DbError> {
    let get = DbError::Query;
    let scope: String = row.try_get("scope").map_err(get)?;
    let state: String = row.try_get("state").map_err(get)?;
    Ok(IntegrityCheckRecord {
        id: IntegrityCheckId::from_uuid(row.try_get("id").map_err(get)?),
        scope: stored("integrity_checks.scope", &scope)?,
        artifact_id: row.try_get("artifact_id").map_err(get)?,
        state: stored("integrity_checks.state", &state)?,
        objects_total: row.try_get("objects_total").map_err(get)?,
        bytes_total: row.try_get("bytes_total").map_err(get)?,
        objects_checked: row.try_get("objects_checked").map_err(get)?,
        bytes_checked: row.try_get("bytes_checked").map_err(get)?,
        resume_after: row.try_get("resume_after").map_err(get)?,
        problems: row.try_get("problems").map_err(get)?,
        problem_count: row.try_get("problem_count").map_err(get)?,
        error_code: row.try_get("error_code").map_err(get)?,
        error_detail: row.try_get("error_detail").map_err(get)?,
        created_by: row.try_get("created_by").map_err(get)?,
        created_at: row.try_get("created_at").map_err(get)?,
        started_at: row.try_get("started_at").map_err(get)?,
        completed_at: row.try_get("completed_at").map_err(get)?,
    })
}

/// What asking for a check came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Requested {
    /// A new check, queued.
    Queued(IntegrityCheckRecord),
    /// One for the same scope was already queued or running.
    AlreadyActive(IntegrityCheckRecord),
    /// No such artifact.
    NoSuchArtifact,
}

/// Ask for a check of `artifact_id`, or of everything when `None`.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn request_check(
    pool: &PgPool,
    artifact_id: Option<uuid::Uuid>,
    actor: &str,
) -> Result<Requested, DbError> {
    let scope = if artifact_id.is_some() {
        IntegrityScope::Artifact
    } else {
        IntegrityScope::All
    };
    let inserted = sqlx::query(&format!(
        "INSERT INTO integrity_checks (id, scope, artifact_id, created_by)
         VALUES ($1, $2, $3, $4)
         RETURNING {COLUMNS}"
    ))
    .bind(uuid::Uuid::now_v7())
    .bind(scope.as_str())
    .bind(artifact_id)
    .bind(actor)
    .fetch_one(pool)
    .await;
    match inserted {
        Ok(row) => Ok(Requested::Queued(from_row(&row)?)),
        Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("23503") => {
            Ok(Requested::NoSuchArtifact)
        }
        Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("23505") => {
            let row = sqlx::query(&format!(
                "SELECT {COLUMNS} FROM integrity_checks
                 WHERE scope = $1 AND artifact_id IS NOT DISTINCT FROM $2
                   AND state IN ('queued', 'running')"
            ))
            .bind(scope.as_str())
            .bind(artifact_id)
            .fetch_one(pool)
            .await
            .map_err(DbError::Query)?;
            Ok(Requested::AlreadyActive(from_row(&row)?))
        }
        Err(error) => Err(DbError::Query(error)),
    }
}

/// Take the next check to run, with a lease. A check of one artifact goes
/// before a full scrub: it is quick, and somebody is waiting on it.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn claim_next_check(
    pool: &PgPool,
    owner: &str,
    lease_seconds: i64,
) -> Result<Option<IntegrityCheckRecord>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let id: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT id FROM integrity_checks
         WHERE state IN ('queued', 'running')
           AND (lease_expires_at IS NULL OR lease_expires_at < now())
         ORDER BY (scope = 'all'), created_at
         FOR UPDATE SKIP LOCKED
         LIMIT 1",
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let Some(id) = id else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(None);
    };
    let row = sqlx::query(&format!(
        "UPDATE integrity_checks
         SET state = 'running', started_at = COALESCE(started_at, now()),
             lease_owner = $2, lease_expires_at = now() + ($3::int * interval '1 second')
         WHERE id = $1
         RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(owner)
    .bind(seconds(lease_seconds))
    .fetch_one(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    tx.commit().await.map_err(DbError::Query)?;
    from_row(&row).map(Some)
}

fn seconds(value: i64) -> i32 {
    i32::try_from(value.max(1)).unwrap_or(i32::MAX)
}

/// The objects a check reads, after `after` in digest order, up to `limit`:
/// each digest once, with its recorded length.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn objects_after(
    pool: &PgPool,
    check: &IntegrityCheckRecord,
    after: Option<&str>,
    limit: i64,
) -> Result<Vec<(String, i64)>, DbError> {
    let rows = match check.artifact_id {
        Some(artifact_id) => {
            sqlx::query(
                "SELECT DISTINCT sha256, length_bytes AS size FROM artifact_components
                 WHERE artifact_id = $1 AND ($2::text IS NULL OR sha256 > $2)
                 ORDER BY sha256 LIMIT $3",
            )
            .bind(artifact_id)
            .bind(after)
            .bind(limit)
            .fetch_all(pool)
            .await
        }
        None => {
            sqlx::query(
                "SELECT sha256, size_bytes AS size FROM cas_objects
                 WHERE $1::text IS NULL OR sha256 > $1
                 ORDER BY sha256 LIMIT $2",
            )
            .bind(after)
            .bind(limit)
            .fetch_all(pool)
            .await
        }
    }
    .map_err(DbError::Query)?;
    rows.iter()
        .map(|row| {
            Ok((
                row.try_get("sha256").map_err(DbError::Query)?,
                row.try_get("size").map_err(DbError::Query)?,
            ))
        })
        .collect()
}

/// Count what a check will read, and record it.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn count_and_record_totals(
    pool: &PgPool,
    check: &IntegrityCheckRecord,
) -> Result<(i64, i64), DbError> {
    let row = match check.artifact_id {
        Some(artifact_id) => {
            sqlx::query(
                "SELECT count(*) AS objects, COALESCE(sum(size), 0)::bigint AS bytes
                 FROM (SELECT DISTINCT sha256, length_bytes AS size FROM artifact_components
                       WHERE artifact_id = $1) o",
            )
            .bind(artifact_id)
            .fetch_one(pool)
            .await
        }
        None => {
            sqlx::query(
                "SELECT count(*) AS objects, COALESCE(sum(size_bytes), 0)::bigint AS bytes
                 FROM cas_objects",
            )
            .fetch_one(pool)
            .await
        }
    }
    .map_err(DbError::Query)?;
    let objects: i64 = row.try_get("objects").map_err(DbError::Query)?;
    let bytes: i64 = row.try_get("bytes").map_err(DbError::Query)?;
    sqlx::query("UPDATE integrity_checks SET objects_total = $2, bytes_total = $3 WHERE id = $1")
        .bind(check.id.as_uuid())
        .bind(objects)
        .bind(bytes)
        .execute(pool)
        .await
        .map_err(DbError::Query)?;
    Ok((objects, bytes))
}

/// Progress made since the last write.
pub struct Progress<'a> {
    /// The check.
    pub id: IntegrityCheckId,
    /// The process holding it.
    pub owner: &'a str,
    /// Objects read so far, in all.
    pub objects_checked: i64,
    /// Bytes read so far, in all.
    pub bytes_checked: i64,
    /// The last digest finished.
    pub resume_after: Option<&'a str>,
    /// Problems found since the last write, to keep (at most up to
    /// [`MAX_KEPT_PROBLEMS`] in all).
    pub new_problems: &'a [Value],
    /// How many problems were found since the last write, kept or not.
    pub new_problem_count: i32,
    /// How long to hold the lease from now.
    pub lease_seconds: i64,
}

/// Write a check's progress and hold its lease. Answers whether it is still
/// this process's to run: false once it was cancelled or taken over.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn record_progress(pool: &PgPool, progress: &Progress<'_>) -> Result<bool, DbError> {
    let result = sqlx::query(
        "UPDATE integrity_checks
         SET objects_checked = $3, bytes_checked = $4,
             resume_after = COALESCE($5, resume_after),
             problems = CASE WHEN jsonb_array_length(problems) >= $8 THEN problems
                             ELSE problems || $6 END,
             problem_count = problem_count + $7,
             lease_expires_at = now() + ($9::int * interval '1 second')
         WHERE id = $1 AND state = 'running' AND lease_owner = $2",
    )
    .bind(progress.id.as_uuid())
    .bind(progress.owner)
    .bind(progress.objects_checked)
    .bind(progress.bytes_checked)
    .bind(progress.resume_after)
    .bind(Value::Array(progress.new_problems.to_vec()))
    .bind(progress.new_problem_count)
    .bind(i32::try_from(MAX_KEPT_PROBLEMS).unwrap_or(i32::MAX))
    .bind(seconds(progress.lease_seconds))
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(result.rows_affected() > 0)
}

/// Mark every artifact that names object `sha256` damaged, now.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn mark_object_damaged(pool: &PgPool, sha256: &str) -> Result<u64, DbError> {
    sqlx::query(
        "UPDATE artifacts SET integrity_state = 'damaged', integrity_checked_at = now()
         WHERE id IN (SELECT artifact_id FROM artifact_components WHERE sha256 = $1)",
    )
    .bind(sha256)
    .execute(pool)
    .await
    .map(|result| result.rows_affected())
    .map_err(DbError::Query)
}

/// Mark one artifact damaged, now: for a manifest that does not agree.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn mark_artifact_damaged(pool: &PgPool, artifact_id: uuid::Uuid) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE artifacts SET integrity_state = 'damaged', integrity_checked_at = now()
         WHERE id = $1",
    )
    .bind(artifact_id)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}

/// The components the catalogue records for an artifact, as
/// `(logical_path, sha256)`, sorted: what its manifest must name.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn catalogued_components(
    pool: &PgPool,
    artifact_id: uuid::Uuid,
) -> Result<Vec<(String, String)>, DbError> {
    let rows = sqlx::query(
        "SELECT logical_path, sha256 FROM artifact_components
         WHERE artifact_id = $1 ORDER BY logical_path, sha256",
    )
    .bind(artifact_id)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter()
        .map(|row| {
            Ok((
                row.try_get("logical_path").map_err(DbError::Query)?,
                row.try_get("sha256").map_err(DbError::Query)?,
            ))
        })
        .collect()
}

/// Finish a check: complete, and every artifact it covered that it did not
/// find damaged marked intact (a damaged one restored since is intact
/// again), with an audit event when it found anything. Answers whether it
/// was still this process's to finish.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn complete_check(
    pool: &PgPool,
    id: IntegrityCheckId,
    owner: &str,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let row = sqlx::query(&format!(
        "UPDATE integrity_checks
         SET state = 'complete', completed_at = now(), lease_owner = NULL,
             lease_expires_at = NULL
         WHERE id = $1 AND state = 'running' AND lease_owner = $2
         RETURNING {COLUMNS}"
    ))
    .bind(id.as_uuid())
    .bind(owner)
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let Some(row) = row else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(false);
    };
    let check = from_row(&row)?;
    let started = check.started_at.unwrap_or(check.created_at);
    // Damaged in this check means integrity_checked_at at or after its start.
    match check.artifact_id {
        Some(artifact_id) => {
            sqlx::query(
                "UPDATE artifacts SET integrity_state = 'intact', integrity_checked_at = now()
                 WHERE id = $1
                   AND NOT (integrity_state = 'damaged' AND integrity_checked_at >= $2)",
            )
            .bind(artifact_id)
            .bind(started)
            .execute(&mut *tx)
            .await
        }
        None => {
            sqlx::query(
                "UPDATE artifacts SET integrity_state = 'intact', integrity_checked_at = now()
                 WHERE created_at <= $1
                   AND NOT (integrity_state = 'damaged' AND integrity_checked_at >= $1)",
            )
            .bind(started)
            .execute(&mut *tx)
            .await
        }
    }
    .map_err(DbError::Query)?;
    if check.problem_count > 0 {
        sqlx::query(
            "INSERT INTO audit_events
                 (id, actor_type, actor_id, action, target_type, target_id, outcome, metadata)
             VALUES ($1, 'system', 'integrity-check', 'integrity.damage_found',
                     'integrity_check', $2, 'failure', $3)",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(id.to_string())
        .bind(serde_json::json!({
            "scope": check.scope.as_str(),
            "artifact_id": check.artifact_id,
            "problem_count": check.problem_count,
        }))
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    }
    tx.commit().await.map_err(DbError::Query)?;
    Ok(true)
}

/// End a check that could not go on.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn fail_check(
    pool: &PgPool,
    id: IntegrityCheckId,
    code: &str,
    detail: &str,
) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE integrity_checks
         SET state = 'failed', error_code = $2, error_detail = $3, completed_at = now(),
             lease_owner = NULL, lease_expires_at = NULL
         WHERE id = $1 AND state IN ('queued', 'running')",
    )
    .bind(id.as_uuid())
    .bind(code)
    .bind(detail.chars().take(10_000).collect::<String>())
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}

/// Withdraw a check. Answers it as it is now, or `None` when there is none.
/// One already ended is answered unchanged.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn cancel_check(
    pool: &PgPool,
    id: IntegrityCheckId,
) -> Result<Option<IntegrityCheckRecord>, DbError> {
    sqlx::query(
        "UPDATE integrity_checks
         SET state = 'canceled', completed_at = now(), lease_owner = NULL,
             lease_expires_at = NULL
         WHERE id = $1 AND state IN ('queued', 'running')",
    )
    .bind(id.as_uuid())
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    get_check(pool, id).await
}

/// One check.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn get_check(
    pool: &PgPool,
    id: IntegrityCheckId,
) -> Result<Option<IntegrityCheckRecord>, DbError> {
    let row = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM integrity_checks WHERE id = $1"
    ))
    .bind(id.as_uuid())
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    row.as_ref().map(from_row).transpose()
}

/// Checks, newest first, after `before` (a check id, for the next page).
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn list_checks(
    pool: &PgPool,
    before: Option<uuid::Uuid>,
    limit: i64,
) -> Result<Vec<IntegrityCheckRecord>, DbError> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM integrity_checks
         WHERE $1::uuid IS NULL OR id < $1
         ORDER BY id DESC LIMIT $2"
    ))
    .bind(before)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter().map(from_row).collect()
}

/// What the last check covering each artifact found, for those of `ids` the
/// catalogue has: `(integrity_state, integrity_checked_at)` by artifact.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn integrity_of(
    pool: &PgPool,
    ids: &[uuid::Uuid],
) -> Result<std::collections::HashMap<uuid::Uuid, (String, Option<OffsetDateTime>)>, DbError> {
    let rows = sqlx::query(
        "SELECT id, integrity_state, integrity_checked_at FROM artifacts WHERE id = ANY($1)",
    )
    .bind(ids)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter()
        .map(|row| {
            Ok((
                row.try_get("id").map_err(DbError::Query)?,
                (
                    row.try_get("integrity_state").map_err(DbError::Query)?,
                    row.try_get("integrity_checked_at")
                        .map_err(DbError::Query)?,
                ),
            ))
        })
        .collect()
}
