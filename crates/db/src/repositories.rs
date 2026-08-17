// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Repositories over the schema.
//!
//! Free functions taking a pool or a transaction rather than structs holding
//! one. There is only ever one implementation, so a trait would add indirection
//! without buying substitutability, and a function that takes an executor can
//! be composed into a caller's transaction, which matters because claiming a
//! job and recording its attempt must be one atomic step.
//!
//! Two rules from the conventions shape everything here. Queue claims use
//! `SELECT ... FOR UPDATE SKIP LOCKED`, so concurrent workers step over each
//! other's rows rather than serialising or deadlocking. And no transaction
//! spans a network call or a subprocess: every function below opens, does its
//! work against the database alone, and commits.

use sqlx::{PgPool, Postgres, Row, Transaction};
use tangible_domain::{BurnAttemptId, BurnJobId, DriveId, WorkerId};
use time::OffsetDateTime;

use crate::DbError;

/// Attempt states that occupy a drive.
///
/// Must match the partial unique indexes in the schema. Kept in one place so
/// the query and the index cannot drift apart: if they did, the index would
/// still be correct and the query would simply stop finding conflicts, which
/// is the quiet kind of wrong.
pub const ACTIVE_ATTEMPT_STATES: &[&str] = &[
    "claimed",
    "staging",
    "preflighting",
    "writing",
    "written",
    "verifying",
];

/// A job a worker has just been given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedJob {
    /// The attempt created for this claim.
    pub attempt_id: BurnAttemptId,
    /// The job.
    pub burn_job_id: BurnJobId,
    /// Which attempt this is for the job, starting at 1.
    pub attempt_number: i32,
    /// The artifact to write.
    pub artifact_id: uuid::Uuid,
    /// The disc it represents.
    pub disc_id: uuid::Uuid,
    /// Ordered verification steps the job requires.
    pub verification_policy: Vec<String>,
    /// When the lease lapses.
    pub lease_expires_at: OffsetDateTime,
}

/// Why a claim could not be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// No queued job was available.
    NoWork,
    /// The drive already has an active attempt.
    ///
    /// Distinct from `NoWork` because it means something different to a
    /// worker: there is work, but this drive is busy with it.
    DriveBusy,
}

/// Why an enrollment could not be completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnrollmentOutcome {
    /// The token does not exist, has been used, was revoked, or has expired.
    ///
    /// One variant covering all four on purpose. A caller able to tell them
    /// apart could confirm that a guessed token once existed, so nothing
    /// downstream is given the chance to leak the difference.
    TokenUnusable,
    /// Another worker already uses that name.
    ///
    /// Safe to report precisely: it concerns the request, not the token.
    NameTaken,
}

