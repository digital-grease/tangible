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
use std::str::FromStr;

use tangible_domain::{
    ArtifactId, BurnAttemptId, BurnAttemptState, BurnJobId, BurnJobState, ComponentId, DiscId,
    DiscSetId, DriveId, EditionId, ImportJobId, ImportState, PhysicalCopyId, PhysicalCopyStatus,
    TitleId, WorkerId,
};
use time::OffsetDateTime;

use crate::DbError;

/// Parse a stored enum value, reporting drift rather than guessing.
///
/// A value the enum does not know means the column's CHECK constraint and the
/// domain have diverged. Every available default would be a claim about the
/// state of a burn, so this fails instead.
fn stored_enum<T: FromStr>(column: &'static str, value: &str) -> Result<T, DbError> {
    T::from_str(value).map_err(|_| DbError::Enum {
        column,
        value: value.to_owned(),
    })
}

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
    /// What to do with the disc when the attempt ends.
    pub eject_policy: String,
    /// Media profile the operator asked for, if any.
    pub requested_media_profile: Option<String>,
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
        "SELECT id, artifact_id, disc_id, verification_policy, eject_policy,
                requested_media_profile
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
    let eject_policy: String = job.try_get("eject_policy").map_err(DbError::Query)?;
    let requested_media_profile: Option<String> = job
        .try_get("requested_media_profile")
        .map_err(DbError::Query)?;

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
        eject_policy,
        requested_media_profile,
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

/// What a worker's latest event says it is doing.
///
/// The server drives none of the burn stages itself, so a worker's events are
/// its only account of them. Carried alongside a batch rather than derived
/// from the rows afterwards, because deciding what a stage name means belongs
/// to the protocol layer that knows which worker vocabulary it is speaking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportedStage {
    /// The job state the stage corresponds to.
    pub job: BurnJobState,
    /// The attempt state the stage corresponds to.
    pub attempt: BurnAttemptState,
}

/// Record a batch of worker events and report what is now persisted.
///
/// Idempotent by `(attempt_id, sequence)`: a replayed batch collides with the
/// primary key and is ignored rather than duplicating. That is what lets a
/// worker resend everything unacknowledged after a network outage without
/// knowing which of them arrived.
///
/// When `reported` is present, the job and attempt are moved to match what the
/// worker says it is doing, subject to the domain's rules about which reports
/// may be adopted. Both happen in one transaction: an event saying a write has
/// begun and a record saying the job has not are the same lie told twice.
///
/// Returns the highest contiguous sequence held for the attempt, which is what
/// the worker prunes against.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure, or [`DbError::Enum`] if a stored
/// state is not one this build understands.
pub async fn record_events(
    pool: &PgPool,
    attempt_id: BurnAttemptId,
    events: &[IncomingEvent],
    reported: Option<ReportedStage>,
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

    if let Some(reported) = reported {
        apply_reported_stage(&mut tx, attempt_id, reported).await?;
    }

    let highest = highest_contiguous_sequence(&mut tx, attempt_id).await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(highest)
}

/// Move a job and its attempt to match what a worker reports.
///
/// The rules live in the domain: an attempt that has consumed media only ever
/// moves forward, a settled burn is not reopened, and before any of that a
/// worker may legitimately go backwards: preflight can reject the disc that
/// was inserted and return to waiting for media.
///
/// The attempt row is locked before the job row, the same order
/// [`complete_attempt`] takes them in, so the two cannot deadlock against each
/// other.
async fn apply_reported_stage(
    tx: &mut Transaction<'_, Postgres>,
    attempt_id: BurnAttemptId,
    reported: ReportedStage,
) -> Result<(), DbError> {
    let row = sqlx::query(
        "SELECT a.state AS attempt_state, j.id AS job_id, j.state AS job_state
         FROM burn_attempts a
         JOIN burn_jobs j ON j.id = a.burn_job_id
         WHERE a.id = $1
         FOR UPDATE OF a, j",
    )
    .bind(attempt_id.as_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map_err(DbError::Query)?;

    // No attempt means no state to advance. The events themselves are already
    // stored, and the foreign key means this cannot happen for a batch that
    // inserted anything.
    let Some(row) = row else { return Ok(()) };

    let attempt_state: String = row.try_get("attempt_state").map_err(DbError::Query)?;
    let attempt_state: BurnAttemptState = stored_enum("burn_attempts.state", &attempt_state)?;
    let job_id: uuid::Uuid = row.try_get("job_id").map_err(DbError::Query)?;
    let job_state: String = row.try_get("job_state").map_err(DbError::Query)?;
    let job_state: BurnJobState = stored_enum("burn_jobs.state", &job_state)?;

    if attempt_state != reported.attempt && attempt_state.accepts_report(reported.attempt) {
        // No `ended_at` here: every state a worker can report is a live one,
        // and an attempt that is running has not ended.
        sqlx::query("UPDATE burn_attempts SET state = $2 WHERE id = $1")
            .bind(attempt_id.as_uuid())
            .bind(reported.attempt.as_str())
            .execute(&mut **tx)
            .await
            .map_err(DbError::Query)?;
    }

    if job_state != reported.job && job_state.accepts_report(reported.job) {
        // updated_at is left to the touch trigger, which is the one place
        // that timestamp is set.
        sqlx::query("UPDATE burn_jobs SET state = $2 WHERE id = $1")
            .bind(job_id)
            .bind(reported.job.as_str())
            .execute(&mut **tx)
            .await
            .map_err(DbError::Query)?;
    }

    Ok(())
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

/// Record a newly issued enrollment token, and that it was issued.
///
/// Only the hash is stored; the token itself is shown to whoever asked for it
/// and is not recoverable afterwards. The audit row is written in the same
/// transaction, so a token can never exist without a record of its issue.
///
/// `actor` is who asked, as far as the server knows. Until operator
/// authentication exists that is `unauthenticated`, and the audit row says so
/// rather than inventing an identity.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure, including a hash collision,
/// which the column's uniqueness turns into an error rather than two rows one
/// secret could satisfy.
pub async fn issue_enrollment(
    pool: &PgPool,
    token_hash: &str,
    expires_at: OffsetDateTime,
    actor: &str,
) -> Result<uuid::Uuid, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let enrollment_id = uuid::Uuid::now_v7();

    sqlx::query(
        "INSERT INTO worker_enrollments (id, token_hash, expires_at, created_by)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(enrollment_id)
    .bind(token_hash)
    .bind(expires_at)
    .bind(actor)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    sqlx::query(
        "INSERT INTO audit_events
             (id, actor_type, actor_id, action, target_type, target_id, outcome, metadata)
         VALUES ($1, 'user', $2, 'worker_enrollment.issued', 'worker_enrollment', $3,
                 'success', jsonb_build_object('expires_at', $4::timestamptz))",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(actor)
    .bind(enrollment_id.to_string())
    .bind(expires_at)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    tx.commit().await.map_err(DbError::Query)?;
    Ok(enrollment_id)
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

/// What a worker reports when it finishes an attempt.
///
/// Every field the completion writes, gathered into one struct so the call
/// site reads as a record rather than a queue of positional arguments.
#[derive(Debug, Clone, Copy)]
pub struct AttemptCompletion<'a> {
    /// The terminal state the attempt reached.
    pub state: BurnAttemptState,
    /// The engine's own account of the write.
    pub write_report: &'a serde_json::Value,
    /// The read-back comparison, when one ran.
    pub verify_report: Option<&'a serde_json::Value>,
    /// Media profile of the disc in the drive.
    pub media_profile: &'a str,
    /// Manufacturer identifier, when the drive reported one.
    pub manufacturer_id: Option<&'a str>,
    /// Media serial, when the drive reported one.
    pub media_serial: Option<&'a str>,
    /// Which verification the policy asked for.
    pub verification_level: &'a str,
    /// How that verification turned out.
    pub verification_result: &'a str,
    /// The resulting condition of the disc.
    pub copy_status: &'a str,
    /// A stable failure code, when the attempt failed.
    pub error_code: Option<&'a str>,
    /// Human-readable failure detail, when the attempt failed.
    pub error_detail: Option<&'a str>,
}

/// What completing an attempt produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionRecord {
    /// The disc recorded, if the attempt consumed media.
    pub physical_copy_id: Option<uuid::Uuid>,
    /// The job's eject policy, so the worker can be told what to do with the
    /// medium without a second round trip.
    pub eject_policy: String,
    /// Whether the attempt had already been completed by an earlier request.
    ///
    /// The worker is told the same thing either way. This exists so the server
    /// can tell a retry from a first report in its logs.
    pub already_completed: bool,
}

/// Why an attempt could not be completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionOutcome {
    /// No attempt with that identifier holds that lease.
    ///
    /// One outcome for "no such attempt" and "not your lease" together: a
    /// caller able to distinguish them could confirm which attempt
    /// identifiers exist.
    NoSuchLeasedAttempt,
}

/// The update that records how an attempt ended.
///
/// The engine the attempt was claimed with is the worker's, which for a
/// combined worker is `auto`. Once the write report names the engine that
/// actually did the work, that is what the attempt records. Only a name the
/// column accepts replaces it: completion arrives after a disc may already
/// exist, and failing it over a label would be the worst place to fail.
const RECORD_ATTEMPT_OUTCOME: &str = "UPDATE burn_attempts
     SET state = $2, write_report_json = $3, verify_report_json = $4,
         error_code = $5, error_detail = $6, ended_at = now(),
         engine = CASE WHEN $3->>'engine' IN ('fake', 'xorriso', 'cdrdao')
                       THEN $3->>'engine' ELSE engine END,
         engine_version = CASE WHEN $3->>'engine' IN ('fake', 'xorriso', 'cdrdao')
                                AND length(coalesce($3->>'engine_version', '')) > 0
                               THEN $3->>'engine_version' ELSE engine_version END
     WHERE id = $1";

/// Record the end of a burn attempt.
///
/// Idempotent, because completion is precisely the request a worker retries
/// after a network failure, and it is the request that creates a physical
/// disc record. A second report of an already-finished attempt returns what
/// the first produced rather than recording a second disc, which the unique
/// constraint on `physical_copies.burn_attempt_id` would refuse anyway. Doing
/// it here means the worker sees success on its retry instead of an error it
/// cannot act on.
///
/// A physical copy is recorded exactly when the attempt consumed media, using
/// the same judgement the `physical_copies_require_write` trigger enforces. A
/// ruined disc is still a disc: it must be trackable so it can be destroyed
/// rather than shelved and reused.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn complete_attempt(
    pool: &PgPool,
    attempt_id: BurnAttemptId,
    lease_token_hash: &str,
    completion: AttemptCompletion<'_>,
) -> Result<Result<CompletionRecord, CompletionOutcome>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;

    // The lease hash is part of the predicate, so a completion presented by
    // anything but the holder matches no row. FOR UPDATE OF the attempt alone:
    // the joined job and copy are read, not modified under this lock.
    let existing = sqlx::query(
        "SELECT a.state, a.burn_job_id, j.disc_id, j.artifact_id, j.eject_policy,
                c.id AS copy_id
         FROM burn_attempts a
         JOIN burn_jobs j ON j.id = a.burn_job_id
         LEFT JOIN physical_copies c ON c.burn_attempt_id = a.id
         WHERE a.id = $1 AND a.lease_token_hash = $2
         FOR UPDATE OF a",
    )
    .bind(attempt_id.as_uuid())
    .bind(lease_token_hash)
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    let Some(existing) = existing else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(CompletionOutcome::NoSuchLeasedAttempt));
    };

    let current: String = existing.try_get("state").map_err(DbError::Query)?;
    let copy_id: Option<uuid::Uuid> = existing.try_get("copy_id").map_err(DbError::Query)?;
    let eject_policy: String = existing.try_get("eject_policy").map_err(DbError::Query)?;

    // Already finished: report what the first completion produced. Rewriting
    // the reports would let a late duplicate overwrite the account of a burn
    // that has already been reconciled.
    if BurnAttemptState::from_str(&current).is_ok_and(|state| state.is_terminal()) {
        tx.commit().await.map_err(DbError::Query)?;
        return Ok(Ok(CompletionRecord {
            physical_copy_id: copy_id,
            eject_policy,
            already_completed: true,
        }));
    }

    let burn_job_id: uuid::Uuid = existing.try_get("burn_job_id").map_err(DbError::Query)?;
    let disc_id: uuid::Uuid = existing.try_get("disc_id").map_err(DbError::Query)?;
    let artifact_id: uuid::Uuid = existing.try_get("artifact_id").map_err(DbError::Query)?;

    sqlx::query(RECORD_ATTEMPT_OUTCOME)
        .bind(attempt_id.as_uuid())
        .bind(completion.state.as_str())
        .bind(completion.write_report)
        .bind(completion.verify_report)
        .bind(completion.error_code)
        .bind(completion.error_detail)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;

    let physical_copy_id = if completion.state.consumed_media() {
        let id = uuid::Uuid::now_v7();
        sqlx::query(
            "INSERT INTO physical_copies
                 (id, disc_id, artifact_id, burn_attempt_id, status, media_profile,
                  manufacturer_id, media_serial, verification_level, verification_result)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(id)
        .bind(disc_id)
        .bind(artifact_id)
        .bind(attempt_id.as_uuid())
        .bind(completion.copy_status)
        .bind(completion.media_profile)
        .bind(completion.manufacturer_id)
        .bind(completion.media_serial)
        .bind(completion.verification_level)
        .bind(completion.verification_result)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
        Some(id)
    } else {
        None
    };

    // The job follows the attempt. Only a verified attempt completes a job:
    // anything else leaves work an operator may want to retry, and calling a
    // ruined disc "complete" would hide it.
    let job_state = if completion.state.is_success() {
        "complete"
    } else {
        "failed"
    };
    sqlx::query(
        "UPDATE burn_jobs
         SET state = $2, updated_at = now(), completed_at = now()
         WHERE id = $1",
    )
    .bind(burn_job_id)
    .bind(job_state)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    tx.commit().await.map_err(DbError::Query)?;

    Ok(Ok(CompletionRecord {
        physical_copy_id,
        eject_policy,
        already_completed: false,
    }))
}

