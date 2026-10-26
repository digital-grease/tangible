// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! What the catalogue says about stored objects, for the garbage collection
//! planner (docs/plans/storage-integrity.md, phase S2): the database's half
//! of the mark.
//!
//! Read in one snapshot, so an import that finishes while the marks are
//! read is seen either still holding its checkpoint or with its artifact
//! catalogued, never neither.

use std::collections::HashSet;

use serde_json::Value;
use sqlx::{PgPool, Row};
use tangible_domain::Sha256Digest;

use crate::DbError;

/// Import states whose checkpoints still hold objects: everything but an
/// import that ended without needing them. A quarantined import keeps its
/// objects for whoever reviews it.
const ENDED_IMPORT_STATES: &str = "('complete', 'failed_terminal', 'canceled')";

/// The catalogue's marks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogueMarks {
    /// Objects the catalogue holds live: every artifact's components and
    /// primary object, and every object with a positive reference count or a
    /// retention lock.
    pub live: HashSet<Sha256Digest>,
    /// The checkpoints of imports not yet ended, which name the objects
    /// each has promoted. The shape is the import runner's, so it reads
    /// them.
    pub import_checkpoints: Vec<Value>,
    /// Every object the catalogue records, with the size it recorded.
    pub recorded: Vec<(Sha256Digest, i64)>,
}

/// Read the marks.
///
/// # Errors
///
/// [`DbError::Query`] on any failure, or [`DbError::Enum`] for a stored
/// digest that is not one, which the columns' constraints rule out.
pub async fn catalogue_marks(pool: &PgPool) -> Result<CatalogueMarks, DbError> {
    let get = DbError::Query;
    let mut tx = pool.begin().await.map_err(get)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(get)?;

    let live: Vec<String> = sqlx::query_scalar(
        "SELECT sha256 FROM artifact_components
         UNION SELECT primary_sha256 FROM artifacts WHERE primary_sha256 IS NOT NULL
         UNION SELECT sha256 FROM cas_objects WHERE reference_count > 0 OR retention_lock",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(get)?;
    let import_checkpoints: Vec<Value> = sqlx::query_scalar(&format!(
        "SELECT checkpoint_json FROM import_jobs
         WHERE checkpoint_json IS NOT NULL AND state NOT IN {ENDED_IMPORT_STATES}"
    ))
    .fetch_all(&mut *tx)
    .await
    .map_err(get)?;
    let recorded = sqlx::query("SELECT sha256, size_bytes FROM cas_objects")
        .fetch_all(&mut *tx)
        .await
        .map_err(get)?
        .iter()
        .map(|row| {
            let sha256: String = row.try_get("sha256").map_err(get)?;
            Ok((digest(&sha256)?, row.try_get("size_bytes").map_err(get)?))
        })
        .collect::<Result<Vec<_>, DbError>>()?;
    tx.commit().await.map_err(get)?;

    Ok(CatalogueMarks {
        live: live
            .iter()
            .map(|sha256| digest(sha256))
            .collect::<Result<_, _>>()?,
        import_checkpoints,
        recorded,
    })
}

fn digest(value: &str) -> Result<Sha256Digest, DbError> {
    value.parse().map_err(|_| DbError::Enum {
        column: "sha256",
        value: value.to_owned(),
    })
}