/// Claim the next queued burn job for a drive.
///
/// The concurrency-critical function in the system. Two workers polling at the
/// same moment must never receive the same job, and one drive must never end
/// up with two active attempts, because either produces two lasers writing
/// what the records describe as one disc.
///
/// Both are enforced by the database rather than by timing. `FOR UPDATE SKIP
/// LOCKED` gives each caller a different row, and the partial unique index on
/// active attempts rejects a second attempt for a busy drive even if the
/// application logic somehow got that far.
///
/// # Limitation
///
/// Compatibility is not yet considered: any worker may claim any queued job.
/// Once workers report their drive capabilities, this must filter on the
/// media profiles the job requires against those the drive can write, or a
/// worker will accept jobs it can only fail at preflight.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn claim_next_burn_job(
    pool: &PgPool,
    worker_id: WorkerId,
    drive_id: DriveId,
    engine: &str,
    engine_version: &str,
    lease_token_hash: &str,
    lease_duration_seconds: i64,
) -> Result<Result<ClaimedJob, ClaimOutcome>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;

    // Refuse before touching the queue if this drive is already working. Doing
    // it first means a busy drive does not lock a job row it cannot use.
    if drive_has_active_attempt(&mut tx, drive_id).await? {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(ClaimOutcome::DriveBusy));
    }

    // SKIP LOCKED rather than a plain FOR UPDATE: a second worker arriving
    // mid-transaction steps over this row and takes the next one, instead of
    // blocking until the first commits.
    let job = sqlx::query(
        "SELECT id, artifact_id, disc_id, verification_policy
         FROM burn_jobs
         WHERE state = 'queued'
         ORDER BY priority DESC, created_at
         FOR UPDATE SKIP LOCKED
         LIMIT 1",
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    let Some(job) = job else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(ClaimOutcome::NoWork));
    };

    let burn_job_id: uuid::Uuid = job.try_get("id").map_err(DbError::Query)?;
    let artifact_id: uuid::Uuid = job.try_get("artifact_id").map_err(DbError::Query)?;
    let disc_id: uuid::Uuid = job.try_get("disc_id").map_err(DbError::Query)?;
    let verification_policy: Vec<String> =
        job.try_get("verification_policy").map_err(DbError::Query)?;

    // Attempt numbers are per job and must not repeat, so they are derived
    // inside the same transaction that holds the job row locked.
    let previous: Option<i32> =
        sqlx::query("SELECT max(attempt_number) FROM burn_attempts WHERE burn_job_id = $1")
            .bind(burn_job_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(DbError::Query)?
            .try_get(0)
            .map_err(DbError::Query)?;
    let attempt_number = previous.unwrap_or(0) + 1;

    let attempt_id = BurnAttemptId::generate();
    let lease_expires_at =
        OffsetDateTime::now_utc() + time::Duration::seconds(lease_duration_seconds);

    let inserted = sqlx::query(
        "INSERT INTO burn_attempts
             (id, burn_job_id, worker_id, drive_id, attempt_number, state,
              lease_token_hash, lease_expires_at, engine, engine_version, plan_json)
         VALUES ($1, $2, $3, $4, $5, 'claimed', $6, $7, $8, $9, '{}'::jsonb)",
    )
    .bind(attempt_id.as_uuid())
    .bind(burn_job_id)
    .bind(worker_id.as_uuid())
    .bind(drive_id.as_uuid())
    .bind(attempt_number)
    .bind(lease_token_hash)
    .bind(lease_expires_at)
    .bind(engine)
    .bind(engine_version)
    .execute(&mut *tx)
    .await;

    if let Err(error) = inserted {
        tx.rollback().await.map_err(DbError::Query)?;
        // The partial unique index fired. Another worker won the race for this
        // drive between the check above and here, which is exactly the window
        // the index exists to close.
        if is_unique_violation(&error) {
            return Ok(Err(ClaimOutcome::DriveBusy));
        }
        return Err(DbError::Query(error));
    }

    sqlx::query("UPDATE burn_jobs SET state = 'leased' WHERE id = $1")
        .bind(burn_job_id)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;

    tx.commit().await.map_err(DbError::Query)?;

    Ok(Ok(ClaimedJob {
        attempt_id,
        burn_job_id: BurnJobId::from_uuid(burn_job_id),
        attempt_number,
        artifact_id,
        disc_id,
        verification_policy,
        lease_expires_at,
    }))
}

/// Whether a drive already has an attempt occupying it.
async fn drive_has_active_attempt(
    tx: &mut Transaction<'_, Postgres>,
    drive_id: DriveId,
) -> Result<bool, DbError> {
    let row = sqlx::query(
        "SELECT EXISTS (
             SELECT 1 FROM burn_attempts
             WHERE drive_id = $1 AND state = ANY($2)
         )",
    )
    .bind(drive_id.as_uuid())
    .bind(ACTIVE_ATTEMPT_STATES)
    .fetch_one(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    row.try_get(0).map_err(DbError::Query)
}

/// Whether an error is a unique-constraint violation.
fn is_unique_violation(error: &sqlx::Error) -> bool {
    // 23505 is the SQLSTATE for unique_violation. Matching the code rather
    // than the message text keeps this working across locales and versions.
    matches!(
        error.as_database_error().and_then(sqlx::error::DatabaseError::code),
        Some(code) if code == "23505"
    )
}

