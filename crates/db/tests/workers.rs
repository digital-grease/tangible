// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Workers as people manage them, against a real PostgreSQL
//! (docs/plans/worker-admin.md).
//!
//! Each test makes its own worker and drive, so they run in parallel against
//! the shared database. Nothing here queues a burn: a draining worker's burn
//! claim is refused before the queue is read, which is what is asserted.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use sqlx::{Executor as _, PgPool};
use tangible_db::erasures::{claim_erasure, request_erasure};
use tangible_db::repositories::{
    ClaimOutcome, authenticate_worker, claim_next_burn_job, record_heartbeat,
};
use tangible_db::rips::{RipOptions, claim_rip, request_rip};
use tangible_db::workers::{
    WorkerChange, drain_worker, enable_worker, get_worker, list_workers, revoke_worker,
};
use tangible_db::{Database, DbConfig};
use tangible_domain::{DriveId, ErasureMode, WorkerId};

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

/// A worker, never heard from, with one drive.
async fn worker_and_drive(pool: &PgPool) -> (WorkerId, DriveId) {
    let worker = WorkerId::generate();
    let drive = DriveId::generate();
    for statement in [
        format!(
            "INSERT INTO workers (id, name, software_version, protocol_version, credential_hash) \
             VALUES ('{worker}', 'admin-{worker}', '0.1.0', '1alpha1', 'hash-{worker}')"
        ),
        format!(
            "INSERT INTO drives (id, worker_id, configured_name, device_alias) \
             VALUES ('{drive}', '{worker}', 'bench', '/dev/sr0')"
        ),
    ] {
        pool.execute(sqlx::AssertSqlSafe(statement.as_str()))
            .await
            .expect("seed");
    }
    (worker, drive)
}

fn status(change: &WorkerChange) -> &str {
    match change {
        WorkerChange::Done(record) => &record.status,
        other => panic!("not done: {other:?}"),
    }
}

