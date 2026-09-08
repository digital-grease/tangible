// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Repository behaviour against a real PostgreSQL.
//!
//! The concurrency tests here are the point. Asserting that a constraint
//! rejects a second insert proves the constraint exists; running two claims at
//! once proves the *query* is safe, which is a different and more easily
//! broken thing.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use sqlx::{Executor as _, PgPool};
use tangible_db::repositories::{
    ClaimOutcome, EnrollmentOutcome, IncomingEvent, NewBurnJob, authenticate_worker,
    cancel_burn_job, claim_next_burn_job, consume_enrollment, create_burn_job, record_events,
    record_heartbeat, renew_lease, set_attempt_state,
};
use tangible_db::{Database, DbConfig};
use tangible_domain::{BurnAttemptId, BurnJobId, BurnJobState, DriveId, WorkerId};
use time::OffsetDateTime;

/// Serialises tests that assert on the state of the whole queue.
///
/// The claim takes any queued job rather than one scoped to the caller, so a
/// test asserting "no work anywhere" races every other test that queues
/// something. The concurrency tests deliberately do *not* take this: they need
/// the contention.
static QUEUE: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

async fn exclusive_queue() -> tokio::sync::MutexGuard<'static, ()> {
    QUEUE
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Empty the claimable queue, so a test sees only what it queues itself.
///
/// Cancelled rather than deleted: burn attempts reference their job with
/// RESTRICT, which is deliberate: a record of work performed must not vanish
/// because someone tidied the queue. Cancelling removes a job from the
/// claimable set without pretending it never existed.
///
/// Only safe to call while holding the queue lock.
async fn drain_queue(pool: &PgPool) {
    pool.execute("UPDATE burn_jobs SET state = 'canceled' WHERE state = 'queued'")
        .await
        .expect("drain the queue");
}

async fn pool() -> PgPool {
    let url = std::env::var("TANGIBLE_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TANGIBLE_DATABASE_URL"))
        .expect("set TANGIBLE_TEST_DATABASE_URL to run integration tests");
    let database = Database::connect(&DbConfig::new(url))
        .await
        .expect("connect");
    database.migrate().await.expect("migrate");
    database.pool().clone()
}

/// A fresh catalog chain plus a worker and drive, all uniquely identified so
/// tests can run in parallel against one database.
struct World {
    worker: WorkerId,
    drive: DriveId,
    artifact: uuid::Uuid,
    disc: uuid::Uuid,
}

async fn seed(pool: &PgPool) -> World {
    let ids: Vec<uuid::Uuid> = (0..8).map(|_| uuid::Uuid::now_v7()).collect();
    let digest = uuid::Uuid::now_v7().simple().to_string().repeat(2);
    let worker = WorkerId::generate();
    let drive = DriveId::generate();

    let statements = [
        format!("INSERT INTO cas_objects (sha256, size_bytes) VALUES ('{digest}', 10)"),
        format!(
            "INSERT INTO titles (id, display_title, sort_title) VALUES ('{}', 'T', 'T')",
            ids[0]
        ),
        format!(
            "INSERT INTO editions (id, title_id, display_name) VALUES ('{}', '{}', 'E')",
            ids[1], ids[0]
        ),
        format!(
            "INSERT INTO disc_sets (id, edition_id, name) VALUES ('{}', '{}', 'S')",
            ids[2], ids[1]
        ),
        format!(
            "INSERT INTO discs (id, disc_set_id, sequence_number) VALUES ('{}', '{}', 1)",
            ids[3], ids[2]
        ),
        format!(
            "INSERT INTO artifacts (id, origin, manifest_version, total_bytes, component_count, validation_state) \
             VALUES ('{}', 'imported_original', 'v1alpha1', 10, 1, 'valid')",
            ids[4]
        ),
        format!(
            "INSERT INTO workers (id, name, software_version, protocol_version, credential_hash) \
             VALUES ('{worker}', 'worker-{worker}', '0.1.0', '1alpha1', 'hash-{worker}')"
        ),
        format!(
            "INSERT INTO drives (id, worker_id, configured_name, device_alias) \
             VALUES ('{drive}', '{worker}', 'd1', '/dev/disc-block')"
        ),
    ];
    for statement in statements {
        pool.execute(statement.as_str())
            .await
            .unwrap_or_else(|error| panic!("seed failed on {statement}: {error}"));
    }

    World {
        worker,
        drive,
        artifact: ids[4],
        disc: ids[3],
    }
}

