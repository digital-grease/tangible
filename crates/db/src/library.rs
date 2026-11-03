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
}

impl ArtifactFilter<'_> {
    /// Whether it keeps everything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
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
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)
}