/// The state of an attempt, if it belongs to this worker.
///
/// Scoped to the worker in the query rather than checked afterwards, so a
/// recovery cannot be answered using another worker's attempt.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn attempt_state_for_worker(
    pool: &PgPool,
    attempt_id: BurnAttemptId,
    worker_id: WorkerId,
) -> Result<Option<BurnAttemptState>, DbError> {
    let row = sqlx::query("SELECT state FROM burn_attempts WHERE id = $1 AND worker_id = $2")
        .bind(attempt_id.as_uuid())
        .bind(worker_id.as_uuid())
        .fetch_optional(pool)
        .await
        .map_err(DbError::Query)?;

    let Some(row) = row else { return Ok(None) };
    let state: String = row.try_get("state").map_err(DbError::Query)?;
    // An unparseable state means the column and the enum have diverged, which
    // is a bug rather than a missing row; treating it as absent would answer a
    // recovery with "discard", so it is reported as unknown instead.
    Ok(BurnAttemptState::from_str(&state).ok())
}

/// Close an attempt its worker abandoned before writing, and fail its job.
///
/// Called when a restarted worker is told to discard pre-write state. The
/// directive told the worker it could drop its local record; until this
/// existed nothing told the server, so the attempt stayed open with an expired
/// lease, the job kept pointing at it, and no worker could ever claim it
/// again. The first end-to-end run found it by restarting a worker
/// mid-preflight.
///
/// Only an attempt that never reached a write is touched, re-checked here
/// under a row lock, so a recovery can never close an attempt whose disc may
/// exist. The attempt ends as `failed_before_write` with `WORKER_RESTARTED`.
/// The job moves to `failed` through the domain's own transition check, not
/// back to the queue: the job machine treats failure as terminal and leaves
/// spending another disc to the operator, which here is one retry.
///
/// Returns whether anything changed; an attempt already closed is left alone.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure, or [`DbError::Enum`] if a stored
/// state is not one this build understands.
pub async fn abandon_prewrite_attempt(
    pool: &PgPool,
    attempt_id: BurnAttemptId,
    worker_id: WorkerId,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;

    let row = sqlx::query(
        "SELECT a.state, a.burn_job_id, j.state AS job_state
         FROM burn_attempts a JOIN burn_jobs j ON j.id = a.burn_job_id
         WHERE a.id = $1 AND a.worker_id = $2
         FOR UPDATE OF a, j",
    )
    .bind(attempt_id.as_uuid())
    .bind(worker_id.as_uuid())
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let Some(row) = row else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(false);
    };

    let state: String = row.try_get("state").map_err(DbError::Query)?;
    let state: BurnAttemptState = stored_enum("burn_attempts.state", &state)?;
    let before_write = matches!(
        state,
        BurnAttemptState::Claimed | BurnAttemptState::Staging | BurnAttemptState::Preflighting
    );
    if !before_write {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(false);
    }

    sqlx::query(
        "UPDATE burn_attempts
         SET state = 'failed_before_write', error_code = 'WORKER_RESTARTED',
             error_detail = 'the worker restarted before writing; nothing was written',
             ended_at = now()
         WHERE id = $1",
    )
    .bind(attempt_id.as_uuid())
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    let burn_job_id: uuid::Uuid = row.try_get("burn_job_id").map_err(DbError::Query)?;
    let job_state: String = row.try_get("job_state").map_err(DbError::Query)?;
    let job_state: BurnJobState = stored_enum("burn_jobs.state", &job_state)?;
    // A job already settled, by an operator cancelling it say, keeps its
    // state; only one still waiting on this attempt is failed.
    if job_state.transition_to(BurnJobState::Failed).is_ok() {
        sqlx::query("UPDATE burn_jobs SET state = 'failed', completed_at = now() WHERE id = $1")
            .bind(burn_job_id)
            .execute(&mut *tx)
            .await
            .map_err(DbError::Query)?;
    }

    tx.commit().await.map_err(DbError::Query)?;
    Ok(true)
}

// --- burn jobs -----------------------------------------------------------------

/// A burn job with the progress an operator needs to see alongside it.
///
/// The latest event is carried on the job rather than fetched separately
/// because a queue listing without it cannot answer the question the queue
/// exists to answer: what is happening right now.
#[derive(Debug, Clone, PartialEq)]
pub struct BurnJobRecord {
    /// The job.
    pub id: BurnJobId,
    /// Which disc it produces.
    pub disc_id: uuid::Uuid,
    /// Which artifact is written.
    pub artifact_id: uuid::Uuid,
    /// Media profile the operator asked for, if any.
    pub requested_media_profile: Option<String>,
    /// Ordered verification steps.
    pub verification_policy: Vec<String>,
    /// What to do with the disc afterwards.
    pub eject_policy: String,
    /// Queue priority; higher is claimed first.
    pub priority: i16,
    /// Lifecycle state.
    pub state: BurnJobState,
    /// Who asked for it.
    pub created_by: String,
    /// When it was queued.
    pub created_at: OffsetDateTime,
    /// When it last changed.
    pub updated_at: OffsetDateTime,
    /// When it finished, if it has.
    pub completed_at: Option<OffsetDateTime>,
    /// How many attempts have been made, including failed ones.
    pub attempt_count: i64,
    /// Stage of the most recent event, if any worker has reported.
    pub latest_stage: Option<String>,
    /// Progress within that stage, from 0 to 1.
    pub latest_progress: Option<f32>,
    /// Stable code of the most recent event.
    pub latest_message_code: Option<String>,
    /// When the worker observed it.
    pub latest_event_at: Option<OffsetDateTime>,
}

/// The columns and joins every job read shares.
///
/// One string rather than three near-identical queries: a listing and a detail
/// view that disagreed about what a job looks like is a bug a reader would
/// have to diff two SQL statements to find.
const BURN_JOB_SELECT: &str = "
    SELECT j.id, j.disc_id, j.artifact_id, j.requested_media_profile,
           j.verification_policy, j.eject_policy, j.priority, j.state,
           j.created_by, j.created_at, j.updated_at, j.completed_at,
           coalesce(counted.attempts, 0) AS attempt_count,
           latest.stage AS latest_stage,
           latest.progress_fraction AS latest_progress,
           latest.message_code AS latest_message_code,
           latest.worker_timestamp AS latest_event_at
    FROM burn_jobs j
    LEFT JOIN LATERAL (
        SELECT count(*) AS attempts FROM burn_attempts a WHERE a.burn_job_id = j.id
    ) counted ON true
    LEFT JOIN LATERAL (
        SELECT e.stage, e.progress_fraction, e.message_code, e.worker_timestamp
        FROM burn_attempts a
        JOIN burn_events e ON e.attempt_id = a.id
        WHERE a.burn_job_id = j.id
        ORDER BY a.attempt_number DESC, e.sequence DESC
        LIMIT 1
    ) latest ON true
";

fn burn_job_from_row(row: &sqlx::postgres::PgRow) -> Result<BurnJobRecord, DbError> {
    let state: String = row.try_get("state").map_err(DbError::Query)?;
    Ok(BurnJobRecord {
        id: BurnJobId::from_uuid(row.try_get("id").map_err(DbError::Query)?),
        disc_id: row.try_get("disc_id").map_err(DbError::Query)?,
        artifact_id: row.try_get("artifact_id").map_err(DbError::Query)?,
        requested_media_profile: row
            .try_get("requested_media_profile")
            .map_err(DbError::Query)?,
        verification_policy: row.try_get("verification_policy").map_err(DbError::Query)?,
        eject_policy: row.try_get("eject_policy").map_err(DbError::Query)?,
        priority: row.try_get("priority").map_err(DbError::Query)?,
        state: stored_enum("burn_jobs.state", &state)?,
        created_by: row.try_get("created_by").map_err(DbError::Query)?,
        created_at: row.try_get("created_at").map_err(DbError::Query)?,
        updated_at: row.try_get("updated_at").map_err(DbError::Query)?,
        completed_at: row.try_get("completed_at").map_err(DbError::Query)?,
        attempt_count: row.try_get("attempt_count").map_err(DbError::Query)?,
        latest_stage: row.try_get("latest_stage").map_err(DbError::Query)?,
        latest_progress: row.try_get("latest_progress").map_err(DbError::Query)?,
        latest_message_code: row.try_get("latest_message_code").map_err(DbError::Query)?,
        latest_event_at: row.try_get("latest_event_at").map_err(DbError::Query)?,
    })
}

/// What an operator asks for when queueing a burn.
#[derive(Debug, Clone, Copy)]
pub struct NewBurnJob<'a> {
    /// The disc the copy represents.
    pub disc_id: uuid::Uuid,
    /// The artifact to write.
    pub artifact_id: uuid::Uuid,
    /// Media profile requested, or none for whatever fits.
    pub requested_media_profile: Option<&'a str>,
    /// Ordered verification steps. Never empty: the schema refuses that.
    pub verification_policy: &'a [String],
    /// What to do with the disc afterwards.
    pub eject_policy: &'a str,
    /// Queue priority; higher is claimed first.
    pub priority: i16,
    /// Who asked.
    pub created_by: &'a str,
    /// Idempotency key, when the caller supplied one.
    pub idempotency_key: Option<&'a str>,
}

/// The result of a create that succeeded.
#[derive(Debug, Clone, PartialEq)]
pub struct CreatedBurnJob {
    /// The job, whether it was created now or by an earlier identical request.
    pub job: BurnJobRecord,
    /// Whether this returned an existing job rather than creating one.
    pub replayed: bool,
}

/// Why a burn job could not be created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateJobOutcome {
    /// No such disc.
    DiscMissing,
    /// No such artifact.
    ArtifactMissing,
    /// The key was used before, for a different request.
    ///
    /// Reported rather than silently creating a second job: an idempotency
    /// key that returned somebody else's burn would be worse than an error,
    /// and creating a second one defeats the point of the key.
    IdempotencyConflict,
}

