// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Stored collection plans (ADR 0024, docs/plans/gc-sweep.md G2): a plan
//! kept with every candidate, collected at most once, and kept after with
//! what became of each candidate.

use serde_json::Value;
use sqlx::{PgPool, Postgres, Row as _, Transaction};
use time::OffsetDateTime;

use crate::DbError;

/// Import states that ended without registering what they stored: their
/// checkpoints are not marks, but still say where an orphan came from.
const ORPHANING_IMPORT_STATES: &str = "('failed_terminal', 'canceled')";

/// A plan, as it is made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPlan {
    /// Its identifier.
    pub id: uuid::Uuid,
    /// The administrator who made it, or `None` for the schedule.
    pub planned_by: Option<String>,
    /// How young an unreferenced object was kept anyway, in seconds.
    pub grace_seconds: i64,
    /// Whether every manifest and checkpoint could be read.
    pub certain: bool,
    /// How many candidates, and their bytes.
    pub candidate_count: i64,
    /// Their bytes.
    pub candidate_bytes: i64,
    /// Partial files in the object tree older than the grace period.
    pub stale_partial_files: i64,
}

/// One candidate, as it is planned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPlanObject {
    /// Its digest, lowercase hex.
    pub sha256: String,
    /// Its length when planned.
    pub size_bytes: i64,
    /// When its file was last written, when planned.
    pub modified_at: OffsetDateTime,
    /// Whether the catalogue recorded it.
    pub recorded: bool,
    /// The ended import whose checkpoint lists it, if one does.
    pub import_job_id: Option<uuid::Uuid>,
}

/// A stored plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRow {
    /// Its identifier.
    pub id: uuid::Uuid,
    /// When it was made.
    pub created_at: OffsetDateTime,
    /// The administrator who made it, or `None` for the schedule.
    pub planned_by: Option<String>,
    /// The grace period it was made with, in seconds.
    pub grace_seconds: i64,
    /// Whether every mark could be read.
    pub certain: bool,
    /// How many candidates.
    pub candidate_count: i64,
    /// Their bytes.
    pub candidate_bytes: i64,
    /// Stale partial files when planned.
    pub stale_partial_files: i64,
    /// When collecting it was asked for.
    pub collect_requested_at: Option<OffsetDateTime>,
    /// Who asked, or `None` for the schedule.
    pub collect_requested_by: Option<String>,
    /// What collecting it did, once it has.
    pub collected: Option<Collected>,
}

/// What collecting a plan did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Collected {
    /// When it finished.
    pub at: OffsetDateTime,
    /// Candidates removed.
    pub removed_count: i64,
    /// Their bytes.
    pub removed_bytes: i64,
    /// Candidates kept.
    pub kept_count: i64,
    /// Their bytes, as planned.
    pub kept_bytes: i64,
    /// Stale partial files removed.
    pub partial_files_removed: i64,
}

/// One candidate of a stored plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanObjectRow {
    /// Its digest.
    pub sha256: String,
    /// Its length when planned.
    pub size_bytes: i64,
    /// When its file was last written, when planned.
    pub modified_at: OffsetDateTime,
    /// Whether the catalogue recorded it.
    pub recorded: bool,
    /// What collecting did with it, once it has.
    pub outcome: Option<String>,
    /// The ended import it came from, when the catalogue can say.
    pub import: Option<ImportOrigin>,
}

/// The ended import an orphan came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportOrigin {
    /// The import.
    pub id: uuid::Uuid,
    /// Its state: `failed_terminal` or `canceled`.
    pub state: String,
    /// Its failure code, when it failed.
    pub error_code: Option<String>,
    /// What it brought in, as its source named it.
    pub name: Option<String>,
    /// When it last changed, which for an ended import is when it ended.
    pub ended_at: OffsetDateTime,
}

/// Why collecting a plan was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// No such plan.
    NotFound,
    /// Not every mark could be read when it was made.
    Uncertain,
    /// Made too long ago.
    Expired,
    /// Made with a grace period under the least collection accepts.
    GraceTooShort,
    /// Collecting it was asked for already.
    AlreadyRequested,
}

