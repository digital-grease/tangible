// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Derivation jobs, and the lineage they leave behind.
//!
//! A job is a request to make a derivative; a row in `derivations` is the
//! record that one was made. Asking for a derivative that already exists, or
//! is already being made, returns that rather than starting the work again:
//! the fingerprint (parent content, transformation, options, tool version)
//! is what makes two requests the same request.
//!
//! The queue discipline is the import queue's: a claim takes a lease with
//! `FOR UPDATE SKIP LOCKED`, so two server processes never run the same job,
//! and a job whose worker died becomes claimable again when its lease lapses.

use std::str::FromStr;

use serde_json::Value;
use sqlx::{PgPool, Row};
use tangible_domain::{
    ArtifactId, DerivationJobId, DerivationJobState, LossCharacter, Transformation,
};
use time::OffsetDateTime;

use crate::DbError;

/// A job as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct DerivationJobRecord {
    /// The job.
    pub id: DerivationJobId,
    /// What it derives from.
    pub parent_artifact_id: ArtifactId,
    /// What it does.
    pub transformation: Transformation,
    /// Its normalized options.
    pub options: Value,
    /// The tool, and the version the fingerprint was computed for.
    pub tool_name: String,
    /// The tool's version.
    pub tool_version: String,
    /// The fingerprint.
    pub command_fingerprint: String,
    /// Where it has got to.
    pub state: DerivationJobState,
    /// The derivative, once made.
    pub child_artifact_id: Option<ArtifactId>,
    /// How many times a worker has taken it.
    pub attempts: i32,
    /// Why it last failed.
    pub error_code: Option<String>,
    /// Detail of the last failure.
    pub error_detail: Option<String>,
    /// Who asked.
    pub created_by: String,
    /// When it was asked for.
    pub created_at: OffsetDateTime,
    /// When it last changed.
    pub updated_at: OffsetDateTime,
    /// When a worker first took it.
    pub started_at: Option<OffsetDateTime>,
    /// When it finished.
    pub completed_at: Option<OffsetDateTime>,
}

/// A derivation as recorded: the lineage between two artifacts.
#[derive(Debug, Clone, PartialEq)]
pub struct DerivationRecord {
    /// The parent.
    pub parent_artifact_id: ArtifactId,
    /// The derivative.
    pub child_artifact_id: ArtifactId,
    /// What was done.
    pub transformation: String,
    /// The tool.
    pub tool_name: String,
    /// Its version.
    pub tool_version: String,
    /// The normalized options.
    pub options: Value,
    /// The fingerprint.
    pub command_fingerprint: String,
    /// What the derivative preserved.
    pub loss_character: LossCharacter,
    /// When the work started.
    pub started_at: OffsetDateTime,
    /// When it finished.
    pub completed_at: Option<OffsetDateTime>,
}

/// A new request.
#[derive(Debug, Clone)]
pub struct NewDerivationJob<'a> {
    /// What to derive from.
    pub parent_artifact_id: ArtifactId,
    /// What to do.
    pub transformation: Transformation,
    /// The normalized options.
    pub options: &'a Value,
    /// The tool.
    pub tool_name: &'a str,
    /// Its version.
    pub tool_version: &'a str,
    /// The fingerprint.
    pub command_fingerprint: &'a str,
    /// Who asks.
    pub created_by: &'a str,
}

/// What a request came to.
#[derive(Debug, Clone, PartialEq)]
pub enum DerivationRequestOutcome {
    /// The derivative already exists.
    Exists(DerivationRecord),
    /// The same work is already queued or running.
    Pending(DerivationJobRecord),
    /// A new job was queued.
    Queued(DerivationJobRecord),
}

const JOB_COLUMNS: &str = "id, parent_artifact_id, transformation, options_json, tool_name, \
     tool_version, command_fingerprint, state, child_artifact_id, attempts, error_code, \
     error_detail, created_by, created_at, updated_at, started_at, completed_at";

const DERIVATION_COLUMNS: &str = "parent_artifact_id, child_artifact_id, transformation, \
     tool_name, tool_version, normalized_options_json, command_fingerprint, loss_character, \
     started_at, completed_at";