/// Queue a burn job.
///
/// Idempotent when a key is supplied. A repeated request carrying a key that
/// was used before returns the job it created, so a retried POST cannot spend
/// a second disc; a key reused for a *different* request is refused.
///
/// Sameness is judged on the request that produced the stored job: its disc,
/// artifact, media profile, verification policy, eject policy, priority and
/// requester. There is no separate fingerprint column because the row already
/// holds every field the request carried.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn create_burn_job(
    pool: &PgPool,
    new: NewBurnJob<'_>,
) -> Result<Result<CreatedBurnJob, CreateJobOutcome>, DbError> {
    // Look before inserting so the common replay costs one query and does not
    // burn a unique-constraint failure. The insert below still handles the
    // race, because two identical requests can arrive at once.
    if let Some(key) = new.idempotency_key
        && let Some(existing) = burn_job_by_idempotency_key(pool, key).await?
    {
        return Ok(replay_or_conflict(existing, new));
    }

    let id = BurnJobId::generate();
    let inserted = sqlx::query(
        "INSERT INTO burn_jobs
             (id, disc_id, artifact_id, requested_media_profile, verification_policy,
              eject_policy, priority, created_by, idempotency_key)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id.as_uuid())
    .bind(new.disc_id)
    .bind(new.artifact_id)
    .bind(new.requested_media_profile)
    .bind(new.verification_policy)
    .bind(new.eject_policy)
    .bind(new.priority)
    .bind(new.created_by)
    .bind(new.idempotency_key)
    .execute(pool)
    .await;

    if let Err(error) = inserted {
        if is_foreign_key_violation(&error) {
            // Which reference is missing matters to whoever is being told:
            // an artifact that has been deleted and a disc that was never
            // created are different mistakes.
            return Ok(Err(match constraint_of(&error) {
                Some(name) if name.contains("disc") => CreateJobOutcome::DiscMissing,
                _ => CreateJobOutcome::ArtifactMissing,
            }));
        }
        if is_unique_violation(&error) {
            // The key raced. Whoever won stored a job; compare against it.
            if let Some(key) = new.idempotency_key
                && let Some(existing) = burn_job_by_idempotency_key(pool, key).await?
            {
                return Ok(replay_or_conflict(existing, new));
            }
        }
        return Err(DbError::Query(error));
    }

    let job = get_burn_job(pool, id).await?.ok_or_else(|| DbError::Enum {
        column: "burn_jobs.id",
        value: id.to_string(),
    })?;

    Ok(Ok(CreatedBurnJob {
        job,
        replayed: false,
    }))
}

/// Decide whether an existing job answers this request or conflicts with it.
fn replay_or_conflict(
    existing: BurnJobRecord,
    new: NewBurnJob<'_>,
) -> Result<CreatedBurnJob, CreateJobOutcome> {
    let same = existing.disc_id == new.disc_id
        && existing.artifact_id == new.artifact_id
        && existing.requested_media_profile.as_deref() == new.requested_media_profile
        && existing.verification_policy == new.verification_policy
        && existing.eject_policy == new.eject_policy
        && existing.priority == new.priority
        && existing.created_by == new.created_by;

    if same {
        Ok(CreatedBurnJob {
            job: existing,
            replayed: true,
        })
    } else {
        Err(CreateJobOutcome::IdempotencyConflict)
    }
}

async fn burn_job_by_idempotency_key(
    pool: &PgPool,
    key: &str,
) -> Result<Option<BurnJobRecord>, DbError> {
    let sql = format!("{BURN_JOB_SELECT} WHERE j.idempotency_key = $1");
    let row = sqlx::query(&sql)
        .bind(key)
        .fetch_optional(pool)
        .await
        .map_err(DbError::Query)?;
    row.as_ref().map(burn_job_from_row).transpose()
}

/// Whether an error is a foreign-key violation.
fn is_foreign_key_violation(error: &sqlx::Error) -> bool {
    // 23503 is the SQLSTATE for foreign_key_violation.
    matches!(
        error.as_database_error().and_then(sqlx::error::DatabaseError::code),
        Some(code) if code == "23503"
    )
}

/// The constraint an error names, when it names one.
fn constraint_of(error: &sqlx::Error) -> Option<String> {
    error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::constraint)
        .map(ToOwned::to_owned)
}

/// Which jobs a listing should include.
#[derive(Debug, Clone, Copy, Default)]
pub struct BurnJobFilter {
    /// Only jobs in this state.
    pub state: Option<BurnJobState>,
    /// Only jobs writing this artifact.
    pub artifact_id: Option<uuid::Uuid>,
    /// Only jobs producing this disc.
    pub disc_id: Option<uuid::Uuid>,
}

/// List burn jobs, newest first.
///
/// Newest first rather than oldest, unlike the library: a queue is read to see
/// what was just asked for and what is running now, and creation order would
/// bury both under everything ever burned.
///
/// Keyset paginated on the identifier, which is UUIDv7 and therefore ordered
/// by creation. `before` is the last identifier of the previous page.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn list_burn_jobs(
    pool: &PgPool,
    filter: BurnJobFilter,
    limit: i64,
    before: Option<uuid::Uuid>,
) -> Result<Vec<BurnJobRecord>, DbError> {
    let sql = format!(
        "{BURN_JOB_SELECT}
         WHERE ($1::text IS NULL OR j.state = $1)
           AND ($2::uuid IS NULL OR j.artifact_id = $2)
           AND ($3::uuid IS NULL OR j.disc_id = $3)
           AND ($4::uuid IS NULL OR j.id < $4)
         ORDER BY j.id DESC
         LIMIT $5"
    );

    let rows = sqlx::query(&sql)
        .bind(filter.state.map(|state| state.as_str()))
        .bind(filter.artifact_id)
        .bind(filter.disc_id)
        .bind(before)
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(DbError::Query)?;

    rows.iter().map(burn_job_from_row).collect()
}

/// One burn job.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn get_burn_job(pool: &PgPool, id: BurnJobId) -> Result<Option<BurnJobRecord>, DbError> {
    let sql = format!("{BURN_JOB_SELECT} WHERE j.id = $1");
    let row = sqlx::query(&sql)
        .bind(id.as_uuid())
        .fetch_optional(pool)
        .await
        .map_err(DbError::Query)?;
    row.as_ref().map(burn_job_from_row).transpose()
}

/// Why a job could not be cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// No such job.
    NotFound,
    /// A disc is already being consumed.
    WriteInProgress,
    /// The job has already finished.
    AlreadyFinished {
        /// The state it finished in.
        state: BurnJobState,
    },
}

/// Cancel a burn job that has not started writing.
///
/// Two checks, not one. The job's own state is consulted through the domain,
/// and then its attempts are examined directly, because the job row learns
/// what a worker is doing only from events that may not have arrived. If an
/// attempt has consumed media the cancellation is refused whatever the job
/// row says: the alternative is telling an operator a burn was cancelled
/// while a laser is still writing.
///
/// Cancelling an already-cancelled job succeeds and changes nothing, so a
/// repeated request is not an error.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure, or [`DbError::Enum`] if a stored
/// state is not one this build understands.
pub async fn cancel_burn_job(
    pool: &PgPool,
    id: BurnJobId,
) -> Result<Result<BurnJobState, CancelOutcome>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;

    let row = sqlx::query("SELECT state FROM burn_jobs WHERE id = $1 FOR UPDATE")
        .bind(id.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(DbError::Query)?;

    let Some(row) = row else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(CancelOutcome::NotFound));
    };
    let state: String = row.try_get("state").map_err(DbError::Query)?;
    let state: BurnJobState = stored_enum("burn_jobs.state", &state)?;

    if state == BurnJobState::Canceled {
        tx.commit().await.map_err(DbError::Query)?;
        return Ok(Ok(BurnJobState::Canceled));
    }

    // The domain owns the rule. It refuses a job that has started writing and
    // one that has already finished.
    if let Err(error) = state.cancel() {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(match error {
            tangible_domain::BurnTransitionError::WriteInProgress => CancelOutcome::WriteInProgress,
            _ => CancelOutcome::AlreadyFinished { state },
        }));
    }

    // The physical check. An attempt that consumed media outranks whatever
    // the job row believes, because the disc exists either way.
    let attempts = sqlx::query(
        "SELECT id, state FROM burn_attempts
         WHERE burn_job_id = $1 AND state = ANY($2)
         FOR UPDATE",
    )
    .bind(id.as_uuid())
    .bind(ACTIVE_ATTEMPT_STATES)
    .fetch_all(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    for attempt in &attempts {
        let raw: String = attempt.try_get("state").map_err(DbError::Query)?;
        let attempt_state: BurnAttemptState = stored_enum("burn_attempts.state", &raw)?;
        if attempt_state.consumed_media() {
            tx.rollback().await.map_err(DbError::Query)?;
            return Ok(Err(CancelOutcome::WriteInProgress));
        }
    }

    // Attempts first, then the job: while both are being written the
    // intermediate state is a cancelled attempt under a live job, which reads
    // as work stopping. The reverse order would briefly show a cancelled job
    // with a running attempt, which reads as a burn nobody is watching.
    sqlx::query(
        "UPDATE burn_attempts SET state = 'canceled', ended_at = now()
         WHERE burn_job_id = $1 AND state = ANY($2)",
    )
    .bind(id.as_uuid())
    .bind(ACTIVE_ATTEMPT_STATES)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    sqlx::query("UPDATE burn_jobs SET state = 'canceled', completed_at = now() WHERE id = $1")
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;

    tx.commit().await.map_err(DbError::Query)?;
    Ok(Ok(BurnJobState::Canceled))
}

/// Why a job could not be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryOutcome {
    /// No such job.
    NotFound,
    /// The job's state does not admit a retry.
    NotRetryable {
        /// The state it is in.
        state: BurnJobState,
    },
}

/// Requeue a failed or cancelled job for another attempt.
///
/// The previous attempts are left exactly as they are. A retry produces a new
/// attempt; it does not revise the record of the discs already spent.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure, or [`DbError::Enum`] if a stored
/// state is not one this build understands.
pub async fn retry_burn_job(
    pool: &PgPool,
    id: BurnJobId,
) -> Result<Result<BurnJobState, RetryOutcome>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;

    let row = sqlx::query("SELECT state FROM burn_jobs WHERE id = $1 FOR UPDATE")
        .bind(id.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(DbError::Query)?;

    let Some(row) = row else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(RetryOutcome::NotFound));
    };
    let state: String = row.try_get("state").map_err(DbError::Query)?;
    let state: BurnJobState = stored_enum("burn_jobs.state", &state)?;

    if state.retry().is_err() {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(RetryOutcome::NotRetryable { state }));
    }

    // Belt and braces, for the same reason cancellation checks it: a job row
    // can lag behind its attempt. Requeueing a job a worker still holds would
    // hand the same disc to a second drive.
    if drive_free_of_active_attempts(&mut tx, id).await? {
        sqlx::query("UPDATE burn_jobs SET state = 'queued', completed_at = NULL WHERE id = $1")
            .bind(id.as_uuid())
            .execute(&mut *tx)
            .await
            .map_err(DbError::Query)?;
        tx.commit().await.map_err(DbError::Query)?;
        Ok(Ok(BurnJobState::Queued))
    } else {
        tx.rollback().await.map_err(DbError::Query)?;
        Ok(Err(RetryOutcome::NotRetryable { state }))
    }
}

/// Record that a person has accounted for a job's disc.
///
/// `needs_attention` is a dead end on purpose: it means a disc may exist that
/// the system cannot account for, and nothing automatic may release it. A
/// person looks, records what they found against the disc's inventory entry,
/// and this is where they say so.
///
/// The job lands on `failed`, from which the ordinary retry applies. Whatever
/// was found, the attempt produced no verified disc.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure, or [`DbError::Enum`] if the
/// stored state is not one this build understands.
pub async fn resolve_burn_job_attention(
    pool: &PgPool,
    id: BurnJobId,
) -> Result<Result<BurnJobState, RetryOutcome>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;

    let row = sqlx::query("SELECT state FROM burn_jobs WHERE id = $1 FOR UPDATE")
        .bind(id.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(DbError::Query)?;

    let Some(row) = row else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(RetryOutcome::NotFound));
    };
    let state: String = row.try_get("state").map_err(DbError::Query)?;
    let state: BurnJobState = stored_enum("burn_jobs.state", &state)?;

    let Ok(next) = state.resolve_attention() else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(RetryOutcome::NotRetryable { state }));
    };

    sqlx::query("UPDATE burn_jobs SET state = $2 WHERE id = $1")
        .bind(id.as_uuid())
        .bind(next.as_str())
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;

    tx.commit().await.map_err(DbError::Query)?;
    Ok(Ok(next))
}

/// Whether a job has no attempt currently occupying a drive.
async fn drive_free_of_active_attempts(
    tx: &mut Transaction<'_, Postgres>,
    id: BurnJobId,
) -> Result<bool, DbError> {
    let row = sqlx::query(
        "SELECT NOT EXISTS (
             SELECT 1 FROM burn_attempts WHERE burn_job_id = $1 AND state = ANY($2)
         )",
    )
    .bind(id.as_uuid())
    .bind(ACTIVE_ATTEMPT_STATES)
    .fetch_one(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    row.try_get(0).map_err(DbError::Query)
}

// --- burn attempts -------------------------------------------------------------