async fn queue_job(pool: &PgPool, world: &World) -> uuid::Uuid {
    let id = uuid::Uuid::now_v7();
    let statement = format!(
        "INSERT INTO burn_jobs (id, disc_id, artifact_id, created_by) VALUES ('{id}', '{}', '{}', 'test')",
        world.disc, world.artifact
    );
    pool.execute(statement.as_str()).await.expect("queue job");
    id
}

async fn claim(pool: &PgPool, world: &World) -> Result<tangible_db::ClaimedJob, ClaimOutcome> {
    claim_next_burn_job(
        pool,
        world.worker,
        world.drive,
        "fake",
        "0.1.0",
        &format!("lease-{}", uuid::Uuid::now_v7()),
        90,
    )
    .await
    .expect("claim query")
}

// --- claiming ------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_queued_job_can_be_claimed() {
    let _queue = exclusive_queue().await;
    let pool = pool().await;
    drain_queue(&pool).await;
    let world = seed(&pool).await;
    let job = queue_job(&pool, &world).await;

    let claimed = claim(&pool, &world).await.expect("a job");
    assert_eq!(claimed.burn_job_id.as_uuid(), &job);
    assert_eq!(claimed.attempt_number, 1);
    assert_eq!(claimed.artifact_id, world.artifact);
    assert_eq!(claimed.verification_policy, vec!["full_sector_readback"]);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_empty_queue_reports_no_work() {
    let _queue = exclusive_queue().await;
    let pool = pool().await;
    drain_queue(&pool).await;
    let world = seed(&pool).await;
    assert_eq!(claim(&pool, &world).await, Err(ClaimOutcome::NoWork));
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_busy_drive_is_reported_distinctly_from_an_empty_queue() {
    // Different information to a worker: there is work, but this drive is
    // already doing some.
    let _queue = exclusive_queue().await;
    let pool = pool().await;
    drain_queue(&pool).await;
    let world = seed(&pool).await;
    queue_job(&pool, &world).await;
    queue_job(&pool, &world).await;

    claim(&pool, &world).await.expect("first");
    assert_eq!(claim(&pool, &world).await, Err(ClaimOutcome::DriveBusy));
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_drive_frees_up_once_its_attempt_is_terminal() {
    let _queue = exclusive_queue().await;
    let pool = pool().await;
    drain_queue(&pool).await;
    let world = seed(&pool).await;
    queue_job(&pool, &world).await;
    queue_job(&pool, &world).await;

    let first = claim(&pool, &world).await.expect("first");
    set_attempt_state(&pool, first.attempt_id, "verified")
        .await
        .expect("terminal");

    claim(&pool, &world).await.expect("the drive is free again");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn attempt_numbers_increase_within_a_job() {
    let _queue = exclusive_queue().await;
    let pool = pool().await;
    drain_queue(&pool).await;
    let world = seed(&pool).await;
    queue_job(&pool, &world).await;

    let first = claim(&pool, &world).await.expect("first");
    set_attempt_state(&pool, first.attempt_id, "write_failed")
        .await
        .expect("terminal");

    // Requeue the same job for a retry.
    let statement = format!(
        "UPDATE burn_jobs SET state = 'queued' WHERE id = '{}'",
        first.burn_job_id
    );
    pool.execute(statement.as_str()).await.expect("requeue");

    let second = claim(&pool, &world).await.expect("second");
    assert_eq!(second.burn_job_id, first.burn_job_id);
    assert_eq!(second.attempt_number, 2, "a retry is a new attempt");
    assert_ne!(second.attempt_id, first.attempt_id);
}

// --- the concurrency that matters ------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn concurrent_workers_never_receive_the_same_job() {
    // The reason the claim uses SKIP LOCKED. Two lasers writing what the
    // records call one disc is the failure this prevents.
    let _queue = exclusive_queue().await;
    let pool = pool().await;
    drain_queue(&pool).await;

    // Ten drives on ten workers, five jobs. Every drive polls at once.
    let mut worlds = Vec::new();
    for _ in 0..10 {
        worlds.push(seed(&pool).await);
    }
    let shared = &worlds[0];
    for _ in 0..5 {
        queue_job(&pool, shared).await;
    }

    let mut handles = Vec::new();
    for world in worlds {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            claim_next_burn_job(
                &pool,
                world.worker,
                world.drive,
                "fake",
                "0.1.0",
                &format!("lease-{}", uuid::Uuid::now_v7()),
                90,
            )
            .await
            .expect("claim query")
        }));
    }

    let mut claimed_jobs = Vec::new();
    for handle in handles {
        if let Ok(job) = handle.await.expect("task") {
            claimed_jobs.push(job.burn_job_id);
        }
    }

    assert_eq!(
        claimed_jobs.len(),
        5,
        "exactly the five queued jobs went out"
    );
    let unique: std::collections::BTreeSet<_> = claimed_jobs.iter().collect();
    assert_eq!(
        unique.len(),
        claimed_jobs.len(),
        "no job was handed to two workers"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn one_drive_polling_concurrently_gets_at_most_one_attempt() {
    // A worker with a retry loop, or two processes sharing a drive by mistake.
    // The partial unique index is the backstop the query cannot provide alone.
    let _queue = exclusive_queue().await;
    let pool = pool().await;
    drain_queue(&pool).await;
    let world = seed(&pool).await;
    for _ in 0..5 {
        queue_job(&pool, &world).await;
    }

    let mut handles = Vec::new();
    for _ in 0..8 {
        let pool = pool.clone();
        let worker = world.worker;
        let drive = world.drive;
        handles.push(tokio::spawn(async move {
            claim_next_burn_job(
                &pool,
                worker,
                drive,
                "fake",
                "0.1.0",
                &format!("lease-{}", uuid::Uuid::now_v7()),
                90,
            )
            .await
            .expect("claim query")
        }));
    }

    let mut succeeded = 0;
    for handle in handles {
        if handle.await.expect("task").is_ok() {
            succeeded += 1;
        }
    }
    assert_eq!(
        succeeded, 1,
        "one drive, one attempt, however hard it polls"
    );
}

// --- leases ----------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_lease_can_be_renewed_by_its_holder() {
    let _queue = exclusive_queue().await;
    let pool = pool().await;
    drain_queue(&pool).await;
    let world = seed(&pool).await;
    queue_job(&pool, &world).await;

    let token = format!("lease-{}", uuid::Uuid::now_v7());
    let claimed = claim_next_burn_job(
        &pool,
        world.worker,
        world.drive,
        "fake",
        "0.1.0",
        &token,
        90,
    )
    .await
    .expect("claim")
    .expect("a job");

    let renewed = renew_lease(&pool, claimed.attempt_id, &token, 180)
        .await
        .expect("renew")
        .expect("a new expiry");
    assert!(renewed > claimed.lease_expires_at);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_lease_cannot_be_renewed_by_anyone_else() {
    // The token is part of the WHERE clause, so a renewal from anything but
    // the holder matches no row.
    let _queue = exclusive_queue().await;
    let pool = pool().await;
    drain_queue(&pool).await;
    let world = seed(&pool).await;
    queue_job(&pool, &world).await;
    let claimed = claim(&pool, &world).await.expect("a job");

    assert_eq!(
        renew_lease(&pool, claimed.attempt_id, "not-the-token", 180)
            .await
            .expect("renew"),
        None
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_terminal_attempt_cannot_have_its_lease_renewed() {
    let _queue = exclusive_queue().await;
    let pool = pool().await;
    drain_queue(&pool).await;
    let world = seed(&pool).await;
    queue_job(&pool, &world).await;
    let token = format!("lease-{}", uuid::Uuid::now_v7());
    let claimed = claim_next_burn_job(
        &pool,
        world.worker,
        world.drive,
        "fake",
        "0.1.0",
        &token,
        90,
    )
    .await
    .expect("claim")
    .expect("a job");

    set_attempt_state(&pool, claimed.attempt_id, "verified")
        .await
        .expect("terminal");
    assert_eq!(
        renew_lease(&pool, claimed.attempt_id, &token, 180)
            .await
            .expect("renew"),
        None
    );
}

// --- events ----------------------------------------------------------------------

fn event(sequence: i64) -> IncomingEvent {
    IncomingEvent {
        sequence,
        event_type: "progress".to_owned(),
        stage: "writing".to_owned(),
        message_code: "WRITE_PROGRESS".to_owned(),
        progress: Some(0.5),
        data: serde_json::json!({}),
        worker_timestamp: OffsetDateTime::now_utc(),
    }
}

async fn attempt_with_events(pool: &PgPool) -> BurnAttemptId {
    let _queue = exclusive_queue().await;
    drain_queue(pool).await;
    let world = seed(pool).await;
    queue_job(pool, &world).await;
    claim(pool, &world).await.expect("a job").attempt_id
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn events_are_recorded_and_acknowledged() {
    let pool = pool().await;
    let attempt = attempt_with_events(&pool).await;

    let through = record_events(&pool, attempt, &[event(1), event(2), event(3)], None)
        .await
        .expect("record");
    assert_eq!(through, 3);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn resubmitting_a_batch_is_idempotent() {
    // What lets a worker resend everything unacknowledged after an outage
    // without knowing which of them arrived.
    let pool = pool().await;
    let attempt = attempt_with_events(&pool).await;

    let batch = [event(1), event(2), event(3)];
    assert_eq!(
        record_events(&pool, attempt, &batch, None)
            .await
            .expect("first"),
        3
    );
    assert_eq!(
        record_events(&pool, attempt, &batch, None)
            .await
            .expect("second"),
        3,
        "a replay changes nothing"
    );

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM burn_events WHERE attempt_id = $1")
        .bind(attempt.as_uuid())
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(count, 3, "no duplicates were created");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn acknowledgement_stops_at_a_gap() {
    // Acknowledging the maximum would tell the worker to discard events the
    // server does not have. Contiguous is the only safe answer.
    let pool = pool().await;
    let attempt = attempt_with_events(&pool).await;

    let through = record_events(&pool, attempt, &[event(1), event(2), event(5)], None)
        .await
        .expect("record");
    assert_eq!(through, 2, "3 and 4 are missing, so 5 is not acknowledged");

    let through = record_events(&pool, attempt, &[event(3), event(4)], None)
        .await
        .expect("fill the gap");
    assert_eq!(through, 5, "the gap closed, so everything is acknowledged");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_empty_batch_reports_what_is_already_held() {
    let pool = pool().await;
    let attempt = attempt_with_events(&pool).await;
    record_events(&pool, attempt, &[event(1)], None)
        .await
        .expect("record");

    assert_eq!(
        record_events(&pool, attempt, &[], None)
            .await
            .expect("empty"),
        1
    );
}

// --- enrollment --------------------------------------------------------------------

async fn issue_enrollment(pool: &PgPool, hash: &str, minutes: i64) {
    let statement = format!(
        "INSERT INTO worker_enrollments (id, token_hash, expires_at, created_by) \
         VALUES ('{}', '{hash}', now() + interval '{minutes} minutes', 'admin')",
        uuid::Uuid::now_v7()
    );
    pool.execute(statement.as_str()).await.expect("issue");
}

fn hash() -> String {
    uuid::Uuid::now_v7().simple().to_string().repeat(2)
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_enrollment_token_creates_a_worker() {
    let pool = pool().await;
    let token = hash();
    issue_enrollment(&pool, &token, 15).await;

    let worker = consume_enrollment(
        &pool,
        &token,
        &format!("w-{token}"),
        "0.1.0",
        "1alpha1",
        &format!("cred-{token}"),
    )
    .await
    .expect("consume")
    .expect("a worker");

    assert_eq!(
        authenticate_worker(&pool, &format!("cred-{token}"))
            .await
            .expect("authenticate"),
        Some(worker)
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_enrollment_token_cannot_be_used_twice() {
    let pool = pool().await;
    let token = hash();
    issue_enrollment(&pool, &token, 15).await;

    consume_enrollment(
        &pool,
        &token,
        &format!("w-{token}"),
        "0.1.0",
        "1alpha1",
        &format!("c-{token}"),
    )
    .await
    .expect("first")
    .expect("a worker");

    assert_eq!(
        consume_enrollment(
            &pool,
            &token,
            &format!("w2-{token}"),
            "0.1.0",
            "1alpha1",
            &format!("c2-{token}")
        )
        .await
        .expect("second"),
        Err(EnrollmentOutcome::TokenUnusable),
        "a replayed enrollment must not mint a second credential"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn concurrent_enrollment_with_one_token_yields_one_worker() {
    // The one-use guarantee under a race. Check-then-update would leave a
    // window where both requests see 'unused'.
    let pool = pool().await;
    let token = hash();
    issue_enrollment(&pool, &token, 15).await;

    let mut handles = Vec::new();
    for index in 0..8 {
        let pool = pool.clone();
        let token = token.clone();
        handles.push(tokio::spawn(async move {
            consume_enrollment(
                &pool,
                &token,
                &format!("w{index}-{token}"),
                "0.1.0",
                "1alpha1",
                &format!("c{index}-{token}"),
            )
            .await
            .expect("consume")
        }));
    }

    let mut created = 0;
    for handle in handles {
        if handle.await.expect("task").is_ok() {
            created += 1;
        }
    }
    assert_eq!(created, 1, "one token, one worker, however many racers");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_expired_enrollment_token_is_refused() {
    let pool = pool().await;
    let token = hash();
    issue_enrollment(&pool, &token, -5).await;

    assert_eq!(
        consume_enrollment(
            &pool,
            &token,
            &format!("w-{token}"),
            "0.1.0",
            "1alpha1",
            &format!("c-{token}")
        )
        .await
        .expect("consume"),
        Err(EnrollmentOutcome::TokenUnusable)
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_unknown_token_is_refused() {
    let pool = pool().await;
    assert_eq!(
        consume_enrollment(
            &pool,
            &hash(),
            &format!("w-{}", hash()),
            "0.1.0",
            "1alpha1",
            "c"
        )
        .await
        .expect("consume"),
        Err(EnrollmentOutcome::TokenUnusable)
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_name_already_taken_is_reported_as_such() {
    // Worker names are unique. An operator reusing one is making an ordinary
    // mistake, and must not be told storage is unavailable, which is what the
    // unique violation surfaced as before it was given its own outcome.
    let pool = pool().await;
    let name = format!("w-{}", hash());

    let first = hash();
    issue_enrollment(&pool, &first, 15).await;
    consume_enrollment(
        &pool,
        &first,
        &name,
        "0.1.0",
        "1alpha1",
        &format!("c-{first}"),
    )
    .await
    .expect("first")
    .expect("a worker");

    let second = hash();
    issue_enrollment(&pool, &second, 15).await;
    assert_eq!(
        consume_enrollment(
            &pool,
            &second,
            &name,
            "0.1.0",
            "1alpha1",
            &format!("c-{second}")
        )
        .await
        .expect("second"),
        Err(EnrollmentOutcome::NameTaken)
    );

    // The token is untouched, so a corrected request still works.
    assert!(
        consume_enrollment(
            &pool,
            &second,
            &format!("{name}-2"),
            "0.1.0",
            "1alpha1",
            &format!("c-{second}")
        )
        .await
        .expect("retry")
        .is_ok(),
        "a rejected name must not spend the token"
    );
}

// --- workers --------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_revoked_worker_cannot_authenticate() {
    let pool = pool().await;
    let world = seed(&pool).await;
    let credential = format!("hash-{}", world.worker);

    assert!(
        authenticate_worker(&pool, &credential)
            .await
            .expect("authenticate")
            .is_some()
    );

    let statement = format!(
        "UPDATE workers SET status = 'revoked', revoked_at = now() WHERE id = '{}'",
        world.worker
    );
    pool.execute(statement.as_str()).await.expect("revoke");

    assert_eq!(
        authenticate_worker(&pool, &credential)
            .await
            .expect("authenticate"),
        None,
        "revocation is enforced in the lookup, not left to the caller"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_heartbeat_brings_a_pending_worker_online() {
    let pool = pool().await;
    let world = seed(&pool).await;

    assert!(
        record_heartbeat(&pool, world.worker)
            .await
            .expect("heartbeat")
    );

    let status: String = sqlx::query_scalar("SELECT status FROM workers WHERE id = $1")
        .bind(world.worker.as_uuid())
        .fetch_one(&pool)
        .await
        .expect("status");
    assert_eq!(status, "online");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_heartbeat_from_a_revoked_worker_is_refused() {
    let pool = pool().await;
    let world = seed(&pool).await;
    let statement = format!(
        "UPDATE workers SET status = 'revoked', revoked_at = now() WHERE id = '{}'",
        world.worker
    );
    pool.execute(statement.as_str()).await.expect("revoke");

    assert!(
        !record_heartbeat(&pool, world.worker)
            .await
            .expect("heartbeat")
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_heartbeat_does_not_resurrect_a_draining_worker() {
    // Draining means "finish what you hold, take nothing new". A heartbeat
    // must not undo that.
    let pool = pool().await;
    let world = seed(&pool).await;
    let statement = format!(
        "UPDATE workers SET status = 'draining' WHERE id = '{}'",
        world.worker
    );
    pool.execute(statement.as_str()).await.expect("drain");

    record_heartbeat(&pool, world.worker)
        .await
        .expect("heartbeat");

    let status: String = sqlx::query_scalar("SELECT status FROM workers WHERE id = $1")
        .bind(world.worker.as_uuid())
        .fetch_one(&pool)
        .await
        .expect("status");
    assert_eq!(status, "draining");
}

// --- queueing ------------------------------------------------------------------

fn new_job<'a>(world: &World, policy: &'a [String], key: Option<&'a str>) -> NewBurnJob<'a> {
    NewBurnJob {
        disc_id: world.disc,
        artifact_id: world.artifact,
        requested_media_profile: None,
        verification_policy: policy,
        eject_policy: "eject_on_success",
        priority: 0,
        created_by: "test",
        idempotency_key: key,
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn two_simultaneous_creates_under_one_key_produce_one_job() {
    // The window the unique index exists to close. Both requests look up the
    // key, find nothing, and insert; one loses, and losing must mean being
    // handed the winner's job rather than an error the caller would retry:
    // which is how a dropped response turns into a second disc.
    let pool = pool().await;
    let world = seed(&pool).await;
    let policy = vec!["full_sector_readback".to_owned()];
    let key = format!("race-{}", uuid::Uuid::now_v7());

    let first = create_burn_job(&pool, new_job(&world, &policy, Some(&key)));
    let second = create_burn_job(&pool, new_job(&world, &policy, Some(&key)));
    let (first, second) = tokio::join!(first, second);

    let first = first.expect("query").expect("created");
    let second = second.expect("query").expect("created");
    assert_eq!(first.job.id, second.job.id, "the key produced two jobs");

    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM burn_jobs WHERE idempotency_key = $1")
            .bind(&key)
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(count, 1);
    // Exactly one of them created it; the other replayed.
    assert!(
        first.replayed != second.replayed,
        "one create must be a replay of the other"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_job_queued_without_a_key_is_its_own_job() {
    // Idempotency is opt-in. Two deliberate requests for two discs must
    // produce two jobs.
    let pool = pool().await;
    let world = seed(&pool).await;
    let policy = vec!["full_sector_readback".to_owned()];

    let first = create_burn_job(&pool, new_job(&world, &policy, None))
        .await
        .expect("query")
        .expect("created");
    let second = create_burn_job(&pool, new_job(&world, &policy, None))
        .await
        .expect("query")
        .expect("created");

    assert_ne!(first.job.id, second.job.id);
    assert!(!first.replayed && !second.replayed);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn cancelling_while_a_worker_claims_never_leaves_both_true() {
    // Whichever wins, the pair must stay consistent: a cancelled job with a
    // live attempt would be a drive nobody is watching, and a leased job that
    // reported itself cancelled would tell an operator a burn had stopped
    // while it had not.
    let _queue = exclusive_queue().await;
    let pool = pool().await;
    drain_queue(&pool).await;
    let world = seed(&pool).await;
    let policy = vec!["full_sector_readback".to_owned()];
    let created = create_burn_job(&pool, new_job(&world, &policy, None))
        .await
        .expect("query")
        .expect("created");

    let lease = format!("lease-{}", uuid::Uuid::now_v7());
    let cancelling = cancel_burn_job(&pool, created.job.id);
    let claiming = claim_next_burn_job(
        &pool,
        world.worker,
        world.drive,
        "fake",
        "0.1.0",
        &lease,
        90,
    );
    let (cancelled, claimed) = tokio::join!(cancelling, claiming);
    cancelled.expect("cancel query").ok();
    claimed.expect("claim query").ok();

    let state: String = sqlx::query_scalar("SELECT state FROM burn_jobs WHERE id = $1")
        .bind(created.job.id.as_uuid())
        .fetch_one(&pool)
        .await
        .expect("state");
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM burn_attempts
         WHERE burn_job_id = $1
           AND state IN ('claimed', 'staging', 'preflighting', 'writing', 'written', 'verifying')",
    )
    .bind(created.job.id.as_uuid())
    .fetch_one(&pool)
    .await
    .expect("count");

    match state.parse::<BurnJobState>().expect("a known state") {
        BurnJobState::Canceled => assert_eq!(active, 0, "a cancelled job kept a live attempt"),
        BurnJobState::Leased => assert_eq!(active, 1, "a leased job has exactly one attempt"),
        other => panic!("unexpected state {other}"),
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn cancelling_an_unknown_job_is_reported_rather_than_silently_succeeding() {
    let pool = pool().await;
    assert!(
        cancel_burn_job(&pool, BurnJobId::generate())
            .await
            .expect("query")
            .is_err()
    );
}