/// Store a plan and its candidates, in one transaction.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn insert_plan(
    pool: &PgPool,
    plan: &NewPlan,
    objects: &[NewPlanObject],
) -> Result<(), DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    sqlx::query(
        "INSERT INTO gc_plans
             (id, planned_by, grace_seconds, certain, candidate_count, candidate_bytes,
              stale_partial_files)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(plan.id)
    .bind(plan.planned_by.as_deref())
    .bind(plan.grace_seconds)
    .bind(plan.certain)
    .bind(plan.candidate_count)
    .bind(plan.candidate_bytes)
    .bind(plan.stale_partial_files)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    // In batches, so a plan of a great many candidates is not one statement
    // with a great many parameters' worth of arrays.
    for batch in objects.chunks(5_000) {
        let sha256: Vec<&str> = batch.iter().map(|o| o.sha256.as_str()).collect();
        let size: Vec<i64> = batch.iter().map(|o| o.size_bytes).collect();
        let modified: Vec<OffsetDateTime> = batch.iter().map(|o| o.modified_at).collect();
        let recorded: Vec<bool> = batch.iter().map(|o| o.recorded).collect();
        let import: Vec<Option<uuid::Uuid>> = batch.iter().map(|o| o.import_job_id).collect();
        sqlx::query(
            "INSERT INTO gc_plan_objects
                 (plan_id, sha256, size_bytes, modified_at, recorded, import_job_id)
             SELECT $1, * FROM UNNEST($2::text[], $3::bigint[], $4::timestamptz[],
                                      $5::boolean[], $6::uuid[])",
        )
        .bind(plan.id)
        .bind(&sha256)
        .bind(&size)
        .bind(&modified)
        .bind(&recorded)
        .bind(&import)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    }
    tx.commit().await.map_err(DbError::Query)
}

const PLAN_COLUMNS: &str = "id, created_at, planned_by, grace_seconds, certain, candidate_count,
     candidate_bytes, stale_partial_files, collect_requested_at, collect_requested_by,
     collected_at, removed_count, removed_bytes, kept_count, kept_bytes, partial_files_removed";

fn plan_row(row: &sqlx::postgres::PgRow) -> Result<PlanRow, DbError> {
    let get = DbError::Query;
    let collected_at: Option<OffsetDateTime> = row.try_get("collected_at").map_err(get)?;
    let collected = match collected_at {
        Some(at) => Some(Collected {
            at,
            removed_count: row.try_get("removed_count").map_err(get)?,
            removed_bytes: row.try_get("removed_bytes").map_err(get)?,
            kept_count: row.try_get("kept_count").map_err(get)?,
            kept_bytes: row.try_get("kept_bytes").map_err(get)?,
            partial_files_removed: row.try_get("partial_files_removed").map_err(get)?,
        }),
        None => None,
    };
    Ok(PlanRow {
        id: row.try_get("id").map_err(get)?,
        created_at: row.try_get("created_at").map_err(get)?,
        planned_by: row.try_get("planned_by").map_err(get)?,
        grace_seconds: row.try_get("grace_seconds").map_err(get)?,
        certain: row.try_get("certain").map_err(get)?,
        candidate_count: row.try_get("candidate_count").map_err(get)?,
        candidate_bytes: row.try_get("candidate_bytes").map_err(get)?,
        stale_partial_files: row.try_get("stale_partial_files").map_err(get)?,
        collect_requested_at: row.try_get("collect_requested_at").map_err(get)?,
        collect_requested_by: row.try_get("collect_requested_by").map_err(get)?,
        collected,
    })
}

/// A stored plan, or `None`.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn get_plan(pool: &PgPool, id: uuid::Uuid) -> Result<Option<PlanRow>, DbError> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {PLAN_COLUMNS} FROM gc_plans WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?
    .as_ref()
    .map(plan_row)
    .transpose()
}

/// The most recent plans, newest first.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn recent_plans(pool: &PgPool, limit: i64) -> Result<Vec<PlanRow>, DbError> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {PLAN_COLUMNS} FROM gc_plans ORDER BY created_at DESC, id DESC LIMIT $1"
    )))
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?
    .iter()
    .map(plan_row)
    .collect()
}