async fn audited(pool: &PgPool, worker: WorkerId) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT action, actor_id FROM audit_events WHERE target_id = $1 ORDER BY created_at, id",
    )
    .bind(worker.to_string())
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_worker_is_drained_enabled_and_revoked_as_asked_and_audited() {
    let pool = pool().await;
    let (worker, drive) = worker_and_drive(&pool).await;

    // Never heard from: enabled again it is pending, not online.
    assert_eq!(
        status(&drain_worker(&pool, worker, "alice").await.unwrap()),
        "draining"
    );
    assert_eq!(
        status(&drain_worker(&pool, worker, "alice").await.unwrap()),
        "draining",
        "draining again changes nothing"
    );
    assert_eq!(
        status(&enable_worker(&pool, worker, "alice").await.unwrap()),
        "pending"
    );

    // Heard from, it comes back online.
    record_heartbeat(&pool, worker).await.unwrap();
    drain_worker(&pool, worker, "alice").await.unwrap();
    assert_eq!(
        status(&enable_worker(&pool, worker, "bob").await.unwrap()),
        "online"
    );
    assert_eq!(
        status(&enable_worker(&pool, worker, "bob").await.unwrap()),
        "online",
        "enabling a worker that takes work changes nothing"
    );

    let revoked = revoke_worker(&pool, worker, "bob").await.unwrap();
    let WorkerChange::Done(record) = &revoked else {
        panic!("{revoked:?}");
    };
    assert_eq!(record.status, "revoked");
    assert!(record.revoked_at.is_some());
    assert_eq!(record.drives.len(), 1);
    assert_eq!(record.drives[0], (drive, "bench".to_owned()));
    // Final: its credential stops working, and it is not drained or enabled.
    assert_eq!(
        authenticate_worker(&pool, &format!("hash-{worker}"))
            .await
            .unwrap(),
        None
    );
    assert_eq!(record_heartbeat(&pool, worker).await.unwrap(), None);
    assert_eq!(
        drain_worker(&pool, worker, "bob").await.unwrap(),
        WorkerChange::Refused("revoked".to_owned())
    );
    assert_eq!(
        enable_worker(&pool, worker, "bob").await.unwrap(),
        WorkerChange::Refused("revoked".to_owned())
    );
    assert_eq!(
        status(&revoke_worker(&pool, worker, "bob").await.unwrap()),
        "revoked"
    );

    // Only the changes are audited, each by who asked.
    let pairs = |rows: &[(&str, &str)]| -> Vec<(String, String)> {
        rows.iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    };
    assert_eq!(
        audited(&pool, worker).await,
        pairs(&[
            ("worker.drained", "alice"),
            ("worker.enabled", "alice"),
            ("worker.drained", "alice"),
            ("worker.enabled", "bob"),
            ("worker.revoked", "bob"),
        ])
    );

    assert_eq!(
        drain_worker(&pool, WorkerId::generate(), "bob")
            .await
            .unwrap(),
        WorkerChange::NotFound
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_draining_worker_takes_no_new_work_until_enabled() {
    let pool = pool().await;
    let (worker, drive) = worker_and_drive(&pool).await;
    let rip = request_rip(&pool, drive, "operator", RipOptions::default())
        .await
        .unwrap()
        .unwrap();
    drain_worker(&pool, worker, "alice").await.unwrap();

    assert_eq!(
        record_heartbeat(&pool, worker).await.unwrap().as_deref(),
        Some("draining"),
        "the heartbeat tells it"
    );
    assert_eq!(
        claim_rip(&pool, worker, drive, false).await.unwrap(),
        Ok(None)
    );
    assert_eq!(
        claim_next_burn_job(&pool, worker, drive, "fake", "0.1.0", "lease-x", 90)
            .await
            .unwrap()
            .err(),
        Some(ClaimOutcome::NoWork)
    );

    enable_worker(&pool, worker, "alice").await.unwrap();
    let taken = claim_rip(&pool, worker, drive, false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(taken.map(|r| r.id), Some(rip.id));
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_draining_worker_erases_nothing_new() {
    let pool = pool().await;
    let (worker, drive) = worker_and_drive(&pool).await;
    request_erasure(&pool, drive, ErasureMode::Quick, "admin")
        .await
        .unwrap()
        .unwrap();
    drain_worker(&pool, worker, "alice").await.unwrap();
    assert_eq!(claim_erasure(&pool, worker, drive).await.unwrap(), Ok(None));
    enable_worker(&pool, worker, "alice").await.unwrap();
    assert!(
        claim_erasure(&pool, worker, drive)
            .await
            .unwrap()
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn workers_are_listed_oldest_first_a_page_at_a_time() {
    let pool = pool().await;
    let (first, _) = worker_and_drive(&pool).await;
    let (second, _) = worker_and_drive(&pool).await;
    // From just before the first, so the shared database's other workers
    // are behind the cursor.
    let before = uuid::Uuid::from_u128(first.as_uuid().as_u128() - 1);
    let page = list_workers(&pool, 1, Some(before)).await.unwrap();
    assert_eq!(page.iter().map(|w| w.id).collect::<Vec<_>>(), [first]);
    // Tests running beside this one may enroll workers between the two.
    let next = list_workers(&pool, 1000, Some(*first.as_uuid()))
        .await
        .unwrap();
    let ids: Vec<WorkerId> = next.iter().map(|w| w.id).collect();
    assert!(ids.contains(&second));
    assert!(
        ids.windows(2)
            .all(|pair| pair[0].as_uuid() < pair[1].as_uuid())
    );
    assert!(ids.iter().all(|id| id.as_uuid() > first.as_uuid()));
    let one = get_worker(&pool, second).await.unwrap().unwrap();
    assert_eq!(one.status, "pending");
    assert_eq!(one.drives.len(), 1);
    assert_eq!(get_worker(&pool, WorkerId::generate()).await.unwrap(), None);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_revoked_worker_past_authentication_takes_nothing() {
    // Found by review: a claim that authenticated just before the revocation
    // committed reaches the claim after it, and must take nothing.
    let pool = pool().await;
    let (worker, drive) = worker_and_drive(&pool).await;
    request_rip(&pool, drive, "operator", RipOptions::default())
        .await
        .unwrap()
        .unwrap();
    request_erasure(&pool, drive, ErasureMode::Quick, "admin")
        .await
        .unwrap()
        .unwrap();
    revoke_worker(&pool, worker, "alice").await.unwrap();
    assert_eq!(
        claim_rip(&pool, worker, drive, false).await.unwrap(),
        Ok(None)
    );
    assert_eq!(claim_erasure(&pool, worker, drive).await.unwrap(), Ok(None));
    assert_eq!(
        claim_next_burn_job(&pool, worker, drive, "fake", "0.1.0", "lease-x", 90)
            .await
            .unwrap()
            .err(),
        Some(ClaimOutcome::NoWork)
    );
}