fn parse<T: FromStr>(text: &str, column: &'static str) -> Result<T, DbError> {
    text.parse().map_err(|_| DbError::Enum {
        column,
        value: text.to_owned(),
    })
}

fn job_from_row(row: &sqlx::postgres::PgRow) -> Result<DerivationJobRecord, DbError> {
    let get = |e| DbError::Query(e);
    let transformation: String = row.try_get("transformation").map_err(get)?;
    let state: String = row.try_get("state").map_err(get)?;
    Ok(DerivationJobRecord {
        id: DerivationJobId::from_uuid(row.try_get("id").map_err(get)?),
        parent_artifact_id: ArtifactId::from_uuid(row.try_get("parent_artifact_id").map_err(get)?),
        transformation: parse(&transformation, "transformation")?,
        options: row.try_get("options_json").map_err(get)?,
        tool_name: row.try_get("tool_name").map_err(get)?,
        tool_version: row.try_get("tool_version").map_err(get)?,
        command_fingerprint: row.try_get("command_fingerprint").map_err(get)?,
        state: parse(&state, "state")?,
        child_artifact_id: row
            .try_get::<Option<uuid::Uuid>, _>("child_artifact_id")
            .map_err(get)?
            .map(ArtifactId::from_uuid),
        attempts: row.try_get("attempts").map_err(get)?,
        error_code: row.try_get("error_code").map_err(get)?,
        error_detail: row.try_get("error_detail").map_err(get)?,
        created_by: row.try_get("created_by").map_err(get)?,
        created_at: row.try_get("created_at").map_err(get)?,
        updated_at: row.try_get("updated_at").map_err(get)?,
        started_at: row.try_get("started_at").map_err(get)?,
        completed_at: row.try_get("completed_at").map_err(get)?,
    })
}

fn derivation_from_row(row: &sqlx::postgres::PgRow) -> Result<DerivationRecord, DbError> {
    let get = |e| DbError::Query(e);
    let loss: String = row.try_get("loss_character").map_err(get)?;
    Ok(DerivationRecord {
        parent_artifact_id: ArtifactId::from_uuid(row.try_get("parent_artifact_id").map_err(get)?),
        child_artifact_id: ArtifactId::from_uuid(row.try_get("child_artifact_id").map_err(get)?),
        transformation: row.try_get("transformation").map_err(get)?,
        tool_name: row.try_get("tool_name").map_err(get)?,
        tool_version: row.try_get("tool_version").map_err(get)?,
        options: row.try_get("normalized_options_json").map_err(get)?,
        command_fingerprint: row.try_get("command_fingerprint").map_err(get)?,
        loss_character: parse(&loss, "loss_character")?,
        started_at: row.try_get("started_at").map_err(get)?,
        completed_at: row.try_get("completed_at").map_err(get)?,
    })
}

/// A lease length the database accepts: whole seconds, at least one, and no
/// more than a day.
fn lease_seconds(seconds: i64) -> i32 {
    i32::try_from(seconds.clamp(1, 86_400)).unwrap_or(86_400)
}

