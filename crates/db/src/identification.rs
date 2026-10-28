// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Preservation hashes as the catalogue keeps them
//! (docs/plans/dat-matching.md H1): which artifacts still need them, a lease
//! on the one being hashed, and the CRC32, MD5 and SHA-1 of each component
//! (`artifact_hashes`) and each CD track (`artifact_track_hashes`).

use sqlx::{PgPool, Row};
use time::OffsetDateTime;

use crate::DbError;

/// One set of preservation hashes, lowercase hex.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hashes<'a> {
    /// CRC32, eight hex digits.
    pub crc32: &'a str,
    /// MD5.
    pub md5: &'a str,
    /// SHA-1.
    pub sha1: &'a str,
    /// Bytes covered.
    pub length: i64,
}

/// Take the next artifact whose hashes are pending, with a lease of
/// `lease_seconds`. Answers its id.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn claim_next_unhashed(
    pool: &PgPool,
    owner: &str,
    lease_seconds: i64,
) -> Result<Option<uuid::Uuid>, DbError> {
    sqlx::query_scalar(
        "UPDATE artifacts SET
             hashes_lease_owner = $1,
             hashes_lease_until = now() + ($2::bigint * interval '1 second')
         WHERE id = (
             SELECT id FROM artifacts
             WHERE hashes_state = 'pending'
               AND (hashes_lease_until IS NULL OR hashes_lease_until < now())
             ORDER BY id
             LIMIT 1
             FOR UPDATE SKIP LOCKED
         )
         RETURNING id",
    )
    .bind(owner)
    .bind(lease_seconds)
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)
}

/// Take `artifact_id` for hashing, with a lease, if it is pending and not
/// held. Answers whether it was taken.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn claim_for_hashing(
    pool: &PgPool,
    artifact_id: uuid::Uuid,
    owner: &str,
    lease_seconds: i64,
) -> Result<bool, DbError> {
    let done = sqlx::query(
        "UPDATE artifacts SET
             hashes_lease_owner = $2,
             hashes_lease_until = now() + ($3::bigint * interval '1 second')
         WHERE id = $1 AND hashes_state = 'pending'
           AND (hashes_lease_until IS NULL OR hashes_lease_until < now())",
    )
    .bind(artifact_id)
    .bind(owner)
    .bind(lease_seconds)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(done.rows_affected() > 0)
}

/// Record an artifact's hashes, replacing any it had, match it against
/// every loaded DAT, and mark it done, all in one transaction, if `owner`
/// still holds it. Answers how many matches it made, or `None` when it no
/// longer held the artifact.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn record_hashes(
    pool: &PgPool,
    artifact_id: uuid::Uuid,
    owner: &str,
    components: &[(uuid::Uuid, Hashes<'_>)],
    tracks: &[(u32, Hashes<'_>)],
) -> Result<Option<u64>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let held = sqlx::query(
        "UPDATE artifacts SET hashes_state = 'done', hashed_at = now(), hashes_error = NULL,
             hashes_lease_owner = NULL, hashes_lease_until = NULL
         WHERE id = $1 AND hashes_lease_owner = $2",
    )
    .bind(artifact_id)
    .bind(owner)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    if held.rows_affected() == 0 {
        return Ok(None);
    }
    sqlx::query(
        "DELETE FROM artifact_hashes
         WHERE artifact_id = $1 AND algorithm IN ('crc32', 'md5', 'sha1')",
    )
    .bind(artifact_id)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    sqlx::query("DELETE FROM artifact_track_hashes WHERE artifact_id = $1")
        .bind(artifact_id)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    for (component, hashes) in components {
        for (algorithm, value) in [
            ("crc32", hashes.crc32),
            ("md5", hashes.md5),
            ("sha1", hashes.sha1),
        ] {
            sqlx::query(
                "INSERT INTO artifact_hashes (id, artifact_id, component_id, scope, algorithm, value)
                 VALUES ($1, $2, $3, 'component', $4, $5)",
            )
            .bind(uuid::Uuid::now_v7())
            .bind(artifact_id)
            .bind(component)
            .bind(algorithm)
            .bind(value)
            .execute(&mut *tx)
            .await
            .map_err(DbError::Query)?;
        }
    }
    for (number, hashes) in tracks {
        sqlx::query(
            "INSERT INTO artifact_track_hashes
                 (artifact_id, track_number, length_bytes, crc32, md5, sha1)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(artifact_id)
        .bind(i32::try_from(*number).unwrap_or(0))
        .bind(hashes.length)
        .bind(hashes.crc32)
        .bind(hashes.md5)
        .bind(hashes.sha1)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    }
    let matches = crate::dats::rematch_in(&mut tx, Some(&[artifact_id]), None).await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(Some(matches))
}

/// Record that hashing an artifact held by `owner` failed: tried again after
/// `retry_seconds` until `max_attempts`, then left failed, saying why.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn fail_hashing(
    pool: &PgPool,
    artifact_id: uuid::Uuid,
    owner: &str,
    error: &str,
    retry_seconds: i64,
    max_attempts: i32,
) -> Result<(), DbError> {
    let error: String = error.chars().take(2000).collect();
    sqlx::query(
        "UPDATE artifacts SET
             hashes_attempts = hashes_attempts + 1,
             hashes_error = $3,
             hashes_state = CASE WHEN hashes_attempts + 1 >= $5 THEN 'failed' ELSE 'pending' END,
             hashes_lease_owner = CASE WHEN hashes_attempts + 1 >= $5 THEN NULL
                                       ELSE 'retry-backoff' END,
             hashes_lease_until = CASE WHEN hashes_attempts + 1 >= $5 THEN NULL
                                       ELSE now() + ($4::bigint * interval '1 second') END
         WHERE id = $1 AND hashes_lease_owner = $2",
    )
    .bind(artifact_id)
    .bind(owner)
    .bind(error)
    .bind(retry_seconds)
    .bind(max_attempts)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}

/// Ask for an artifact's hashes again. Answers whether there is such an
/// artifact.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn request_hashes(
    pool: &PgPool,
    artifact_id: uuid::Uuid,
    actor: &str,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let done = sqlx::query(
        "UPDATE artifacts SET hashes_state = 'pending', hashed_at = NULL, hashes_error = NULL,
             hashes_attempts = 0, hashes_lease_owner = NULL, hashes_lease_until = NULL
         WHERE id = $1",
    )
    .bind(artifact_id)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    if done.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO audit_events (id, actor_type, actor_id, action, target_type, target_id, outcome)
         VALUES ($1, 'user', $2, 'artifact.rehash_requested', 'artifact', $3, 'success')",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(actor)
    .bind(artifact_id.to_string())
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(true)
}

/// A component's hashes, as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentHashes {
    /// The component.
    pub component_id: uuid::Uuid,
    /// Its path in the artifact.
    pub logical_path: String,
    /// Its size.
    pub length_bytes: i64,
    /// CRC32.
    pub crc32: Option<String>,
    /// MD5.
    pub md5: Option<String>,
    /// SHA-1.
    pub sha1: Option<String>,
}

