// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! What diagnostics reads from the database: the workers, and recent
//! failures by code.
//!
//! Nothing here returns a name from the library, a file name, or a failure's
//! detail text. Diagnostics are meant to be pasted into an issue, and a
//! code with a time says enough to start from.

use serde_json::Value;
use sqlx::{PgPool, Row};
use tangible_domain::WorkerId;
use time::OffsetDateTime;

use crate::DbError;

/// A worker, as diagnostics shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerDiagnostic {
    /// The worker.
    pub id: WorkerId,
    /// `pending`, `online`, `offline`, `draining`, `revoked` or `incompatible`.
    pub status: String,
    /// The software version it last reported.
    pub software_version: String,
    /// The protocol version it enrolled with.
    pub protocol_version: String,
    /// When it was last heard from.
    pub last_seen_at: Option<OffsetDateTime>,
    /// The engines it reported, as `[{"name", "version"}]`.
    pub engines: Value,
}

/// Every worker, oldest first, so the labels diagnostics gives them stay put.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn list_workers(pool: &PgPool) -> Result<Vec<WorkerDiagnostic>, DbError> {
    let rows = sqlx::query(
        "SELECT id, status, software_version, protocol_version, last_seen_at,
                COALESCE(capabilities_json -> 'engines', '[]'::jsonb) AS engines
         FROM workers ORDER BY created_at, id",
    )
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter()
        .map(|row| {
            let get = DbError::Query;
            Ok(WorkerDiagnostic {
                id: WorkerId::from_uuid(row.try_get("id").map_err(get)?),
                status: row.try_get("status").map_err(get)?,
                software_version: row.try_get("software_version").map_err(get)?,
                protocol_version: row.try_get("protocol_version").map_err(get)?,
                last_seen_at: row.try_get("last_seen_at").map_err(get)?,
                engines: row.try_get("engines").map_err(get)?,
            })
        })
        .collect()
}

/// What kind of work failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// A burn attempt.
    Burn,
    /// An import.
    Import,
    /// A derivation.
    Derivation,
}

impl FailureKind {
    /// The word for it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Burn => "burn",
            Self::Import => "import",
            Self::Derivation => "derivation",
        }
    }
}

/// One failure: what failed, its code, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// What failed.
    pub kind: FailureKind,
    /// Its stable code, such as `ENGINE_FAILED`.
    pub code: String,
    /// When it was recorded.
    pub at: OffsetDateTime,
}

/// The `limit` most recent failures across burns, imports and derivations,
/// newest first.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn recent_failures(pool: &PgPool, limit: i64) -> Result<Vec<Failure>, DbError> {
    let rows = sqlx::query(
        "SELECT kind, code, at FROM (
             SELECT 'burn' AS kind, error_code AS code, COALESCE(ended_at, started_at) AS at
             FROM burn_attempts WHERE error_code IS NOT NULL
             UNION ALL
             SELECT 'import', error_code, updated_at
             FROM import_jobs WHERE error_code IS NOT NULL
             UNION ALL
             SELECT 'derivation', error_code, updated_at
             FROM derivation_jobs WHERE error_code IS NOT NULL
         ) failures
         ORDER BY at DESC, kind, code
         LIMIT $1",
    )
    .bind(limit.max(0))
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter()
        .map(|row| {
            let get = DbError::Query;
            let kind: String = row.try_get("kind").map_err(get)?;
            Ok(Failure {
                kind: match kind.as_str() {
                    "burn" => FailureKind::Burn,
                    "import" => FailureKind::Import,
                    _ => FailureKind::Derivation,
                },
                code: row.try_get("code").map_err(get)?,
                at: row.try_get("at").map_err(get)?,
            })
        })
        .collect()
}