/// One execution of a burn job, as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct BurnAttemptRecord {
    /// The attempt.
    pub id: BurnAttemptId,
    /// The job it belongs to.
    pub burn_job_id: BurnJobId,
    /// The worker that claimed it.
    pub worker_id: WorkerId,
    /// The drive it runs on.
    pub drive_id: DriveId,
    /// Which attempt this is for the job, starting at 1.
    pub attempt_number: i32,
    /// Where it got to.
    pub state: BurnAttemptState,
    /// Which engine wrote.
    pub engine: String,
    /// That engine's version.
    pub engine_version: String,
    /// A stable failure code, when it failed.
    pub error_code: Option<String>,
    /// Human-readable failure detail, when it failed.
    pub error_detail: Option<String>,
    /// The engine's account of the write, once it has one.
    pub write_report: Option<serde_json::Value>,
    /// The read-back comparison, when one ran.
    pub verify_report: Option<serde_json::Value>,
    /// The disc this attempt produced, if it produced one.
    pub physical_copy_id: Option<uuid::Uuid>,
    /// When it started.
    pub started_at: OffsetDateTime,
    /// When it ended, if it has.
    pub ended_at: Option<OffsetDateTime>,
    /// Highest event sequence held for it.
    pub last_event_sequence: Option<i64>,
}

const BURN_ATTEMPT_SELECT: &str = "
    SELECT a.id, a.burn_job_id, a.worker_id, a.drive_id, a.attempt_number, a.state,
           a.engine, a.engine_version, a.error_code, a.error_detail,
           a.write_report_json, a.verify_report_json, a.started_at, a.ended_at,
           c.id AS physical_copy_id,
           (SELECT max(sequence) FROM burn_events e WHERE e.attempt_id = a.id)
               AS last_event_sequence
    FROM burn_attempts a
    LEFT JOIN physical_copies c ON c.burn_attempt_id = a.id
";

fn burn_attempt_from_row(row: &sqlx::postgres::PgRow) -> Result<BurnAttemptRecord, DbError> {
    let state: String = row.try_get("state").map_err(DbError::Query)?;
    Ok(BurnAttemptRecord {
        id: BurnAttemptId::from_uuid(row.try_get("id").map_err(DbError::Query)?),
        burn_job_id: BurnJobId::from_uuid(row.try_get("burn_job_id").map_err(DbError::Query)?),
        worker_id: WorkerId::from_uuid(row.try_get("worker_id").map_err(DbError::Query)?),
        drive_id: DriveId::from_uuid(row.try_get("drive_id").map_err(DbError::Query)?),
        attempt_number: row.try_get("attempt_number").map_err(DbError::Query)?,
        state: stored_enum("burn_attempts.state", &state)?,
        engine: row.try_get("engine").map_err(DbError::Query)?,
        engine_version: row.try_get("engine_version").map_err(DbError::Query)?,
        error_code: row.try_get("error_code").map_err(DbError::Query)?,
        error_detail: row.try_get("error_detail").map_err(DbError::Query)?,
        write_report: row.try_get("write_report_json").map_err(DbError::Query)?,
        verify_report: row.try_get("verify_report_json").map_err(DbError::Query)?,
        physical_copy_id: row.try_get("physical_copy_id").map_err(DbError::Query)?,
        started_at: row.try_get("started_at").map_err(DbError::Query)?,
        ended_at: row.try_get("ended_at").map_err(DbError::Query)?,
        last_event_sequence: row.try_get("last_event_sequence").map_err(DbError::Query)?,
    })
}

/// Every attempt made at a job, oldest first.
///
/// Oldest first because this is a history: the interesting reading is what was
/// tried, in the order it was tried, and how many discs it cost.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn list_burn_attempts(
    pool: &PgPool,
    burn_job_id: BurnJobId,
) -> Result<Vec<BurnAttemptRecord>, DbError> {
    let sql = format!("{BURN_ATTEMPT_SELECT} WHERE a.burn_job_id = $1 ORDER BY a.attempt_number");
    let rows = sqlx::query(&sql)
        .bind(burn_job_id.as_uuid())
        .fetch_all(pool)
        .await
        .map_err(DbError::Query)?;
    rows.iter().map(burn_attempt_from_row).collect()
}

/// One attempt.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn get_burn_attempt(
    pool: &PgPool,
    attempt_id: BurnAttemptId,
) -> Result<Option<BurnAttemptRecord>, DbError> {
    let sql = format!("{BURN_ATTEMPT_SELECT} WHERE a.id = $1");
    let row = sqlx::query(&sql)
        .bind(attempt_id.as_uuid())
        .fetch_optional(pool)
        .await
        .map_err(DbError::Query)?;
    row.as_ref().map(burn_attempt_from_row).transpose()
}

/// One stored event.
#[derive(Debug, Clone, PartialEq)]
pub struct BurnEventRecord {
    /// Monotonic within the attempt.
    pub sequence: i64,
    /// Event type.
    pub event_type: String,
    /// Stage it relates to.
    pub stage: String,
    /// Progress from 0 to 1, when reported.
    pub progress: Option<f32>,
    /// Stable machine-readable code.
    pub message_code: String,
    /// Structured detail.
    pub data: serde_json::Value,
    /// When the worker observed it.
    pub worker_timestamp: OffsetDateTime,
    /// When the server stored it.
    pub server_received_at: OffsetDateTime,
}

/// Events for an attempt, in sequence order.
///
/// Ascending, and paginated by sequence rather than by identifier: an event
/// timeline is read forwards, and a client following a live burn asks for
/// everything after what it already has.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn list_burn_events(
    pool: &PgPool,
    attempt_id: BurnAttemptId,
    after_sequence: Option<i64>,
    limit: i64,
) -> Result<Vec<BurnEventRecord>, DbError> {
    let rows = sqlx::query(
        "SELECT sequence, event_type, stage, progress_fraction, message_code,
                data_json, worker_timestamp, server_received_at
         FROM burn_events
         WHERE attempt_id = $1 AND ($2::bigint IS NULL OR sequence > $2)
         ORDER BY sequence
         LIMIT $3",
    )
    .bind(attempt_id.as_uuid())
    .bind(after_sequence)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;

    rows.iter()
        .map(|row| {
            Ok(BurnEventRecord {
                sequence: row.try_get("sequence").map_err(DbError::Query)?,
                event_type: row.try_get("event_type").map_err(DbError::Query)?,
                stage: row.try_get("stage").map_err(DbError::Query)?,
                progress: row.try_get("progress_fraction").map_err(DbError::Query)?,
                message_code: row.try_get("message_code").map_err(DbError::Query)?,
                data: row.try_get("data_json").map_err(DbError::Query)?,
                worker_timestamp: row.try_get("worker_timestamp").map_err(DbError::Query)?,
                server_received_at: row.try_get("server_received_at").map_err(DbError::Query)?,
            })
        })
        .collect()
}

// --- artifacts, as burning sees them --------------------------------------------

/// What the catalog knows about an artifact, for deciding whether to burn it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactBurnFacts {
    /// Structural validation outcome.
    pub validation_state: String,
    /// Whether the artifact is under quarantine.
    pub quarantine_state: String,
    /// Whether its payload is scheduled for deletion.
    pub pending_deletion: bool,
    /// Detected container format.
    pub format: String,
    /// Total size of the artifact.
    pub total_bytes: i64,
}

/// Read the facts a burn decision needs about an artifact.
///
/// Separate from the create, and read before it, so the refusal an operator
/// gets names what is wrong with the artifact rather than reporting a
/// constraint failure.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn artifact_burn_facts(
    pool: &PgPool,
    artifact_id: uuid::Uuid,
) -> Result<Option<ArtifactBurnFacts>, DbError> {
    let row = sqlx::query(
        "SELECT validation_state, quarantine_state, pending_deletion, format, total_bytes
         FROM artifacts WHERE id = $1",
    )
    .bind(artifact_id)
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;

    let Some(row) = row else { return Ok(None) };
    Ok(Some(ArtifactBurnFacts {
        validation_state: row.try_get("validation_state").map_err(DbError::Query)?,
        quarantine_state: row.try_get("quarantine_state").map_err(DbError::Query)?,
        pending_deletion: row.try_get("pending_deletion").map_err(DbError::Query)?,
        format: row.try_get("format").map_err(DbError::Query)?,
        total_bytes: row.try_get("total_bytes").map_err(DbError::Query)?,
    }))
}

// --- catalog registration ---------------------------------------------------------

/// One file of an artifact being registered.
#[derive(Debug, Clone, Copy)]
pub struct ComponentRegistration<'a> {
    /// Stable component identity, from the manifest.
    pub component_id: ComponentId,
    /// Portable path inside the artifact.
    pub logical_path: &'a str,
    /// What part it plays.
    pub role: &'a str,
    /// Position within the artifact.
    pub ordinal: i32,
    /// Size in bytes.
    pub length_bytes: i64,
    /// Content digest, lowercase hex.
    pub sha256: &'a str,
    /// Filename as received.
    pub source_filename: Option<&'a str>,
    /// MIME type, when one applies.
    pub media_type: Option<&'a str>,
}

/// An artifact being added to the catalog.
#[derive(Debug, Clone, Copy)]
pub struct ArtifactRegistration<'a> {
    /// Catalog identity, from the manifest.
    pub artifact_id: ArtifactId,
    /// Shape of the artifact.
    pub artifact_kind: &'a str,
    /// Container format.
    pub format: &'a str,
    /// Whether it is an original, a derivative, a dump.
    pub origin: &'a str,
    /// Schema version of the manifest that describes it.
    pub manifest_version: &'a str,
    /// Sum of all component lengths.
    pub total_bytes: i64,
    /// Structural validation outcome.
    pub validation_state: &'a str,
    /// The validation detail, when there is any worth keeping.
    pub validation_summary: Option<&'a serde_json::Value>,
    /// Digest of the component that stands for the artifact, when one does.
    pub primary_sha256: Option<&'a str>,
    /// The files making up the artifact.
    pub components: &'a [ComponentRegistration<'a>],
}

/// What registering an artifact did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegisteredArtifact {
    /// Whether this call created the catalog rows.
    ///
    /// False means an earlier call already did. The caller is told the same
    /// thing either way; this exists so a replay is visible in the logs.
    pub created: bool,
}

/// Add an artifact and its components to the catalog.
///
/// Idempotent by artifact identity, because import completion is: a pipeline
/// that finished storing bytes and then failed to answer must be able to run
/// its final step again. A second registration of the same artifact changes
/// nothing and reports that it changed nothing.
///
/// Reference counts are incremented only when the rows are created, so a
/// replay cannot inflate them and pin objects the garbage collector should be
/// free to take once the artifact is deleted.
///
/// The object rows are inserted first because components reference them, and
/// a component whose bytes the catalog has never heard of is exactly the
/// dangling reference the foreign key exists to prevent.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn register_artifact(
    pool: &PgPool,
    registration: ArtifactRegistration<'_>,
) -> Result<RegisteredArtifact, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;

    // Distinct digests only. One artifact may reference the same bytes twice
    // (a duplicated track, an identical stub file), and the object row is
    // one row per digest by construction.
    let mut digests: Vec<(&str, i64)> = registration
        .components
        .iter()
        .map(|component| (component.sha256, component.length_bytes))
        .collect();
    digests.sort_unstable();
    digests.dedup_by(|a, b| a.0 == b.0);

    for (sha256, size_bytes) in &digests {
        sqlx::query(
            "INSERT INTO cas_objects (sha256, size_bytes)
             VALUES ($1, $2)
             ON CONFLICT (sha256) DO NOTHING",
        )
        .bind(sha256)
        .bind(size_bytes)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    }

    let inserted = sqlx::query(
        "INSERT INTO artifacts
             (id, artifact_kind, format, origin, manifest_version, total_bytes,
              component_count, primary_sha256, validation_state, validation_summary)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(registration.artifact_id.as_uuid())
    .bind(registration.artifact_kind)
    .bind(registration.format)
    .bind(registration.origin)
    .bind(registration.manifest_version)
    .bind(registration.total_bytes)
    .bind(i32::try_from(registration.components.len()).unwrap_or(i32::MAX))
    .bind(registration.primary_sha256)
    .bind(registration.validation_state)
    .bind(registration.validation_summary)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    if inserted.rows_affected() == 0 {
        // Already catalogued. Its components are immutable, so there is
        // nothing to reconcile and nothing to overwrite.
        tx.commit().await.map_err(DbError::Query)?;
        return Ok(RegisteredArtifact { created: false });
    }

    for component in registration.components {
        sqlx::query(
            "INSERT INTO artifact_components
                 (id, artifact_id, logical_path, role, media_type, length_bytes,
                  sha256, ordinal, source_filename)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(component.component_id.as_uuid())
        .bind(registration.artifact_id.as_uuid())
        .bind(component.logical_path)
        .bind(component.role)
        .bind(component.media_type)
        .bind(component.length_bytes)
        .bind(component.sha256)
        .bind(component.ordinal)
        .bind(component.source_filename)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    }

    // One increment per distinct object, matching what was attached. Counting
    // per component would double-count an artifact that references the same
    // bytes twice and pin the object forever.
    for (sha256, _) in &digests {
        sqlx::query(
            "UPDATE cas_objects SET reference_count = reference_count + 1 WHERE sha256 = $1",
        )
        .bind(sha256)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    }

    tx.commit().await.map_err(DbError::Query)?;
    Ok(RegisteredArtifact { created: true })
}

// --- drives ----------------------------------------------------------------------

/// What a worker reports about one drive.
#[derive(Debug, Clone, Copy)]
pub struct DriveReport<'a> {
    /// Worker-local stable alias, such as `/dev/disc-block`.
    ///
    /// The identity, together with the worker. Never a host device node: the
    /// same `/dev/sr0` is a different drive on another machine, and can be a
    /// different drive on the same machine after a reboot.
    pub device_alias: &'a str,
    /// Human-meaningful name an operator chose.
    pub configured_name: &'a str,
    /// Reported vendor.
    pub vendor: Option<&'a str>,
    /// Reported model.
    pub model: Option<&'a str>,
    /// Reported firmware revision.
    pub firmware: Option<&'a str>,
    /// Hash of the drive serial, hashed by the worker.
    ///
    /// The raw serial never leaves the machine that read it: it identifies
    /// hardware an operator may not wish to expose in an export or a support
    /// bundle, and the server only ever needs to recognise it.
    pub serial_hash: Option<&'a str>,
    /// Current drive status.
    pub status: &'a str,
    /// What the drive says it can do.
    pub capabilities: &'a serde_json::Value,
}