/// A plan's candidates, largest first, at most `limit`, with the import
/// each came from.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn plan_objects(
    pool: &PgPool,
    plan_id: uuid::Uuid,
    limit: i64,
) -> Result<Vec<PlanObjectRow>, DbError> {
    let get = DbError::Query;
    sqlx::query(
        "SELECT o.sha256, o.size_bytes, o.modified_at, o.recorded, o.outcome,
                j.id AS import_id, j.state AS import_state, j.error_code,
                COALESCE(j.source_descriptor_json ->> 'name',
                         j.source_descriptor_json ->> 'filename') AS import_name,
                j.updated_at AS import_ended_at
         FROM gc_plan_objects o LEFT JOIN import_jobs j ON j.id = o.import_job_id
         WHERE o.plan_id = $1
         ORDER BY o.size_bytes DESC, o.sha256
         LIMIT $2",
    )
    .bind(plan_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(get)?
    .iter()
    .map(|row| {
        let import_id: Option<uuid::Uuid> = row.try_get("import_id").map_err(get)?;
        let import = match import_id {
            Some(id) => Some(ImportOrigin {
                id,
                state: row.try_get("import_state").map_err(get)?,
                error_code: row.try_get("error_code").map_err(get)?,
                name: row.try_get("import_name").map_err(get)?,
                ended_at: row.try_get("import_ended_at").map_err(get)?,
            }),
            None => None,
        };
        Ok(PlanObjectRow {
            sha256: row.try_get("sha256").map_err(get)?,
            size_bytes: row.try_get("size_bytes").map_err(get)?,
            modified_at: row.try_get("modified_at").map_err(get)?,
            recorded: row.try_get("recorded").map_err(get)?,
            outcome: row.try_get("outcome").map_err(get)?,
            import,
        })
    })
    .collect()
}

/// Every candidate of a plan, for collecting it: digest, size and modified
/// time as planned, and whether the catalogue recorded it.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn plan_candidates(
    pool: &PgPool,
    plan_id: uuid::Uuid,
) -> Result<Vec<(String, i64, OffsetDateTime, bool)>, DbError> {
    sqlx::query_as(
        "SELECT sha256, size_bytes, modified_at, recorded FROM gc_plan_objects
         WHERE plan_id = $1 ORDER BY sha256",
    )
    .bind(plan_id)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)
}

/// Ask for a plan to be collected: by `by`, or by the schedule when `None`.
/// Refused for a plan that was not certain, one made more than `max_age`
/// ago, one made with a grace period under `min_grace_seconds`, or one
/// asked for already.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn request_collection(
    pool: &PgPool,
    id: uuid::Uuid,
    by: Option<&str>,
    max_age: time::Duration,
    min_grace_seconds: i64,
) -> Result<Result<PlanRow, Refusal>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {PLAN_COLUMNS}, now() AS now FROM gc_plans WHERE id = $1 FOR UPDATE"
    )))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let Some(row) = row else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(Refusal::NotFound));
    };
    let now: OffsetDateTime = row.try_get("now").map_err(DbError::Query)?;
    let plan = plan_row(&row)?;
    let refusal = if plan.collect_requested_at.is_some() {
        Some(Refusal::AlreadyRequested)
    } else if !plan.certain {
        Some(Refusal::Uncertain)
    } else if plan.grace_seconds < min_grace_seconds {
        Some(Refusal::GraceTooShort)
    } else if now - plan.created_at > max_age {
        Some(Refusal::Expired)
    } else {
        None
    };
    if let Some(refusal) = refusal {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(refusal));
    }
    sqlx::query(
        "UPDATE gc_plans SET collect_requested_at = now(), collect_requested_by = $2
         WHERE id = $1",
    )
    .bind(id)
    .bind(by)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    if let Some(by) = by {
        crate::accounts::audit(
            &mut tx,
            by,
            "storage.orphan_collection_requested",
            &id.to_string(),
            serde_json::json!({
                "candidates": plan.candidate_count,
                "bytes": plan.candidate_bytes,
            }),
        )
        .await?;
    }
    tx.commit().await.map_err(DbError::Query)?;
    get_plan(pool, id)
        .await?
        .map(Ok)
        .ok_or(DbError::Query(sqlx::Error::RowNotFound))
}

/// Plans asked to be collected and not yet collected, oldest request
/// first.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn queued_plans(pool: &PgPool) -> Result<Vec<uuid::Uuid>, DbError> {
    sqlx::query_scalar(
        "SELECT id FROM gc_plans
         WHERE collect_requested_at IS NOT NULL AND collected_at IS NULL
         ORDER BY collect_requested_at, id",
    )
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)
}

