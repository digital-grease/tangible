// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Cover art (ADR 0023): the covers wanted, fetched and kept.
//!
//! A row is one cover a provider keeps, named by the provider and its own id
//! for the item. The artwork runner adds rows for what is wanted, fetches
//! those due, and records how each went; the API reads ready ones.

use std::collections::HashMap;

use sqlx::{PgPool, Row as _};
use time::Duration;

use crate::DbError;

/// A cover to fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueArtwork {
    /// The row.
    pub id: uuid::Uuid,
    /// `tmdb`, `imdb`, `musicbrainz`, `igdb` or `libretro`.
    pub source: String,
    /// The provider's id for the item.
    pub external_id: String,
    /// How many times it has been tried.
    pub attempts: i32,
}

/// A cover ready to show: its row, and its digest, which names its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyArtwork {
    /// The row.
    pub id: uuid::Uuid,
    /// The provider it came from.
    pub source: String,
    /// The SHA-256 of its bytes, in hex.
    pub sha256: String,
}

/// A cover's bytes, as kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtworkImage {
    /// `image/jpeg`, `image/png` or `image/webp`, as its bytes say.
    pub content_type: String,
    /// The bytes.
    pub bytes: Vec<u8>,
    /// Their SHA-256, in hex.
    pub sha256: String,
}

/// Want the covers `keys` (source, external id): a pending row for each
/// not already there. Answers how many were added.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn want_artwork(pool: &PgPool, keys: &[(String, String)]) -> Result<u64, DbError> {
    if keys.is_empty() {
        return Ok(0);
    }
    let (sources, ids): (Vec<&str>, Vec<&str>) = keys
        .iter()
        .map(|(source, id)| (source.as_str(), id.as_str()))
        .unzip();
    let ids_for_rows: Vec<uuid::Uuid> = keys.iter().map(|_| uuid::Uuid::now_v7()).collect();
    let added = sqlx::query(
        "INSERT INTO artwork (id, source, external_id)
         SELECT * FROM unnest($1::uuid[], $2::text[], $3::text[])
         ON CONFLICT (source, external_id) DO NOTHING",
    )
    .bind(&ids_for_rows)
    .bind(&sources)
    .bind(&ids)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(added.rows_affected())
}

/// Up to `limit` covers due a fetch: not ready, their time come, the
/// longest waiting first.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn due_artwork(pool: &PgPool, limit: i64) -> Result<Vec<DueArtwork>, DbError> {
    let rows = sqlx::query(
        "SELECT id, source, external_id, attempts FROM artwork
         WHERE state <> 'ready' AND next_attempt_at <= now()
         ORDER BY next_attempt_at, id
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter()
        .map(|row| {
            Ok(DueArtwork {
                id: row.try_get("id").map_err(DbError::Query)?,
                source: row.try_get("source").map_err(DbError::Query)?,
                external_id: row.try_get("external_id").map_err(DbError::Query)?,
                attempts: row.try_get("attempts").map_err(DbError::Query)?,
            })
        })
        .collect()
}

/// Keep the cover fetched for `id`.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure, including bytes the table
/// refuses (over 2 MiB, or a type it does not take).
pub async fn artwork_ready(
    pool: &PgPool,
    id: uuid::Uuid,
    content_type: &str,
    bytes: &[u8],
    sha256: &str,
) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE artwork
         SET state = 'ready', content_type = $2, bytes = $3, sha256 = $4,
             attempts = attempts + 1, error = NULL, fetched_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(content_type)
    .bind(bytes)
    .bind(sha256)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}

/// Record that `id` was not fetched: `missing` (the provider has none) or
/// `failed`, saying why, and when to try again.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn artwork_not_fetched(
    pool: &PgPool,
    id: uuid::Uuid,
    missing: bool,
    error: &str,
    again_after: Duration,
) -> Result<(), DbError> {
    let error: String = error.chars().take(500).collect();
    sqlx::query(
        "UPDATE artwork
         SET state = CASE WHEN $2 THEN 'missing' ELSE 'failed' END,
             content_type = NULL, bytes = NULL, sha256 = NULL,
             attempts = attempts + 1, error = $3,
             next_attempt_at = now() + make_interval(secs => $4)
         WHERE id = $1",
    )
    .bind(id)
    .bind(missing)
    .bind(error)
    .bind(again_after.as_seconds_f64())
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}

/// The cover `id`'s bytes, when it is ready.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn artwork_image(pool: &PgPool, id: uuid::Uuid) -> Result<Option<ArtworkImage>, DbError> {
    let row = sqlx::query(
        "SELECT content_type, bytes, sha256 FROM artwork WHERE id = $1 AND state = 'ready'",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    row.map(|row| {
        Ok(ArtworkImage {
            content_type: row.try_get("content_type").map_err(DbError::Query)?,
            bytes: row.try_get("bytes").map_err(DbError::Query)?,
            sha256: row.try_get("sha256").map_err(DbError::Query)?,
        })
    })
    .transpose()
}

/// The ready covers among `keys` (source, external id), by key.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn ready_artwork(
    pool: &PgPool,
    keys: &[(String, String)],
) -> Result<HashMap<(String, String), ReadyArtwork>, DbError> {
    if keys.is_empty() {
        return Ok(HashMap::new());
    }
    let (sources, ids): (Vec<&str>, Vec<&str>) = keys
        .iter()
        .map(|(source, id)| (source.as_str(), id.as_str()))
        .unzip();
    let rows = sqlx::query(
        "SELECT a.id, a.source, a.external_id, a.sha256
         FROM artwork a
         JOIN unnest($1::text[], $2::text[]) AS wanted (source, external_id)
           ON wanted.source = a.source AND wanted.external_id = a.external_id
         WHERE a.state = 'ready'",
    )
    .bind(&sources)
    .bind(&ids)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter()
        .map(|row| {
            let source: String = row.try_get("source").map_err(DbError::Query)?;
            let external_id: String = row.try_get("external_id").map_err(DbError::Query)?;
            Ok((
                (source.clone(), external_id),
                ReadyArtwork {
                    id: row.try_get("id").map_err(DbError::Query)?,
                    source,
                    sha256: row.try_get("sha256").map_err(DbError::Query)?,
                },
            ))
        })
        .collect()
}

/// Every request's references, for the covers they want: each request's
/// `refs` as stored.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn requests_references(pool: &PgPool) -> Result<Vec<serde_json::Value>, DbError> {
    sqlx::query_scalar(
        "SELECT refs FROM requests
         WHERE state NOT IN ('declined', 'canceled') AND jsonb_array_length(refs) > 0",
    )
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)
}

