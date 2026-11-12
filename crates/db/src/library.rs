// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Which artifacts a filtered library list shows (docs/plans/library-filters.md).
//!
//! Answered from the catalogue, which is written whenever a manifest is
//! published, so a filter for something rare does not read every manifest.
//! Only identifiers come back; the summaries are read from the manifests,
//! as for an unfiltered list.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::DbError;

/// What to keep. Each set field must match; `None` keeps everything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArtifactFilter<'a> {
    /// Its format, such as `cue_bin`.
    pub format: Option<&'a str>,
    /// Where it came from, such as `derived`.
    pub origin: Option<&'a str>,
    /// Its validation state.
    pub validation_state: Option<&'a str>,
    /// Its integrity state.
    pub integrity_state: Option<&'a str>,
    /// Linked to a disc (`true`), or to none (`false`).
    pub linked: Option<bool>,
    /// Catalogued at or after this.
    pub created_after: Option<OffsetDateTime>,
    /// Catalogued before this.
    pub created_before: Option<OffsetDateTime>,
    /// Deleted artifacts only (`true`); otherwise none (ADR 0020).
    pub deleted: Option<bool>,
    /// Part of the name of one of its files, as received or as stored, in
    /// any case. Matched literally: `%` and `_` are not wildcards.
    pub name: Option<&'a str>,
}

/// A name filter as a `LIKE` pattern matching it anywhere, with the
/// pattern's own special characters escaped so they match themselves.
fn contains_pattern(name: &str) -> String {
    let mut pattern = String::with_capacity(name.len() + 2);
    pattern.push('%');
    for character in name.chars() {
        if matches!(character, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    pattern.push('%');
    pattern
}

impl ArtifactFilter<'_> {
    /// Whether it keeps everything but the deleted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        Self {
            deleted: None,
            ..*self
        } == Self::default()
            && self.deleted != Some(true)
    }
}

/// The identifiers a filter keeps, in identifier order (import order),
/// after `after`, at most `limit`.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn filtered_artifact_ids(
    pool: &PgPool,
    filter: ArtifactFilter<'_>,
    after: Option<uuid::Uuid>,
    limit: i64,
) -> Result<Vec<uuid::Uuid>, DbError> {
    sqlx::query_scalar(
        "SELECT a.id FROM artifacts a
         WHERE ($1::uuid IS NULL OR a.id > $1)
           AND ($2::text IS NULL OR a.format = $2)
           AND ($3::text IS NULL OR a.origin = $3)
           AND ($4::text IS NULL OR a.validation_state = $4)
           AND ($5::text IS NULL OR a.integrity_state = $5)
           AND ($6::boolean IS NULL
                OR $6 = EXISTS (SELECT 1 FROM artifact_disc_links l WHERE l.artifact_id = a.id))
           AND ($7::timestamptz IS NULL OR a.created_at >= $7)
           AND ($8::timestamptz IS NULL OR a.created_at < $8)
           AND (a.deleted_at IS NOT NULL) = $10
           AND ($11::text IS NULL OR EXISTS (
                 SELECT 1 FROM artifact_components c
                 WHERE c.artifact_id = a.id
                   AND (c.logical_path ILIKE $11 ESCAPE '\\'
                        OR c.source_filename ILIKE $11 ESCAPE '\\')))
         ORDER BY a.id
         LIMIT $9",
    )
    .bind(after)
    .bind(filter.format)
    .bind(filter.origin)
    .bind(filter.validation_state)
    .bind(filter.integrity_state)
    .bind(filter.linked)
    .bind(filter.created_after)
    .bind(filter.created_before)
    .bind(limit)
    .bind(filter.deleted.unwrap_or(false))
    .bind(filter.name.map(contains_pattern))
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)
}

/// Every deleted artifact: what an unfiltered list, read from the
/// manifests, leaves out (ADR 0020).
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn deleted_artifact_ids(
    pool: &PgPool,
) -> Result<std::collections::HashSet<uuid::Uuid>, DbError> {
    let ids: Vec<uuid::Uuid> =
        sqlx::query_scalar("SELECT id FROM artifacts WHERE deleted_at IS NOT NULL")
            .fetch_all(pool)
            .await
            .map_err(DbError::Query)?;
    Ok(ids.into_iter().collect())
}

/// An artifact's deletion, as a list shows it: asked for, and deleted when
/// it has been.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletionRecord {
    /// When it was asked for.
    pub requested_at: OffsetDateTime,
    /// Who asked.
    pub requested_by: String,
    /// When the bytes may go, while pending.
    pub delete_after: Option<OffsetDateTime>,
    /// When it was deleted.
    pub deleted_at: Option<OffsetDateTime>,
}

/// An artifact's deletion columns, as read: id, asked at, by, delete after,
/// deleted at, pending.
type DeletionRow = (
    uuid::Uuid,
    OffsetDateTime,
    String,
    Option<OffsetDateTime>,
    Option<OffsetDateTime>,
    bool,
);

/// The deletions of these artifacts that have been asked for.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn deletions_of(
    pool: &PgPool,
    ids: &[uuid::Uuid],
) -> Result<std::collections::HashMap<uuid::Uuid, DeletionRecord>, DbError> {
    let rows: Vec<DeletionRow> = sqlx::query_as(
        "SELECT id, deletion_requested_at, deletion_requested_by, delete_after, deleted_at,
                    pending_deletion
             FROM artifacts
             WHERE id = ANY($1) AND deletion_requested_at IS NOT NULL
               AND deletion_requested_by IS NOT NULL",
    )
    .bind(ids)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(rows
        .into_iter()
        .map(
            |(id, requested_at, requested_by, delete_after, deleted_at, pending)| {
                (
                    id,
                    DeletionRecord {
                        requested_at,
                        requested_by,
                        delete_after: delete_after.filter(|_| pending),
                        deleted_at,
                    },
                )
            },
        )
        .collect())
}

#[cfg(test)]
mod tests {
    use super::contains_pattern;

    #[test]
    fn a_name_matches_anywhere_and_only_literally() {
        assert_eq!(contains_pattern("vol2"), "%vol2%");
        // LIKE's own characters, and the escape itself, match only themselves.
        assert_eq!(contains_pattern("100%_done"), "%100\\%\\_done%");
        assert_eq!(contains_pattern("a\\b"), "%a\\\\b%");
        // Any character is otherwise passed as it is.
        assert_eq!(contains_pattern("Ré:v"), "%Ré:v%");
    }
}