/// Where orphan collection stands, for the schedule and for diagnostics
/// (ADR 0024, decision 6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CollectionSummary {
    /// The last plan collected, by hand or by schedule, and what it did.
    pub last_collected: Option<Collected>,
    /// The last plan the schedule made: when, and whether it could read
    /// every mark.
    pub last_scheduled: Option<(OffsetDateTime, bool)>,
    /// Plans asked to be collected and not collected yet.
    pub queued: i64,
}

/// Where orphan collection stands.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn collection_summary(pool: &PgPool) -> Result<CollectionSummary, DbError> {
    let last = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {PLAN_COLUMNS} FROM gc_plans WHERE collected_at IS NOT NULL
         ORDER BY collected_at DESC, id DESC LIMIT 1"
    )))
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    let last_collected = match last {
        Some(row) => plan_row(&row)?.collected,
        None => None,
    };
    let last_scheduled: Option<(OffsetDateTime, bool)> = sqlx::query_as(
        "SELECT created_at, certain FROM gc_plans WHERE planned_by IS NULL
         ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gc_plans
         WHERE collect_requested_at IS NOT NULL AND collected_at IS NULL",
    )
    .fetch_one(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(CollectionSummary {
        last_collected,
        last_scheduled,
        queued,
    })
}

/// What became of one candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Its file was removed.
    Removed,
    /// The marks, read again, found something referring to it.
    KeptLive,
    /// The catalogue counts it now, or it has a retention lock.
    KeptHeld,
    /// Its file is not what was planned: another size, or written since.
    KeptChanged,
    /// Its file was gone already.
    KeptGone,
    /// Not every mark could be read again, so nothing was removed.
    KeptUncertain,
    /// Removing its file failed.
    KeptFailed,
}

impl Outcome {
    /// The stored value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Removed => "removed",
            Self::KeptLive => "kept_live",
            Self::KeptHeld => "kept_held",
            Self::KeptChanged => "kept_changed",
            Self::KeptGone => "kept_gone",
            Self::KeptUncertain => "kept_uncertain",
            Self::KeptFailed => "kept_failed",
        }
    }
}

/// Record what collecting a plan did, every candidate's outcome and the
/// totals, and audit it as `storage.orphans_removed`; all in one
/// transaction, once. `false` if the plan was collected already, when
/// nothing is recorded.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn record_collection(
    pool: &PgPool,
    id: uuid::Uuid,
    outcomes: &[(String, Outcome)],
    totals: Collected,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let requested_by: Option<Option<String>> = sqlx::query_scalar(
        "SELECT collect_requested_by FROM gc_plans
         WHERE id = $1 AND collect_requested_at IS NOT NULL AND collected_at IS NULL
         FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let Some(requested_by) = requested_by else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(false);
    };
    for batch in outcomes.chunks(5_000) {
        let sha256: Vec<&str> = batch.iter().map(|(s, _)| s.as_str()).collect();
        let outcome: Vec<&str> = batch.iter().map(|(_, o)| o.as_str()).collect();
        sqlx::query(
            "UPDATE gc_plan_objects o SET outcome = u.outcome
             FROM UNNEST($2::text[], $3::text[]) AS u (sha256, outcome)
             WHERE o.plan_id = $1 AND o.sha256 = u.sha256",
        )
        .bind(id)
        .bind(&sha256)
        .bind(&outcome)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    }
    sqlx::query(
        "UPDATE gc_plans
         SET collected_at = $2, removed_count = $3, removed_bytes = $4, kept_count = $5,
             kept_bytes = $6, partial_files_removed = $7
         WHERE id = $1",
    )
    .bind(id)
    .bind(totals.at)
    .bind(totals.removed_count)
    .bind(totals.removed_bytes)
    .bind(totals.kept_count)
    .bind(totals.kept_bytes)
    .bind(totals.partial_files_removed)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let (actor_type, actor_id) = match &requested_by {
        Some(user) => ("user", user.as_str()),
        None => ("system", "schedule"),
    };
    sqlx::query(
        "INSERT INTO audit_events
             (id, actor_type, actor_id, action, target_type, target_id, outcome, metadata)
         VALUES ($1, $2, $3, 'storage.orphans_removed', 'storage', $4, 'success', $5)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(actor_type)
    .bind(actor_id)
    .bind(id.to_string())
    .bind(serde_json::json!({
        "plan_id": id.to_string(),
        "removed": { "count": totals.removed_count, "bytes": totals.removed_bytes },
        "kept": { "count": totals.kept_count, "bytes": totals.kept_bytes },
        "partial_files_removed": totals.partial_files_removed,
    }))
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(true)
}