/// Record what a worker reports about itself and its drive.
///
/// Idempotent by worker and device alias: a worker restarting reports the same
/// drive and gets the same identifier back, rather than accumulating a new
/// drive row per restart and leaving burn history pointing at drives nobody
/// can find.
///
/// Capabilities are recorded as evidence, not as permission. A drive claiming
/// it can write a profile may still fail to, which is why preflight inspects
/// the medium rather than trusting this.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn record_capabilities(
    pool: &PgPool,
    worker_id: WorkerId,
    software_version: &str,
    worker_capabilities: &serde_json::Value,
    drive: DriveReport<'_>,
) -> Result<DriveId, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;

    sqlx::query(
        "UPDATE workers
         SET software_version = $2, capabilities_json = $3, last_seen_at = now()
         WHERE id = $1",
    )
    .bind(worker_id.as_uuid())
    .bind(software_version)
    .bind(worker_capabilities)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    let drive_id = DriveId::generate();
    let row = sqlx::query(
        "INSERT INTO drives
             (id, worker_id, configured_name, device_alias, vendor, model, firmware,
              serial_hash, capabilities_json, status, last_seen_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, now())
         ON CONFLICT (worker_id, device_alias) DO UPDATE SET
             configured_name = EXCLUDED.configured_name,
             vendor = EXCLUDED.vendor,
             model = EXCLUDED.model,
             firmware = EXCLUDED.firmware,
             serial_hash = EXCLUDED.serial_hash,
             capabilities_json = EXCLUDED.capabilities_json,
             status = EXCLUDED.status,
             last_seen_at = now()
         RETURNING id",
    )
    .bind(drive_id.as_uuid())
    .bind(worker_id.as_uuid())
    .bind(drive.configured_name)
    .bind(drive.device_alias)
    .bind(drive.vendor)
    .bind(drive.model)
    .bind(drive.firmware)
    .bind(drive.serial_hash)
    .bind(drive.capabilities)
    .bind(drive.status)
    .fetch_one(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    let stored: uuid::Uuid = row.try_get("id").map_err(DbError::Query)?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(DriveId::from_uuid(stored))
}

// --- import jobs -----------------------------------------------------------------

/// A lease duration in the width PostgreSQL's interval arithmetic takes.
///
/// Clamped rather than cast: a lease longer than an `int` of seconds is a
/// configuration mistake, and wrapping it would produce a lease in the past.
fn lease_seconds(seconds: i64) -> i32 {
    i32::try_from(seconds).unwrap_or(i32::MAX)
}

/// An import as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportJobRecord {
    /// The job.
    pub id: ImportJobId,
    /// Where the bytes come from.
    pub source_kind: String,
    /// What the source is, in the shape that source kind defines.
    ///
    /// Never contains a secret and never a post-processing command: an import
    /// descriptor that could carry one would turn importing into running code.
    pub source_descriptor: serde_json::Value,
    /// Lifecycle state.
    pub state: ImportState,
    /// Where a retry re-enters, when the job has got far enough to have one.
    pub resume_stage: Option<String>,
    /// The pipeline's own progress record.
    pub checkpoint: Option<serde_json::Value>,
    /// How many bytes are expected, when the source says.
    pub bytes_expected: Option<i64>,
    /// How many have arrived.
    pub bytes_received: i64,
    /// The artifact it produced, once it has.
    pub artifact_id: Option<uuid::Uuid>,
    /// A stable failure code, when it failed.
    pub error_code: Option<String>,
    /// Human-readable failure detail, when it failed.
    pub error_detail: Option<String>,
    /// Who asked for it.
    pub created_by: String,
    /// When it was requested.
    pub created_at: OffsetDateTime,
    /// When it last changed.
    pub updated_at: OffsetDateTime,
    /// When it finished, if it has.
    pub completed_at: Option<OffsetDateTime>,
}

/// The columns every import read returns.
///
/// One list, used by the select and by the insert's `RETURNING`, so the two
/// cannot drift into disagreeing about what an import job looks like.
const IMPORT_JOB_COLUMNS: &str = "
    id, source_kind, source_descriptor_json, state, resume_stage, checkpoint_json,
    bytes_expected, bytes_received, artifact_id, error_code, error_detail,
    created_by, created_at, updated_at, completed_at
";

fn import_job_from_row(row: &sqlx::postgres::PgRow) -> Result<ImportJobRecord, DbError> {
    let state: String = row.try_get("state").map_err(DbError::Query)?;
    Ok(ImportJobRecord {
        id: ImportJobId::from_uuid(row.try_get("id").map_err(DbError::Query)?),
        source_kind: row.try_get("source_kind").map_err(DbError::Query)?,
        source_descriptor: row
            .try_get("source_descriptor_json")
            .map_err(DbError::Query)?,
        state: stored_enum("import_jobs.state", &state)?,
        resume_stage: row.try_get("resume_stage").map_err(DbError::Query)?,
        checkpoint: row.try_get("checkpoint_json").map_err(DbError::Query)?,
        bytes_expected: row.try_get("bytes_expected").map_err(DbError::Query)?,
        bytes_received: row.try_get("bytes_received").map_err(DbError::Query)?,
        artifact_id: row.try_get("artifact_id").map_err(DbError::Query)?,
        error_code: row.try_get("error_code").map_err(DbError::Query)?,
        error_detail: row.try_get("error_detail").map_err(DbError::Query)?,
        created_by: row.try_get("created_by").map_err(DbError::Query)?,
        created_at: row.try_get("created_at").map_err(DbError::Query)?,
        updated_at: row.try_get("updated_at").map_err(DbError::Query)?,
        completed_at: row.try_get("completed_at").map_err(DbError::Query)?,
    })
}

/// What an operator asked to import.
#[derive(Debug, Clone, Copy)]
pub struct NewImportJob<'a> {
    /// The job's identifier.
    ///
    /// Chosen by the caller rather than here, because the staging area is
    /// named after it and the bytes may land before the row does.
    pub id: ImportJobId,
    /// Where the bytes come from.
    pub source_kind: &'a str,
    /// What the source is.
    pub source_descriptor: &'a serde_json::Value,
    /// The state to record. `requested` for work not yet begun, `staged` when
    /// the bytes are already in the staging area.
    pub state: ImportState,
    /// How many bytes are expected, when the source says.
    pub bytes_expected: Option<i64>,
    /// How many have already arrived.
    pub bytes_received: i64,
    /// Who asked.
    pub created_by: &'a str,
}

/// Record an import.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn create_import_job(
    pool: &PgPool,
    new: NewImportJob<'_>,
) -> Result<ImportJobRecord, DbError> {
    // RETURNING rather than an insert followed by a select: a select in the
    // same statement would read the snapshot from before the insert and find
    // nothing.
    let row = sqlx::query(&format!(
        "INSERT INTO import_jobs
             (id, source_kind, source_descriptor_json, state, bytes_expected,
              bytes_received, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING {IMPORT_JOB_COLUMNS}"
    ))
    .bind(new.id.as_uuid())
    .bind(new.source_kind)
    .bind(new.source_descriptor)
    .bind(new.state.as_str())
    .bind(new.bytes_expected)
    .bind(new.bytes_received)
    .bind(new.created_by)
    .fetch_one(pool)
    .await
    .map_err(DbError::Query)?;

    import_job_from_row(&row)
}

/// One import.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn get_import_job(
    pool: &PgPool,
    id: ImportJobId,
) -> Result<Option<ImportJobRecord>, DbError> {
    let row = sqlx::query(&format!(
        "SELECT {IMPORT_JOB_COLUMNS} FROM import_jobs WHERE id = $1"
    ))
    .bind(id.as_uuid())
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    row.as_ref().map(import_job_from_row).transpose()
}

/// List imports, newest first.
///
/// Same ordering as the burn queue, for the same reason: the interesting
/// import is the one that just started.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn list_import_jobs(
    pool: &PgPool,
    state: Option<ImportState>,
    limit: i64,
    before: Option<uuid::Uuid>,
) -> Result<Vec<ImportJobRecord>, DbError> {
    let rows = sqlx::query(&format!(
        "SELECT {IMPORT_JOB_COLUMNS} FROM import_jobs
         WHERE ($1::text IS NULL OR state = $1)
           AND ($2::uuid IS NULL OR id < $2)
         ORDER BY id DESC
         LIMIT $3"
    ))
    .bind(state.map(|state| state.as_str()))
    .bind(before)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;

    rows.iter().map(import_job_from_row).collect()
}

/// Take the next import that is waiting to be worked on.
///
/// The same claim discipline as burn jobs: `FOR UPDATE SKIP LOCKED` so two
/// server processes step over each other's rows rather than racing, and a
/// lease so a process that dies does not strand the work forever.
///
/// A job whose lease has expired is claimable again. That is the whole point
/// of the lease: an import interrupted by a restart resumes from its
/// checkpoint instead of waiting for somebody to notice.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn claim_next_import_job(
    pool: &PgPool,
    lease_owner: &str,
    lease_duration_seconds: i64,
) -> Result<Option<ImportJobRecord>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;

    // Every state that is still going, not merely the ones that have not
    // started. A job that reached hashing and whose lease then lapsed is a
    // process that died mid-import, and picking it up again from its
    // checkpoint is exactly what the lease exists to allow.
    let row = sqlx::query(
        "SELECT id FROM import_jobs
         WHERE state IN ('requested', 'acquiring', 'staged', 'hashing', 'inspecting',
                         'registering', 'failed_retryable')
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

    sqlx::query(
        "UPDATE import_jobs
         SET lease_owner = $2,
             lease_expires_at = now() + ($3::int * interval '1 second')
         WHERE id = $1",
    )
    .bind(id)
    .bind(lease_owner)
    .bind(lease_seconds(lease_duration_seconds))
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    let row = sqlx::query(&format!(
        "SELECT {IMPORT_JOB_COLUMNS} FROM import_jobs WHERE id = $1"
    ))
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    tx.commit().await.map_err(DbError::Query)?;
    import_job_from_row(&row).map(Some)
}

/// Record how far an import has got, and hold the lease a little longer.
///
/// The checkpoint is what a resumed import re-enters at, so it is written on
/// every stage rather than at the end: a process that dies mid-import should
/// cost the stage it was in, not the whole transfer.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn record_import_progress(
    pool: &PgPool,
    id: ImportJobId,
    state: ImportState,
    checkpoint: Option<&serde_json::Value>,
    resume_stage: Option<&str>,
    bytes_received: Option<i64>,
    lease_duration_seconds: i64,
) -> Result<bool, DbError> {
    let result = sqlx::query(
        "UPDATE import_jobs
         SET state = $2,
             checkpoint_json = coalesce($3, checkpoint_json),
             resume_stage = coalesce($4, resume_stage),
             bytes_received = coalesce($5, bytes_received),
             lease_expires_at = now() + ($6::int * interval '1 second')
         WHERE id = $1",
    )
    .bind(id.as_uuid())
    .bind(state.as_str())
    .bind(checkpoint)
    .bind(resume_stage)
    .bind(bytes_received)
    .bind(lease_seconds(lease_duration_seconds))
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(result.rows_affected() > 0)
}