/// Ask for a derivative.
///
/// Returns the existing derivation when one with this fingerprint was
/// already made from this parent, the open job when the same work is queued
/// or running, and otherwise queues a new job. The request is audited either
/// way.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn request_derivation(
    pool: &PgPool,
    request: &NewDerivationJob<'_>,
) -> Result<DerivationRequestOutcome, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;

    let existing = sqlx::query(&format!(
        "SELECT {DERIVATION_COLUMNS} FROM derivations
         WHERE parent_artifact_id = $1 AND command_fingerprint = $2 AND NOT superseded
         LIMIT 1"
    ))
    .bind(request.parent_artifact_id.as_uuid())
    .bind(request.command_fingerprint)
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    let outcome = if let Some(row) = existing {
        DerivationRequestOutcome::Exists(derivation_from_row(&row)?)
    } else {
        // The partial unique index decides the race between two requests:
        // whichever inserts first wins, and the other finds its job.
        let inserted = sqlx::query(&format!(
            "INSERT INTO derivation_jobs
                 (id, parent_artifact_id, transformation, options_json, tool_name,
                  tool_version, command_fingerprint, created_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (parent_artifact_id, command_fingerprint)
                 WHERE state IN ('queued', 'running', 'failed_retryable')
                 DO NOTHING
             RETURNING {JOB_COLUMNS}"
        ))
        .bind(DerivationJobId::generate().as_uuid())
        .bind(request.parent_artifact_id.as_uuid())
        .bind(request.transformation.as_str())
        .bind(request.options)
        .bind(request.tool_name)
        .bind(request.tool_version)
        .bind(request.command_fingerprint)
        .bind(request.created_by)
        .fetch_optional(&mut *tx)
        .await
        .map_err(DbError::Query)?;
        if let Some(row) = inserted {
            DerivationRequestOutcome::Queued(job_from_row(&row)?)
        } else {
            let row = sqlx::query(&format!(
                "SELECT {JOB_COLUMNS} FROM derivation_jobs
                 WHERE parent_artifact_id = $1 AND command_fingerprint = $2
                   AND state IN ('queued', 'running', 'failed_retryable')"
            ))
            .bind(request.parent_artifact_id.as_uuid())
            .bind(request.command_fingerprint)
            .fetch_one(&mut *tx)
            .await
            .map_err(DbError::Query)?;
            DerivationRequestOutcome::Pending(job_from_row(&row)?)
        }
    };

    let (result, job_id) = match &outcome {
        DerivationRequestOutcome::Exists(_) => ("exists", None),
        DerivationRequestOutcome::Pending(job) => ("pending", Some(job.id.to_string())),
        DerivationRequestOutcome::Queued(job) => ("queued", Some(job.id.to_string())),
    };
    sqlx::query(
        "INSERT INTO audit_events
             (id, actor_type, actor_id, action, target_type, target_id, outcome, metadata)
         VALUES ($1, 'user', $2, 'derivation.requested', 'artifact', $3, 'success', $4)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(request.created_by)
    .bind(request.parent_artifact_id.to_string())
    .bind(serde_json::json!({
        "transformation": request.transformation.as_str(),
        "tool_version": request.tool_version,
        "fingerprint": request.command_fingerprint,
        "result": result,
        "job_id": job_id,
    }))
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    tx.commit().await.map_err(DbError::Query)?;
    Ok(outcome)
}

/// One job.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn get_derivation_job(
    pool: &PgPool,
    id: DerivationJobId,
) -> Result<Option<DerivationJobRecord>, DbError> {
    sqlx::query(&format!(
        "SELECT {JOB_COLUMNS} FROM derivation_jobs WHERE id = $1"
    ))
    .bind(id.as_uuid())
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?
    .as_ref()
    .map(job_from_row)
    .transpose()
}

/// Every job asked of a parent, newest first.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn derivation_jobs_for(
    pool: &PgPool,
    parent: ArtifactId,
) -> Result<Vec<DerivationJobRecord>, DbError> {
    sqlx::query(&format!(
        "SELECT {JOB_COLUMNS} FROM derivation_jobs
         WHERE parent_artifact_id = $1
         ORDER BY created_at DESC
         LIMIT 100"
    ))
    .bind(parent.as_uuid())
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?
    .iter()
    .map(job_from_row)
    .collect()
}

/// An artifact's lineage: what it was derived from, and what was derived
/// from it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Lineage {
    /// How it was made, when it is a derivative.
    pub derived_from: Option<DerivationRecord>,
    /// What was made from it, newest first.
    pub derivatives: Vec<DerivationRecord>,
}

/// Read an artifact's lineage.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn lineage(pool: &PgPool, artifact: ArtifactId) -> Result<Lineage, DbError> {
    let derived_from = sqlx::query(&format!(
        "SELECT {DERIVATION_COLUMNS} FROM derivations
         WHERE child_artifact_id = $1
         ORDER BY started_at DESC LIMIT 1"
    ))
    .bind(artifact.as_uuid())
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?
    .as_ref()
    .map(derivation_from_row)
    .transpose()?;
    let derivatives = sqlx::query(&format!(
        "SELECT {DERIVATION_COLUMNS} FROM derivations
         WHERE parent_artifact_id = $1 AND NOT superseded
         ORDER BY started_at DESC LIMIT 100"
    ))
    .bind(artifact.as_uuid())
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?
    .iter()
    .map(derivation_from_row)
    .collect::<Result<_, _>>()?;
    Ok(Lineage {
        derived_from,
        derivatives,
    })
}