/// How a candidate's catalogue row stands, under a row lock when it has
/// one.
#[derive(Debug)]
pub enum RowHold {
    /// The catalogue has no row for it: an orphan nothing recorded.
    Unrecorded,
    /// Its row counts it, or locks it: it is to be kept.
    Held,
    /// Its row lets it go, and is locked until the removal is finished.
    Removable(OrphanRemoval),
}

/// One recorded candidate's removal, its row locked until it is finished.
pub struct OrphanRemoval {
    tx: Transaction<'static, Postgres>,
    sha256: String,
}

impl std::fmt::Debug for OrphanRemoval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrphanRemoval")
            .field("sha256", &self.sha256)
            .finish_non_exhaustive()
    }
}

/// Lock a candidate's row, if it has one, and say whether it may go: a row
/// that counts it or holds a retention lock keeps it; one marked removed
/// already (a restore's) does not.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn hold_row(pool: &PgPool, sha256: &str) -> Result<RowHold, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let removable: Option<bool> = sqlx::query_scalar(
        "SELECT reference_count = 0 AND NOT retention_lock
         FROM cas_objects WHERE sha256 = $1 FOR UPDATE",
    )
    .bind(sha256)
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    match removable {
        None => {
            tx.rollback().await.map_err(DbError::Query)?;
            Ok(RowHold::Unrecorded)
        }
        Some(false) => {
            tx.rollback().await.map_err(DbError::Query)?;
            Ok(RowHold::Held)
        }
        Some(true) => Ok(RowHold::Removable(OrphanRemoval {
            tx,
            sha256: sha256.to_owned(),
        })),
    }
}

impl OrphanRemoval {
    /// Record the object as removed, if its row did not say so already.
    ///
    /// # Errors
    ///
    /// [`DbError::Query`] on a database failure.
    pub async fn finish(mut self) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE cas_objects SET removed_at = now()
             WHERE sha256 = $1 AND removed_at IS NULL",
        )
        .bind(&self.sha256)
        .execute(&mut *self.tx)
        .await
        .map_err(DbError::Query)?;
        self.tx.commit().await.map_err(DbError::Query)
    }

    /// Leave the row as it was: the bytes could not be removed.
    ///
    /// # Errors
    ///
    /// [`DbError::Query`] on a database failure.
    pub async fn abandon(self) -> Result<(), DbError> {
        self.tx.rollback().await.map_err(DbError::Query)
    }
}

/// The checkpoints of imports that ended without registering what they
/// stored (failed for good, or canceled), newest first: where an orphan
/// came from, when its checkpoint names it.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn orphaning_import_checkpoints(
    pool: &PgPool,
) -> Result<Vec<(uuid::Uuid, Value)>, DbError> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, checkpoint_json FROM import_jobs
         WHERE checkpoint_json IS NOT NULL AND state IN {ORPHANING_IMPORT_STATES}
         ORDER BY updated_at DESC, id DESC"
    )))
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)
}

/// The staging area of every import that ended without registering what it
/// stored, by id: the evidence it keeps (ADR 0024, decision 7). With its
/// state, failure code, name and when it ended.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn orphaning_imports(
    pool: &PgPool,
    ids: &[uuid::Uuid],
) -> Result<Vec<ImportOrigin>, DbError> {
    let get = DbError::Query;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT id, state, error_code,
                COALESCE(source_descriptor_json ->> 'name',
                         source_descriptor_json ->> 'filename') AS name,
                updated_at
         FROM import_jobs WHERE id = ANY($1) AND state IN {ORPHANING_IMPORT_STATES}"
    )))
    .bind(ids)
    .fetch_all(pool)
    .await
    .map_err(get)?
    .iter()
    .map(|row| {
        Ok(ImportOrigin {
            id: row.try_get("id").map_err(get)?,
            state: row.try_get("state").map_err(get)?,
            error_code: row.try_get("error_code").map_err(get)?,
            name: row.try_get("name").map_err(get)?,
            ended_at: row.try_get("updated_at").map_err(get)?,
        })
    })
    .collect()
}