/// Record that an import produced an artifact.
///
/// The artifact and the terminal state are written together, because the
/// schema refuses a completed import without one and a caller that set them
/// separately would be briefly claiming something untrue.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn complete_import_job(
    pool: &PgPool,
    id: ImportJobId,
    artifact_id: uuid::Uuid,
) -> Result<bool, DbError> {
    let result = sqlx::query(
        "UPDATE import_jobs
         SET state = 'complete', artifact_id = $2, completed_at = now(),
             lease_owner = NULL, lease_expires_at = NULL,
             error_code = NULL, error_detail = NULL
         WHERE id = $1",
    )
    .bind(id.as_uuid())
    .bind(artifact_id)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(result.rows_affected() > 0)
}

/// Record that an import failed.
///
/// Retryable and terminal are different states because they mean different
/// things to an operator: one will be picked up again, the other needs them.
///
/// A retryable failure is held off for `retry_after_seconds` before it becomes
/// claimable again. The delay is written as a lease, because the claim already
/// refuses work whose lease has not lapsed and an owner named `retry-backoff`
/// makes it obvious in the table what is holding the job. Without it an import
/// that fails in milliseconds would spin as fast as the database can answer.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn fail_import_job(
    pool: &PgPool,
    id: ImportJobId,
    retryable: bool,
    error_code: &str,
    error_detail: &str,
    retry_after_seconds: i64,
) -> Result<bool, DbError> {
    let state = if retryable {
        ImportState::FailedRetryable
    } else {
        ImportState::FailedTerminal
    };
    let result = sqlx::query(
        "UPDATE import_jobs
         SET state = $2, error_code = $3, error_detail = $4,
             lease_owner = CASE WHEN $5 THEN 'retry-backoff' ELSE NULL END,
             lease_expires_at = CASE
                 WHEN $5 THEN now() + ($6::int * interval '1 second')
                 ELSE NULL
             END,
             completed_at = CASE WHEN $2 = 'failed_terminal' THEN now() ELSE completed_at END
         WHERE id = $1",
    )
    .bind(id.as_uuid())
    .bind(state.as_str())
    .bind(error_code)
    .bind(error_detail)
    .bind(retryable)
    .bind(lease_seconds(retry_after_seconds.max(1)))
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(result.rows_affected() > 0)
}

/// Why an import command was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportCommandOutcome {
    /// No such import.
    NotFound,
    /// The import has already finished.
    AlreadyFinished {
        /// The state it finished in.
        state: ImportState,
    },
    /// The import is running, and this command needs it not to be.
    Running {
        /// The state it is in.
        state: ImportState,
    },
}

/// Stop an import at the operator's request.
///
/// Cancelling is honoured whatever stage the import is in: unlike a burn,
/// nothing physical is consumed, and the staged bytes are the caller's to
/// clean up afterwards.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn cancel_import_job(
    pool: &PgPool,
    id: ImportJobId,
) -> Result<Result<ImportState, ImportCommandOutcome>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;

    let row = sqlx::query("SELECT state FROM import_jobs WHERE id = $1 FOR UPDATE")
        .bind(id.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(DbError::Query)?;

    let Some(row) = row else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(ImportCommandOutcome::NotFound));
    };
    let state: String = row.try_get("state").map_err(DbError::Query)?;
    let state: ImportState = stored_enum("import_jobs.state", &state)?;

    if state == ImportState::Canceled {
        tx.commit().await.map_err(DbError::Query)?;
        return Ok(Ok(ImportState::Canceled));
    }
    if state.is_terminal() {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(ImportCommandOutcome::AlreadyFinished { state }));
    }

    sqlx::query(
        "UPDATE import_jobs
         SET state = 'canceled', completed_at = now(),
             lease_owner = NULL, lease_expires_at = NULL
         WHERE id = $1",
    )
    .bind(id.as_uuid())
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    tx.commit().await.map_err(DbError::Query)?;
    Ok(Ok(ImportState::Canceled))
}

/// Put a failed or cancelled import back in the queue.
///
/// The checkpoint is left exactly as it is, so the retry re-enters at the
/// stage the pipeline recorded rather than starting the transfer again.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn retry_import_job(
    pool: &PgPool,
    id: ImportJobId,
) -> Result<Result<ImportState, ImportCommandOutcome>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;

    let row = sqlx::query("SELECT state FROM import_jobs WHERE id = $1 FOR UPDATE")
        .bind(id.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(DbError::Query)?;

    let Some(row) = row else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(ImportCommandOutcome::NotFound));
    };
    let state: String = row.try_get("state").map_err(DbError::Query)?;
    let state: ImportState = stored_enum("import_jobs.state", &state)?;

    match state {
        // What a retry is for.
        ImportState::FailedRetryable
        | ImportState::FailedTerminal
        | ImportState::Canceled
        | ImportState::Quarantined => {}
        // Already finished successfully: importing the same bytes again is a
        // new import, not a retry of this one.
        ImportState::Complete => {
            tx.rollback().await.map_err(DbError::Query)?;
            return Ok(Err(ImportCommandOutcome::AlreadyFinished { state }));
        }
        running => {
            tx.rollback().await.map_err(DbError::Query)?;
            return Ok(Err(ImportCommandOutcome::Running { state: running }));
        }
    }

    // Back to the stage the checkpoint says is safe to re-enter, or to the
    // beginning when there is none.
    sqlx::query(
        "UPDATE import_jobs
         SET state = coalesce(resume_stage, 'requested'),
             error_code = NULL, error_detail = NULL, completed_at = NULL,
             lease_owner = NULL, lease_expires_at = NULL
         WHERE id = $1",
    )
    .bind(id.as_uuid())
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    let row = sqlx::query("SELECT state FROM import_jobs WHERE id = $1")
        .bind(id.as_uuid())
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    let state: String = row.try_get("state").map_err(DbError::Query)?;
    let state: ImportState = stored_enum("import_jobs.state", &state)?;

    tx.commit().await.map_err(DbError::Query)?;
    Ok(Ok(state))
}

// --- physical copies -------------------------------------------------------------

/// A disc that exists, as the inventory records it.
#[derive(Debug, Clone, PartialEq)]
pub struct PhysicalCopyRecord {
    /// The disc.
    pub id: PhysicalCopyId,
    /// The catalog disc it is a copy of.
    pub disc_id: uuid::Uuid,
    /// The artifact written to it.
    pub artifact_id: uuid::Uuid,
    /// The attempt that produced it. The evidence of how it was made.
    pub burn_attempt_id: uuid::Uuid,
    /// Its condition.
    pub status: PhysicalCopyStatus,
    /// Media profile of the medium.
    pub media_profile: String,
    /// Manufacturer identifier, when the drive reported one.
    pub manufacturer_id: Option<String>,
    /// Media serial, when the drive reported one.
    pub media_serial: Option<String>,
    /// What is written on the disc itself.
    pub label: Option<String>,
    /// Where it is kept.
    pub storage_location: Option<String>,
    /// What verification was performed when it was burned.
    pub verification_level: String,
    /// How that turned out.
    pub verification_result: String,
    /// Operator's notes.
    pub notes: Option<String>,
    /// When it was burned.
    pub created_at: OffsetDateTime,
    /// When the record last changed.
    pub updated_at: OffsetDateTime,
    /// When it was last read back.
    pub last_checked_at: Option<OffsetDateTime>,
    /// How many checks have been recorded.
    pub check_count: i64,
}

const PHYSICAL_COPY_SELECT: &str = "
    SELECT c.id, c.disc_id, c.artifact_id, c.burn_attempt_id, c.status, c.media_profile,
           c.manufacturer_id, c.media_serial, c.label, c.storage_location,
           c.verification_level, c.verification_result, c.notes,
           c.created_at, c.updated_at, c.last_checked_at,
           coalesce(counted.checks, 0) AS check_count
    FROM physical_copies c
    LEFT JOIN LATERAL (
        SELECT count(*) AS checks
        FROM physical_copy_checks k
        WHERE k.physical_copy_id = c.id
    ) counted ON true
";

fn physical_copy_from_row(row: &sqlx::postgres::PgRow) -> Result<PhysicalCopyRecord, DbError> {
    let status: String = row.try_get("status").map_err(DbError::Query)?;
    Ok(PhysicalCopyRecord {
        id: PhysicalCopyId::from_uuid(row.try_get("id").map_err(DbError::Query)?),
        disc_id: row.try_get("disc_id").map_err(DbError::Query)?,
        artifact_id: row.try_get("artifact_id").map_err(DbError::Query)?,
        burn_attempt_id: row.try_get("burn_attempt_id").map_err(DbError::Query)?,
        status: stored_enum("physical_copies.status", &status)?,
        media_profile: row.try_get("media_profile").map_err(DbError::Query)?,
        manufacturer_id: row.try_get("manufacturer_id").map_err(DbError::Query)?,
        media_serial: row.try_get("media_serial").map_err(DbError::Query)?,
        label: row.try_get("label").map_err(DbError::Query)?,
        storage_location: row.try_get("storage_location").map_err(DbError::Query)?,
        verification_level: row.try_get("verification_level").map_err(DbError::Query)?,
        verification_result: row.try_get("verification_result").map_err(DbError::Query)?,
        notes: row.try_get("notes").map_err(DbError::Query)?,
        created_at: row.try_get("created_at").map_err(DbError::Query)?,
        updated_at: row.try_get("updated_at").map_err(DbError::Query)?,
        last_checked_at: row.try_get("last_checked_at").map_err(DbError::Query)?,
        check_count: row.try_get("check_count").map_err(DbError::Query)?,
    })
}

/// Which discs a listing should include.
#[derive(Debug, Clone, Copy, Default)]
pub struct PhysicalCopyFilter {
    /// Only copies of this catalog disc.
    pub disc_id: Option<uuid::Uuid>,
    /// Only copies of this artifact.
    pub artifact_id: Option<uuid::Uuid>,
    /// Only discs in this condition.
    pub status: Option<PhysicalCopyStatus>,
}

/// List physical copies, newest first.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn list_physical_copies(
    pool: &PgPool,
    filter: PhysicalCopyFilter,
    limit: i64,
    before: Option<uuid::Uuid>,
) -> Result<Vec<PhysicalCopyRecord>, DbError> {
    let rows = sqlx::query(&format!(
        "{PHYSICAL_COPY_SELECT}
         WHERE ($1::uuid IS NULL OR c.disc_id = $1)
           AND ($2::uuid IS NULL OR c.artifact_id = $2)
           AND ($3::text IS NULL OR c.status = $3)
           AND ($4::uuid IS NULL OR c.id < $4)
         ORDER BY c.id DESC
         LIMIT $5"
    ))
    .bind(filter.disc_id)
    .bind(filter.artifact_id)
    .bind(filter.status.map(|status| status.as_str()))
    .bind(before)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;

    rows.iter().map(physical_copy_from_row).collect()
}

/// One physical copy.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn get_physical_copy(
    pool: &PgPool,
    id: PhysicalCopyId,
) -> Result<Option<PhysicalCopyRecord>, DbError> {
    let row = sqlx::query(&format!("{PHYSICAL_COPY_SELECT} WHERE c.id = $1"))
        .bind(id.as_uuid())
        .fetch_optional(pool)
        .await
        .map_err(DbError::Query)?;
    row.as_ref().map(physical_copy_from_row).transpose()
}

/// What an operator may change about a disc.
///
/// Every field is optional and absent means "leave it alone", so a client
/// editing a label cannot blank a storage location it never saw.
#[derive(Debug, Clone, Copy, Default)]
pub struct PhysicalCopyEdit<'a> {
    /// What is written on the disc.
    pub label: Option<&'a str>,
    /// Where it is kept.
    pub storage_location: Option<&'a str>,
    /// Operator's notes.
    pub notes: Option<&'a str>,
    /// Its condition, when the operator is recording a change to it.
    pub status: Option<PhysicalCopyStatus>,
}