/// Take the next job a worker may run, under a lease.
///
/// Includes a running job whose lease lapsed: its worker died, and the job
/// starts again from the beginning, because a derivation's only state is its
/// output file, which a dead worker cannot be trusted to have finished.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn claim_next_derivation_job(
    pool: &PgPool,
    lease_owner: &str,
    lease_duration_seconds: i64,
) -> Result<Option<DerivationJobRecord>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let row = sqlx::query(
        "SELECT id FROM derivation_jobs
         WHERE state IN ('queued', 'running', 'failed_retryable')
           AND (lease_expires_at IS NULL OR lease_expires_at < now())
         ORDER BY created_at
         FOR UPDATE SKIP LOCKED
         LIMIT 1",
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let Some(row) = row else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(None);
    };
    let id: uuid::Uuid = row.try_get("id").map_err(DbError::Query)?;
    let row = sqlx::query(&format!(
        "UPDATE derivation_jobs
         SET state = 'running', attempts = attempts + 1,
             started_at = COALESCE(started_at, now()),
             lease_owner = $2,
             lease_expires_at = now() + ($3::int * interval '1 second'),
             error_code = NULL, error_detail = NULL
         WHERE id = $1
         RETURNING {JOB_COLUMNS}"
    ))
    .bind(id)
    .bind(lease_owner)
    .bind(lease_seconds(lease_duration_seconds))
    .fetch_one(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    tx.commit().await.map_err(DbError::Query)?;
    job_from_row(&row).map(Some)
}

/// How a finished derivation is recorded.
#[derive(Debug, Clone, Copy)]
pub struct CompletedDerivation {
    /// The job.
    pub job_id: DerivationJobId,
    /// The derivative, already catalogued.
    pub child_artifact_id: ArtifactId,
    /// What it preserved.
    pub loss_character: LossCharacter,
}