/// A track's hashes, as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackHashes {
    /// The track.
    pub number: i32,
    /// Bytes covered.
    pub length_bytes: i64,
    /// CRC32.
    pub crc32: String,
    /// MD5.
    pub md5: String,
    /// SHA-1.
    pub sha1: String,
}

/// An artifact's preservation hashes and where computing them stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactHashes {
    /// `pending`, `done` or `failed`.
    pub state: String,
    /// Why it last failed.
    pub error: Option<String>,
    /// When they were computed.
    pub hashed_at: Option<OffsetDateTime>,
    /// Each component's, in order.
    pub components: Vec<ComponentHashes>,
    /// Each track's, in order.
    pub tracks: Vec<TrackHashes>,
}

/// An artifact's preservation hashes, or `None` for no such artifact.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn artifact_hashes(
    pool: &PgPool,
    artifact_id: uuid::Uuid,
) -> Result<Option<ArtifactHashes>, DbError> {
    let Some(row) =
        sqlx::query("SELECT hashes_state, hashes_error, hashed_at FROM artifacts WHERE id = $1")
            .bind(artifact_id)
            .fetch_optional(pool)
            .await
            .map_err(DbError::Query)?
    else {
        return Ok(None);
    };
    let get = |error| DbError::Query(error);
    let components = sqlx::query(
        "SELECT c.id, c.logical_path, c.length_bytes,
                max(h.value) FILTER (WHERE h.algorithm = 'crc32') AS crc32,
                max(h.value) FILTER (WHERE h.algorithm = 'md5') AS md5,
                max(h.value) FILTER (WHERE h.algorithm = 'sha1') AS sha1
         FROM artifact_components c
         LEFT JOIN artifact_hashes h ON h.component_id = c.id
         WHERE c.artifact_id = $1
         GROUP BY c.id, c.logical_path, c.length_bytes, c.ordinal
         ORDER BY c.ordinal, c.logical_path",
    )
    .bind(artifact_id)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?
    .iter()
    .map(|row| {
        Ok(ComponentHashes {
            component_id: row.try_get("id").map_err(get)?,
            logical_path: row.try_get("logical_path").map_err(get)?,
            length_bytes: row.try_get("length_bytes").map_err(get)?,
            crc32: row.try_get("crc32").map_err(get)?,
            md5: row.try_get("md5").map_err(get)?,
            sha1: row.try_get("sha1").map_err(get)?,
        })
    })
    .collect::<Result<Vec<_>, DbError>>()?;
    let tracks = sqlx::query(
        "SELECT track_number, length_bytes, crc32, md5, sha1 FROM artifact_track_hashes
         WHERE artifact_id = $1 ORDER BY track_number",
    )
    .bind(artifact_id)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?
    .iter()
    .map(|row| {
        Ok(TrackHashes {
            number: row.try_get("track_number").map_err(get)?,
            length_bytes: row.try_get("length_bytes").map_err(get)?,
            crc32: row.try_get("crc32").map_err(get)?,
            md5: row.try_get("md5").map_err(get)?,
            sha1: row.try_get("sha1").map_err(get)?,
        })
    })
    .collect::<Result<Vec<_>, DbError>>()?;
    Ok(Some(ArtifactHashes {
        state: row.try_get("hashes_state").map_err(get)?,
        error: row.try_get("hashes_error").map_err(get)?,
        hashed_at: row.try_get("hashed_at").map_err(get)?,
        components,
        tracks,
    }))
}