/// Why a disc could not be updated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalCopyOutcome {
    /// No such disc.
    NotFound,
    /// The disc has been destroyed and accepts nothing further.
    Destroyed,
    /// That condition cannot be set by hand.
    ConditionRefused {
        /// What was asked for.
        requested: PhysicalCopyStatus,
    },
}

/// Record what an operator knows about a disc.
///
/// The burn that produced it is not editable; its condition and its
/// whereabouts are. Discs rot, get scratched, get lost and get thrown away,
/// and none of that changes what was written to them.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure, or [`DbError::Enum`] if the
/// stored condition is not one this build understands.
pub async fn update_physical_copy(
    pool: &PgPool,
    id: PhysicalCopyId,
    edit: PhysicalCopyEdit<'_>,
) -> Result<Result<PhysicalCopyStatus, PhysicalCopyOutcome>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;

    let row = sqlx::query("SELECT status FROM physical_copies WHERE id = $1 FOR UPDATE")
        .bind(id.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(DbError::Query)?;

    let Some(row) = row else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(PhysicalCopyOutcome::NotFound));
    };
    let current: String = row.try_get("status").map_err(DbError::Query)?;
    let current: PhysicalCopyStatus = stored_enum("physical_copies.status", &current)?;

    // Checked before the edit is even considered. A destroyed disc has no
    // condition, no whereabouts and no notes worth recording, because it does
    // not exist; only its history does.
    if current.is_terminal() {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(PhysicalCopyOutcome::Destroyed));
    }

    if let Some(requested) = edit.status
        && requested != current
        && !current.accepts_condition(requested)
    {
        // `verified` and `verification_failed` describe what a read-back
        // found. Only a recorded check may write them.
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(PhysicalCopyOutcome::ConditionRefused { requested }));
    }

    let next = edit.status.unwrap_or(current);
    sqlx::query(
        "UPDATE physical_copies
         SET label = coalesce($2, label),
             storage_location = coalesce($3, storage_location),
             notes = coalesce($4, notes),
             status = $5
         WHERE id = $1",
    )
    .bind(id.as_uuid())
    .bind(edit.label)
    .bind(edit.storage_location)
    .bind(edit.notes)
    .bind(next.as_str())
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    tx.commit().await.map_err(DbError::Query)?;
    Ok(Ok(next))
}

/// One check performed on a disc.
#[derive(Debug, Clone, PartialEq)]
pub struct PhysicalCopyCheckRecord {
    /// The check.
    pub id: uuid::Uuid,
    /// What was done.
    pub method: String,
    /// What was found.
    pub result: String,
    /// The digest read back, when the method produces one.
    pub observed_sha256: Option<String>,
    /// How much was read.
    pub bytes_read: Option<i64>,
    /// Which drive read it.
    pub checked_with: Option<String>,
    /// Operator's notes.
    pub notes: Option<String>,
    /// Who checked.
    pub checked_by: String,
    /// When.
    pub checked_at: OffsetDateTime,
}

/// What a check found.
#[derive(Debug, Clone, Copy)]
pub struct NewPhysicalCopyCheck<'a> {
    /// What was done.
    pub method: &'a str,
    /// What was found.
    pub result: &'a str,
    /// The digest read back, when there is one.
    pub observed_sha256: Option<&'a str>,
    /// How much was read.
    pub bytes_read: Option<i64>,
    /// Which drive read it.
    pub checked_with: Option<&'a str>,
    /// Operator's notes.
    pub notes: Option<&'a str>,
    /// Who checked.
    pub checked_by: &'a str,
}

/// Record a check and update the disc's condition to match.
///
/// The two happen together because they are one fact. A check that said the
/// disc failed, next to a record still calling it verified, is exactly the
/// inconsistency the inventory exists to prevent, and `verified` is a word
/// only this path may write.
///
/// A `partial` or `not_performed` result leaves the condition alone: it
/// establishes neither that the disc is good nor that it is bad, and
/// overwriting a known condition with an inconclusive one loses information.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn record_physical_copy_check(
    pool: &PgPool,
    id: PhysicalCopyId,
    check: NewPhysicalCopyCheck<'_>,
) -> Result<Result<PhysicalCopyStatus, PhysicalCopyOutcome>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;

    let row = sqlx::query("SELECT status FROM physical_copies WHERE id = $1 FOR UPDATE")
        .bind(id.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(DbError::Query)?;

    let Some(row) = row else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(PhysicalCopyOutcome::NotFound));
    };
    let current: String = row.try_get("status").map_err(DbError::Query)?;
    let current: PhysicalCopyStatus = stored_enum("physical_copies.status", &current)?;

    if current.is_terminal() {
        // Nothing to read: the disc is gone.
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(PhysicalCopyOutcome::Destroyed));
    }

    sqlx::query(
        "INSERT INTO physical_copy_checks
             (id, physical_copy_id, method, result, observed_sha256, bytes_read,
              checked_with, notes, checked_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(id.as_uuid())
    .bind(check.method)
    .bind(check.result)
    .bind(check.observed_sha256)
    .bind(check.bytes_read)
    .bind(check.checked_with)
    .bind(check.notes)
    .bind(check.checked_by)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;

    let next = match check.result {
        "passed" => PhysicalCopyStatus::after_check(true),
        "failed" => PhysicalCopyStatus::after_check(false),
        // Inconclusive. Overwriting a known condition with it would lose
        // information rather than add any.
        _ => current,
    };

    sqlx::query("UPDATE physical_copies SET status = $2, last_checked_at = now() WHERE id = $1")
        .bind(id.as_uuid())
        .bind(next.as_str())
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;

    tx.commit().await.map_err(DbError::Query)?;
    Ok(Ok(next))
}

/// Every check recorded for a disc, newest first.
///
/// Newest first because the question is nearly always "is it still good?",
/// and the answer is the most recent check.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn list_physical_copy_checks(
    pool: &PgPool,
    id: PhysicalCopyId,
    limit: i64,
) -> Result<Vec<PhysicalCopyCheckRecord>, DbError> {
    let rows = sqlx::query(
        "SELECT id, method, result, observed_sha256, bytes_read, checked_with,
                notes, checked_by, checked_at
         FROM physical_copy_checks
         WHERE physical_copy_id = $1
         ORDER BY checked_at DESC, id DESC
         LIMIT $2",
    )
    .bind(id.as_uuid())
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;

    rows.iter()
        .map(|row| {
            Ok(PhysicalCopyCheckRecord {
                id: row.try_get("id").map_err(DbError::Query)?,
                method: row.try_get("method").map_err(DbError::Query)?,
                result: row.try_get("result").map_err(DbError::Query)?,
                observed_sha256: row.try_get("observed_sha256").map_err(DbError::Query)?,
                bytes_read: row.try_get("bytes_read").map_err(DbError::Query)?,
                checked_with: row.try_get("checked_with").map_err(DbError::Query)?,
                notes: row.try_get("notes").map_err(DbError::Query)?,
                checked_by: row.try_get("checked_by").map_err(DbError::Query)?,
                checked_at: row.try_get("checked_at").map_err(DbError::Query)?,
            })
        })
        .collect()
}

// --- catalog ---------------------------------------------------------------------

/// A work, as the catalog records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleRecord {
    /// The title.
    pub id: TitleId,
    /// What kind of work it is.
    pub kind: String,
    /// What it is called.
    pub display_title: String,
    /// What it sorts as.
    pub sort_title: String,
    /// When it was released.
    pub release_year: Option<i16>,
    /// How many editions it has.
    pub edition_count: i64,
    /// When it was catalogued.
    pub created_at: OffsetDateTime,
}

/// A particular release of a title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditionRecord {
    /// The edition.
    pub id: EditionId,
    /// The title it belongs to.
    pub title_id: TitleId,
    /// What this release is called.
    pub display_name: String,
    /// Who published it.
    pub publisher: Option<String>,
    /// Which region it was sold in.
    pub region: Option<String>,
    /// How many disc sets it has.
    pub set_count: i64,
    /// When it was catalogued.
    pub created_at: OffsetDateTime,
}

/// The discs an edition shipped as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscSetRecord {
    /// The set.
    pub id: DiscSetId,
    /// The edition it belongs to.
    pub edition_id: EditionId,
    /// What the set is called.
    pub name: String,
    /// What shape of set it is.
    pub set_kind: String,
    /// How many discs it should contain.
    pub disc_count_expected: Option<i16>,
    /// How many are catalogued.
    pub disc_count: i64,
    /// When it was catalogued.
    pub created_at: OffsetDateTime,
}

/// One disc within a set. The canonical object of the whole system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscRecord {
    /// The disc.
    pub id: DiscId,
    /// The set it belongs to.
    pub disc_set_id: DiscSetId,
    /// Which disc of the set it is.
    pub sequence_number: i16,
    /// What it is called, when it has a name of its own.
    pub display_name: Option<String>,
    /// What kind of medium it is.
    pub media_family: String,
    /// Which region it plays in.
    pub region: Option<String>,
    /// The volume label read from it.
    pub volume_label: Option<String>,
    /// What reproduction is expected to achieve.
    pub compatibility_claim: String,
    /// How many artifacts are linked to it.
    pub artifact_count: i64,
    /// How many physical copies of it exist.
    pub copy_count: i64,
    /// When it was catalogued.
    pub created_at: OffsetDateTime,
}

/// A new title.
#[derive(Debug, Clone, Copy)]
pub struct NewTitle<'a> {
    /// What kind of work it is.
    pub kind: &'a str,
    /// What it is called.
    pub display_title: &'a str,
    /// What it sorts as.
    pub sort_title: &'a str,
    /// When it was released.
    pub release_year: Option<i16>,
}

/// Record a title.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn create_title(pool: &PgPool, new: NewTitle<'_>) -> Result<TitleRecord, DbError> {
    let id = TitleId::generate();
    sqlx::query(
        "INSERT INTO titles (id, kind, display_title, sort_title, release_year)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id.as_uuid())
    .bind(new.kind)
    .bind(new.display_title)
    .bind(new.sort_title)
    .bind(new.release_year)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;

    get_title(pool, id).await?.ok_or_else(|| DbError::Enum {
        column: "titles.id",
        value: id.to_string(),
    })
}

const TITLE_SELECT: &str = "
    SELECT t.id, t.kind, t.display_title, t.sort_title, t.release_year, t.created_at,
           coalesce(counted.editions, 0) AS edition_count
    FROM titles t
    LEFT JOIN LATERAL (
        SELECT count(*) AS editions FROM editions e WHERE e.title_id = t.id
    ) counted ON true
";

fn title_from_row(row: &sqlx::postgres::PgRow) -> Result<TitleRecord, DbError> {
    Ok(TitleRecord {
        id: TitleId::from_uuid(row.try_get("id").map_err(DbError::Query)?),
        kind: row.try_get("kind").map_err(DbError::Query)?,
        display_title: row.try_get("display_title").map_err(DbError::Query)?,
        sort_title: row.try_get("sort_title").map_err(DbError::Query)?,
        release_year: row.try_get("release_year").map_err(DbError::Query)?,
        edition_count: row.try_get("edition_count").map_err(DbError::Query)?,
        created_at: row.try_get("created_at").map_err(DbError::Query)?,
    })
}

/// One title.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn get_title(pool: &PgPool, id: TitleId) -> Result<Option<TitleRecord>, DbError> {
    let row = sqlx::query(&format!("{TITLE_SELECT} WHERE t.id = $1"))
        .bind(id.as_uuid())
        .fetch_optional(pool)
        .await
        .map_err(DbError::Query)?;
    row.as_ref().map(title_from_row).transpose()
}

/// List titles in sort order.
///
/// Sorted by `sort_title` rather than by creation, because a catalog is read
/// by somebody looking for a work they can name, not by recency.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn list_titles(
    pool: &PgPool,
    search: Option<&str>,
    limit: i64,
    after: Option<&str>,
) -> Result<Vec<TitleRecord>, DbError> {
    let rows = sqlx::query(&format!(
        "{TITLE_SELECT}
         WHERE ($1::text IS NULL OR t.display_title ILIKE '%' || $1 || '%')
           AND ($2::text IS NULL OR (t.sort_title, t.id::text) > ($2, ''))
         ORDER BY t.sort_title, t.id
         LIMIT $3"
    ))
    .bind(search)
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;

    rows.iter().map(title_from_row).collect()
}

/// A new edition.
#[derive(Debug, Clone, Copy)]
pub struct NewEdition<'a> {
    /// The title it belongs to.
    pub title_id: TitleId,
    /// What this release is called.
    pub display_name: &'a str,
    /// Who published it.
    pub publisher: Option<&'a str>,
    /// Which region it was sold in.
    pub region: Option<&'a str>,
}