/// Extend a lease.
///
/// Returns the new expiry, or `None` if the attempt is no longer active. A
/// worker whose renewal is refused keeps writing if it is mid-write; the
/// refusal only stops it claiming anything further.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn renew_lease(
    pool: &PgPool,
    attempt_id: BurnAttemptId,
    lease_token_hash: &str,
    lease_duration_seconds: i64,
) -> Result<Option<OffsetDateTime>, DbError> {
    let expires_at = OffsetDateTime::now_utc() + time::Duration::seconds(lease_duration_seconds);

    // The token hash is part of the WHERE clause, so a renewal presented by
    // anything other than the holder simply matches no row.
    let row = sqlx::query(
        "UPDATE burn_attempts
         SET lease_expires_at = $1
         WHERE id = $2 AND lease_token_hash = $3 AND state = ANY($4)
         RETURNING lease_expires_at",
    )
    .bind(expires_at)
    .bind(attempt_id.as_uuid())
    .bind(lease_token_hash)
    .bind(ACTIVE_ATTEMPT_STATES)
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;

    row.map(|row| row.try_get("lease_expires_at").map_err(DbError::Query))
        .transpose()
}

/// One event a worker submitted.
#[derive(Debug, Clone)]
pub struct IncomingEvent {
    /// Monotonic within the attempt.
    pub sequence: i64,
    /// Event type.
    pub event_type: String,
    /// Stage it relates to.
    pub stage: String,
    /// Stable machine-readable code.
    pub message_code: String,
    /// Progress from 0 to 1, when reported.
    pub progress: Option<f32>,
    /// Structured detail.
    pub data: serde_json::Value,
    /// When the worker observed it.
    pub worker_timestamp: OffsetDateTime,
}

/// Record a batch of worker events and report what is now persisted.
///
/// Idempotent by `(attempt_id, sequence)`: a replayed batch collides with the
/// primary key and is ignored rather than duplicating. That is what lets a
/// worker resend everything unacknowledged after a network outage without
/// knowing which of them arrived.
///
/// Returns the highest contiguous sequence held for the attempt, which is what
/// the worker prunes against.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn record_events(
    pool: &PgPool,
    attempt_id: BurnAttemptId,
    events: &[IncomingEvent],
) -> Result<i64, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;

    for event in events {
        sqlx::query(
            "INSERT INTO burn_events
                 (attempt_id, sequence, event_type, stage, progress_fraction,
                  message_code, data_json, worker_timestamp)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (attempt_id, sequence) DO NOTHING",
        )
        .bind(attempt_id.as_uuid())
        .bind(event.sequence)
        .bind(&event.event_type)
        .bind(&event.stage)
        .bind(event.progress)
        .bind(&event.message_code)
        .bind(&event.data)
        .bind(event.worker_timestamp)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    }

    let highest = highest_contiguous_sequence(&mut tx, attempt_id).await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(highest)
}