/// Record a finished derivation.
///
/// In one transaction: the lineage row, the derivative linked to every disc
/// its parent is linked to (as another representation of the same disc),
/// the job marked complete, and an audit event. Returns false when the job
/// is no longer running, which leaves everything untouched.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn complete_derivation_job(
    pool: &PgPool,
    completed: CompletedDerivation,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let Some(job) = sqlx::query(&format!(
        "SELECT {JOB_COLUMNS} FROM derivation_jobs WHERE id = $1 FOR UPDATE"
    ))
    .bind(completed.job_id.as_uuid())
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?
    .as_ref()
    .map(job_from_row)
    .transpose()?
    else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(false);
    };
    if job.state.transition(DerivationJobState::Complete).is_err() {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(false);
    }

    sqlx::query(
        "INSERT INTO derivations
             (id, parent_artifact_id, child_artifact_id, transformation, tool_name,
              tool_version, normalized_options_json, command_fingerprint, loss_character,
              started_at, completed_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, COALESCE($10, now()), now())",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(job.parent_artifact_id.as_uuid())
    .bind(completed.child_artifact_id.as_uuid())
    .bind(job.transformation.as_str())
    .bind(&job.tool_name)
    .bind(&job.tool_version)
    .bind(&job.options)
    .bind(&job.command_fingerprint)
    .bind(completed.loss_character.as_str())
    .bind(job.started_at)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    sqlx::query(
        "INSERT INTO artifact_disc_links
             (artifact_id, disc_id, relationship, confidence, evidence_json)
         SELECT $1, disc_id, 'representation_of', confidence,
                jsonb_build_object('derived_from', $2::text, 'job', $3::text)
         FROM artifact_disc_links WHERE artifact_id = $4
         ON CONFLICT (artifact_id, disc_id) DO NOTHING",
    )
    .bind(completed.child_artifact_id.as_uuid())
    .bind(job.parent_artifact_id.to_string())
    .bind(job.id.to_string())
    .bind(job.parent_artifact_id.as_uuid())
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    sqlx::query(
        "UPDATE derivation_jobs
         SET state = 'complete', child_artifact_id = $2, completed_at = now(),
             lease_owner = NULL, lease_expires_at = NULL
         WHERE id = $1",
    )
    .bind(job.id.as_uuid())
    .bind(completed.child_artifact_id.as_uuid())
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    sqlx::query(
        "INSERT INTO audit_events
             (id, actor_type, actor_id, action, target_type, target_id, outcome, metadata)
         VALUES ($1, 'system', 'derivation-runner', 'derivation.completed', 'artifact', $2,
                 'success', $3)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(completed.child_artifact_id.to_string())
    .bind(serde_json::json!({
        "parent": job.parent_artifact_id.to_string(),
        "job_id": job.id.to_string(),
        "transformation": job.transformation.as_str(),
        "loss_character": completed.loss_character.as_str(),
    }))
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    tx.commit().await.map_err(DbError::Query)?;
    Ok(true)
}

/// Record a failed attempt.
///
/// A retryable failure is held off for `retry_after_seconds` before the job
/// is offered again; a terminal one finishes the job.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn fail_derivation_job(
    pool: &PgPool,
    id: DerivationJobId,
    retryable: bool,
    error_code: &str,
    error_detail: &str,
    retry_after_seconds: i64,
) -> Result<bool, DbError> {
    let state = if retryable {
        DerivationJobState::FailedRetryable
    } else {
        DerivationJobState::FailedTerminal
    };
    let detail: String = error_detail.chars().take(10_000).collect();
    let result = sqlx::query(
        "UPDATE derivation_jobs
         SET state = $2, error_code = $3, error_detail = $4,
             lease_owner = CASE WHEN $5 THEN 'retry-backoff' ELSE NULL END,
             lease_expires_at = CASE
                 WHEN $5 THEN now() + ($6::int * interval '1 second')
                 ELSE NULL
             END,
             completed_at = CASE WHEN $5 THEN NULL ELSE now() END
         WHERE id = $1 AND state = 'running'",
    )
    .bind(id.as_uuid())
    .bind(state.as_str())
    .bind(error_code)
    .bind(&detail)
    .bind(retryable)
    .bind(lease_seconds(retry_after_seconds))
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(result.rows_affected() > 0)
}

/// What cancelling a job came to.
#[derive(Debug, Clone, PartialEq)]
pub enum CancelOutcome {
    /// Cancelled.
    Canceled(Box<DerivationJobRecord>),
    /// No such job.
    NotFound,
    /// The job is running or finished, and cannot be cancelled.
    Refused(DerivationJobState),
}

/// Withdraw a job that has not started, or is waiting to be retried.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn cancel_derivation_job(
    pool: &PgPool,
    id: DerivationJobId,
    actor: &str,
) -> Result<CancelOutcome, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let Some(job) = sqlx::query(&format!(
        "SELECT {JOB_COLUMNS} FROM derivation_jobs WHERE id = $1 FOR UPDATE"
    ))
    .bind(id.as_uuid())
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?
    .as_ref()
    .map(job_from_row)
    .transpose()?
    else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(CancelOutcome::NotFound);
    };
    if job.state.transition(DerivationJobState::Canceled).is_err() {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(CancelOutcome::Refused(job.state));
    }
    let row = sqlx::query(&format!(
        "UPDATE derivation_jobs
         SET state = 'canceled', completed_at = now(),
             lease_owner = NULL, lease_expires_at = NULL
         WHERE id = $1
         RETURNING {JOB_COLUMNS}"
    ))
    .bind(id.as_uuid())
    .fetch_one(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    sqlx::query(
        "INSERT INTO audit_events
             (id, actor_type, actor_id, action, target_type, target_id, outcome, metadata)
         VALUES ($1, 'user', $2, 'derivation.canceled', 'derivation_job', $3, 'success', '{}')",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(actor)
    .bind(id.to_string())
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(CancelOutcome::Canceled(Box::new(job_from_row(&row)?)))
}