/// Why a catalog row could not be created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogOutcome {
    /// The parent named does not exist.
    ParentMissing,
    /// A disc with that sequence number is already in the set.
    ///
    /// Reported precisely because it is an ordinary mistake with an obvious
    /// fix, and because the alternative (a second disc two) would make the
    /// set ambiguous forever.
    SequenceTaken,
}

/// Record an edition.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn create_edition(
    pool: &PgPool,
    new: NewEdition<'_>,
) -> Result<Result<EditionRecord, CatalogOutcome>, DbError> {
    let id = EditionId::generate();
    let inserted = sqlx::query(
        "INSERT INTO editions (id, title_id, display_name, publisher, region)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id.as_uuid())
    .bind(new.title_id.as_uuid())
    .bind(new.display_name)
    .bind(new.publisher)
    .bind(new.region)
    .execute(pool)
    .await;

    if let Err(error) = inserted {
        if is_foreign_key_violation(&error) {
            return Ok(Err(CatalogOutcome::ParentMissing));
        }
        return Err(DbError::Query(error));
    }

    let edition = get_edition(pool, id).await?.ok_or_else(|| DbError::Enum {
        column: "editions.id",
        value: id.to_string(),
    })?;
    Ok(Ok(edition))
}

const EDITION_SELECT: &str = "
    SELECT e.id, e.title_id, e.display_name, e.publisher, e.region, e.created_at,
           coalesce(counted.sets, 0) AS set_count
    FROM editions e
    LEFT JOIN LATERAL (
        SELECT count(*) AS sets FROM disc_sets s WHERE s.edition_id = e.id
    ) counted ON true
";

fn edition_from_row(row: &sqlx::postgres::PgRow) -> Result<EditionRecord, DbError> {
    Ok(EditionRecord {
        id: EditionId::from_uuid(row.try_get("id").map_err(DbError::Query)?),
        title_id: TitleId::from_uuid(row.try_get("title_id").map_err(DbError::Query)?),
        display_name: row.try_get("display_name").map_err(DbError::Query)?,
        publisher: row.try_get("publisher").map_err(DbError::Query)?,
        region: row.try_get("region").map_err(DbError::Query)?,
        set_count: row.try_get("set_count").map_err(DbError::Query)?,
        created_at: row.try_get("created_at").map_err(DbError::Query)?,
    })
}

/// One edition.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn get_edition(pool: &PgPool, id: EditionId) -> Result<Option<EditionRecord>, DbError> {
    let row = sqlx::query(&format!("{EDITION_SELECT} WHERE e.id = $1"))
        .bind(id.as_uuid())
        .fetch_optional(pool)
        .await
        .map_err(DbError::Query)?;
    row.as_ref().map(edition_from_row).transpose()
}

/// Every edition of a title.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn list_editions(
    pool: &PgPool,
    title_id: TitleId,
) -> Result<Vec<EditionRecord>, DbError> {
    let rows = sqlx::query(&format!(
        "{EDITION_SELECT} WHERE e.title_id = $1 ORDER BY e.display_name, e.id"
    ))
    .bind(title_id.as_uuid())
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter().map(edition_from_row).collect()
}

/// A new disc set.
#[derive(Debug, Clone, Copy)]
pub struct NewDiscSet<'a> {
    /// The edition it belongs to.
    pub edition_id: EditionId,
    /// What the set is called.
    pub name: &'a str,
    /// What shape of set it is.
    pub set_kind: &'a str,
    /// How many discs it should contain.
    pub disc_count_expected: Option<i16>,
}

/// Record a disc set.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn create_disc_set(
    pool: &PgPool,
    new: NewDiscSet<'_>,
) -> Result<Result<DiscSetRecord, CatalogOutcome>, DbError> {
    let id = DiscSetId::generate();
    let inserted = sqlx::query(
        "INSERT INTO disc_sets (id, edition_id, name, set_kind, disc_count_expected)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id.as_uuid())
    .bind(new.edition_id.as_uuid())
    .bind(new.name)
    .bind(new.set_kind)
    .bind(new.disc_count_expected)
    .execute(pool)
    .await;

    if let Err(error) = inserted {
        if is_foreign_key_violation(&error) {
            return Ok(Err(CatalogOutcome::ParentMissing));
        }
        return Err(DbError::Query(error));
    }

    let set = get_disc_set(pool, id).await?.ok_or_else(|| DbError::Enum {
        column: "disc_sets.id",
        value: id.to_string(),
    })?;
    Ok(Ok(set))
}

const DISC_SET_SELECT: &str = "
    SELECT s.id, s.edition_id, s.name, s.set_kind, s.disc_count_expected, s.created_at,
           coalesce(counted.discs, 0) AS disc_count
    FROM disc_sets s
    LEFT JOIN LATERAL (
        SELECT count(*) AS discs FROM discs d WHERE d.disc_set_id = s.id
    ) counted ON true
";

fn disc_set_from_row(row: &sqlx::postgres::PgRow) -> Result<DiscSetRecord, DbError> {
    Ok(DiscSetRecord {
        id: DiscSetId::from_uuid(row.try_get("id").map_err(DbError::Query)?),
        edition_id: EditionId::from_uuid(row.try_get("edition_id").map_err(DbError::Query)?),
        name: row.try_get("name").map_err(DbError::Query)?,
        set_kind: row.try_get("set_kind").map_err(DbError::Query)?,
        disc_count_expected: row.try_get("disc_count_expected").map_err(DbError::Query)?,
        disc_count: row.try_get("disc_count").map_err(DbError::Query)?,
        created_at: row.try_get("created_at").map_err(DbError::Query)?,
    })
}

/// One disc set.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn get_disc_set(pool: &PgPool, id: DiscSetId) -> Result<Option<DiscSetRecord>, DbError> {
    let row = sqlx::query(&format!("{DISC_SET_SELECT} WHERE s.id = $1"))
        .bind(id.as_uuid())
        .fetch_optional(pool)
        .await
        .map_err(DbError::Query)?;
    row.as_ref().map(disc_set_from_row).transpose()
}

/// Every disc set in an edition.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn list_disc_sets(
    pool: &PgPool,
    edition_id: EditionId,
) -> Result<Vec<DiscSetRecord>, DbError> {
    let rows = sqlx::query(&format!(
        "{DISC_SET_SELECT} WHERE s.edition_id = $1 ORDER BY s.name, s.id"
    ))
    .bind(edition_id.as_uuid())
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter().map(disc_set_from_row).collect()
}

/// A new disc.
#[derive(Debug, Clone, Copy)]
pub struct NewDisc<'a> {
    /// The set it belongs to.
    pub disc_set_id: DiscSetId,
    /// Which disc of the set it is.
    pub sequence_number: i16,
    /// What it is called, when it has a name of its own.
    pub display_name: Option<&'a str>,
    /// What kind of medium it is.
    pub media_family: &'a str,
    /// Which region it plays in.
    pub region: Option<&'a str>,
}

/// Record a disc.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn create_disc(
    pool: &PgPool,
    new: NewDisc<'_>,
) -> Result<Result<DiscRecord, CatalogOutcome>, DbError> {
    let id = DiscId::generate();
    let inserted = sqlx::query(
        "INSERT INTO discs (id, disc_set_id, sequence_number, display_name, media_family, region)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id.as_uuid())
    .bind(new.disc_set_id.as_uuid())
    .bind(new.sequence_number)
    .bind(new.display_name)
    .bind(new.media_family)
    .bind(new.region)
    .execute(pool)
    .await;

    if let Err(error) = inserted {
        if is_foreign_key_violation(&error) {
            return Ok(Err(CatalogOutcome::ParentMissing));
        }
        // Disc numbers are unique within a set. A second disc two would make
        // the set ambiguous for good, so it is refused rather than accepted
        // and disambiguated later.
        if is_unique_violation(&error) {
            return Ok(Err(CatalogOutcome::SequenceTaken));
        }
        return Err(DbError::Query(error));
    }

    let disc = get_disc(pool, id).await?.ok_or_else(|| DbError::Enum {
        column: "discs.id",
        value: id.to_string(),
    })?;
    Ok(Ok(disc))
}

const DISC_SELECT: &str = "
    SELECT d.id, d.disc_set_id, d.sequence_number, d.display_name, d.media_family,
           d.region, d.volume_label, d.compatibility_claim, d.created_at,
           coalesce(links.artifacts, 0) AS artifact_count,
           coalesce(copies.made, 0) AS copy_count
    FROM discs d
    LEFT JOIN LATERAL (
        SELECT count(*) AS artifacts FROM artifact_disc_links l WHERE l.disc_id = d.id
    ) links ON true
    LEFT JOIN LATERAL (
        SELECT count(*) AS made FROM physical_copies c WHERE c.disc_id = d.id
    ) copies ON true
";

fn disc_from_row(row: &sqlx::postgres::PgRow) -> Result<DiscRecord, DbError> {
    Ok(DiscRecord {
        id: DiscId::from_uuid(row.try_get("id").map_err(DbError::Query)?),
        disc_set_id: DiscSetId::from_uuid(row.try_get("disc_set_id").map_err(DbError::Query)?),
        sequence_number: row.try_get("sequence_number").map_err(DbError::Query)?,
        display_name: row.try_get("display_name").map_err(DbError::Query)?,
        media_family: row.try_get("media_family").map_err(DbError::Query)?,
        region: row.try_get("region").map_err(DbError::Query)?,
        volume_label: row.try_get("volume_label").map_err(DbError::Query)?,
        compatibility_claim: row.try_get("compatibility_claim").map_err(DbError::Query)?,
        artifact_count: row.try_get("artifact_count").map_err(DbError::Query)?,
        copy_count: row.try_get("copy_count").map_err(DbError::Query)?,
        created_at: row.try_get("created_at").map_err(DbError::Query)?,
    })
}

/// One disc.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn get_disc(pool: &PgPool, id: DiscId) -> Result<Option<DiscRecord>, DbError> {
    let row = sqlx::query(&format!("{DISC_SELECT} WHERE d.id = $1"))
        .bind(id.as_uuid())
        .fetch_optional(pool)
        .await
        .map_err(DbError::Query)?;
    row.as_ref().map(disc_from_row).transpose()
}

/// Every disc in a set, in order.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn list_discs(pool: &PgPool, disc_set_id: DiscSetId) -> Result<Vec<DiscRecord>, DbError> {
    let rows = sqlx::query(&format!(
        "{DISC_SELECT} WHERE d.disc_set_id = $1 ORDER BY d.sequence_number"
    ))
    .bind(disc_set_id.as_uuid())
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter().map(disc_from_row).collect()
}

/// Link an artifact to the disc it represents.
///
/// Idempotent by the pair: linking the same artifact to the same disc twice
/// updates what is claimed about the relationship rather than failing, because
/// an operator correcting a relationship should not have to unlink first.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn link_artifact_to_disc(
    pool: &PgPool,
    artifact_id: ArtifactId,
    disc_id: DiscId,
    relationship: &str,
    confidence: f32,
) -> Result<Result<(), CatalogOutcome>, DbError> {
    let linked = sqlx::query(
        "INSERT INTO artifact_disc_links (artifact_id, disc_id, relationship, confidence)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (artifact_id, disc_id) DO UPDATE SET
             relationship = EXCLUDED.relationship,
             confidence = EXCLUDED.confidence",
    )
    .bind(artifact_id.as_uuid())
    .bind(disc_id.as_uuid())
    .bind(relationship)
    .bind(confidence)
    .execute(pool)
    .await;

    if let Err(error) = linked {
        if is_foreign_key_violation(&error) {
            return Ok(Err(CatalogOutcome::ParentMissing));
        }
        return Err(DbError::Query(error));
    }
    Ok(Ok(()))
}

/// The artifacts linked to a disc.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn list_disc_artifacts(
    pool: &PgPool,
    disc_id: DiscId,
) -> Result<Vec<(ArtifactId, String, f32)>, DbError> {
    let rows = sqlx::query(
        "SELECT artifact_id, relationship, confidence
         FROM artifact_disc_links
         WHERE disc_id = $1
         ORDER BY created_at",
    )
    .bind(disc_id.as_uuid())
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;

    rows.iter()
        .map(|row| {
            Ok((
                ArtifactId::from_uuid(row.try_get("artifact_id").map_err(DbError::Query)?),
                row.try_get("relationship").map_err(DbError::Query)?,
                row.try_get("confidence").map_err(DbError::Query)?,
            ))
        })
        .collect()
}