/// The highest sequence with no gap beneath it.
///
/// Contiguous rather than maximum on purpose. If sequences 1, 2 and 5 arrived,
/// acknowledging 5 would tell the worker to discard 3 and 4, which it still
/// holds and the server does not. Acknowledging 2 keeps them coming.
async fn highest_contiguous_sequence(
    tx: &mut Transaction<'_, Postgres>,
    attempt_id: BurnAttemptId,
) -> Result<i64, DbError> {
    let row = sqlx::query(
        "SELECT coalesce(max(sequence), 0) AS through
         FROM (
             SELECT sequence, row_number() OVER (ORDER BY sequence) AS position
             FROM burn_events
             WHERE attempt_id = $1
         ) numbered
         WHERE sequence = position",
    )
    .bind(attempt_id.as_uuid())
    .fetch_one(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    row.try_get("through").map_err(DbError::Query)
}

/// Exchange an enrollment token for a worker.
///
/// The one-use guarantee is the `SELECT ... FOR UPDATE` that opens the
/// transaction. A second request for the same token blocks on that row lock,
/// and when the first commits, PostgreSQL re-evaluates the predicate against
/// the now-consumed row and finds nothing. Two simultaneous requests therefore
/// cannot both succeed, without an unlocked check-then-update window between
/// them.
///
/// Reports [`EnrollmentOutcome::TokenUnusable`] when the token does not exist,
/// has been used, was revoked, or has expired. Those are deliberately one
/// outcome: a caller that could tell them apart could probe for tokens that
/// once existed.
///
/// A name already in use is reported separately, because it is an ordinary
/// mistake an operator can fix and says nothing about any token.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn consume_enrollment(
    pool: &PgPool,
    token_hash: &str,
    worker_name: &str,
    software_version: &str,
    protocol_version: &str,
    credential_hash: &str,
) -> Result<Result<WorkerId, EnrollmentOutcome>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;

    // The token is settled before anything else is touched. Inserting the
    // worker first would make this route answer differently for a name that
    // exists than for one that does not, turning an unauthenticated endpoint
    // into an oracle for worker names: send any junk token and read the status.
    //
    // FOR UPDATE, not a bare SELECT: it holds the row for the rest of the
    // transaction, so a concurrent request waits here rather than reading
    // 'unused' alongside this one.
    let enrollment = sqlx::query(
        "SELECT id FROM worker_enrollments
         WHERE token_hash = $1 AND state = 'unused' AND expires_at > now()
         FOR UPDATE",
    )
    .bind(token_hash)
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    let Some(enrollment) = enrollment else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(EnrollmentOutcome::TokenUnusable));
    };
    let enrollment_id: uuid::Uuid = enrollment.try_get("id").map_err(DbError::Query)?;

    // The worker is inserted before the token is marked consumed, because the
    // enrollment row's consumed-is-whole constraint requires the state, the
    // timestamp and the worker to be set together. Marking the token used and
    // filling in the worker afterwards violates it in between, which the
    // constraint caught, and which is exactly the intermediate state it exists
    // to forbid.
    let worker_id = WorkerId::generate();
    let inserted = sqlx::query(
        "INSERT INTO workers
             (id, name, status, software_version, protocol_version, credential_hash)
         VALUES ($1, $2, 'pending', $3, $4, $5)",
    )
    .bind(worker_id.as_uuid())
    .bind(worker_name)
    .bind(software_version)
    .bind(protocol_version)
    .bind(credential_hash)
    .execute(&mut *tx)
    .await;

    if let Err(error) = inserted {
        // Worker names are unique, so a second worker enrolling under a name
        // already taken lands here. Reported as its own outcome rather than a
        // database failure: it is a request the operator can correct, and
        // calling it a storage fault points them at the wrong problem.
        if is_unique_violation(&error) {
            tx.rollback().await.map_err(DbError::Query)?;
            return Ok(Err(EnrollmentOutcome::NameTaken));
        }
        return Err(DbError::Query(error));
    }

    // Addressed by id, which this transaction holds locked: no other request
    // can have changed the row out from under it since the check above.
    sqlx::query(
        "UPDATE worker_enrollments
         SET state = 'consumed', consumed_at = now(), consumed_by_worker_id = $2
         WHERE id = $1",
    )
    .bind(enrollment_id)
    .bind(worker_id.as_uuid())
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    tx.commit().await.map_err(DbError::Query)?;
    Ok(Ok(worker_id))
}

/// Find a worker by its credential hash.
///
/// Revoked workers are excluded here rather than checked afterwards, so a
/// caller cannot forget to.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn authenticate_worker(
    pool: &PgPool,
    credential_hash: &str,
) -> Result<Option<WorkerId>, DbError> {
    let row =
        sqlx::query("SELECT id FROM workers WHERE credential_hash = $1 AND status <> 'revoked'")
            .bind(credential_hash)
            .fetch_optional(pool)
            .await
            .map_err(DbError::Query)?;

    row.map(|row| {
        row.try_get::<uuid::Uuid, _>("id")
            .map(WorkerId::from_uuid)
            .map_err(DbError::Query)
    })
    .transpose()
}

/// Record that a worker is alive.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn record_heartbeat(pool: &PgPool, worker_id: WorkerId) -> Result<bool, DbError> {
    let result = sqlx::query(
        "UPDATE workers
         SET last_seen_at = now(), status = CASE WHEN status = 'pending' THEN 'online' ELSE status END
         WHERE id = $1 AND status <> 'revoked'",
    )
    .bind(worker_id.as_uuid())
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(result.rows_affected() > 0)
}

/// Move an attempt to a new state.
///
/// The caller is responsible for the transition being legal; the domain state
/// machine decides that, and this only persists the result.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn set_attempt_state(
    pool: &PgPool,
    attempt_id: BurnAttemptId,
    state: &str,
) -> Result<bool, DbError> {
    let result = sqlx::query(
        "UPDATE burn_attempts
         SET state = $1, ended_at = CASE WHEN $1 = ANY($3) THEN NULL ELSE now() END
         WHERE id = $2",
    )
    .bind(state)
    .bind(attempt_id.as_uuid())
    .bind(ACTIVE_ATTEMPT_STATES)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(result.rows_affected() > 0)
}
