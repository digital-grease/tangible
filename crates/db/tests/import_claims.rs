// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Claiming imports against a real PostgreSQL: an import whose runs keep
//! being interrupted is given up on, rather than taken up on every start
//! (security review, 2026-11-12).

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use sqlx::{Executor as _, PgPool, Row as _};
use tangible_db::repositories::{
    IMPORT_INTERRUPTED, ImportJobRecord, MAX_INTERRUPTED_IMPORT_RUNS, NewImportJob,
    claim_next_import_job, create_import_job, fail_import_job, retry_import_job,
    wait_for_import_source,
};
use tangible_db::{Database, DbConfig};
use tangible_domain::{ImportJobId, ImportState};

/// Serialises the tests here: a claim takes any claimable import, so two
/// tests claiming at once would take each other's.
static QUEUE: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

async fn exclusive_queue() -> tokio::sync::MutexGuard<'static, ()> {
    QUEUE
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// A migrated database with no claimable import but those a test queues.
async fn pool() -> PgPool {
    let url = std::env::var("TANGIBLE_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TANGIBLE_DATABASE_URL"))
        .expect("set TANGIBLE_TEST_DATABASE_URL to run integration tests");
    let database = Database::connect(&DbConfig::new(url))
        .await
        .expect("connect");
    database.migrate().await.expect("migrate");
    let pool = database.pool().clone();
    pool.execute(
        "UPDATE import_jobs SET state = 'canceled', completed_at = now(),
                lease_owner = NULL, lease_expires_at = NULL
         WHERE state NOT IN ('complete', 'failed_terminal', 'canceled', 'quarantined')",
    )
    .await
    .expect("drain the import queue");
    pool
}

async fn queue(pool: &PgPool) -> ImportJobId {
    let id = ImportJobId::generate();
    create_import_job(
        pool,
        NewImportJob {
            id,
            source_kind: "upload",
            source_descriptor: &serde_json::json!({"filename": "disc.iso"}),
            state: ImportState::Requested,
            bytes_expected: None,
            bytes_received: 0,
            created_by: "import-claims-test",
        },
    )
    .await
    .expect("queue an import");
    id
}

async fn claim(pool: &PgPool) -> Option<ImportJobRecord> {
    claim_next_import_job(pool, "import-claims-test", 3600)
        .await
        .expect("claim")
}

/// What a server stopping mid-import leaves: its lease, lapsed.
async fn lapse(pool: &PgPool, id: ImportJobId) {
    sqlx::query(
        "UPDATE import_jobs SET lease_expires_at = now() - interval '1 second' WHERE id = $1",
    )
    .bind(id.as_uuid())
    .execute(pool)
    .await
    .expect("lapse the lease");
}

async fn interrupted_runs(pool: &PgPool, id: ImportJobId) -> i32 {
    sqlx::query("SELECT interrupted_runs FROM import_jobs WHERE id = $1")
        .bind(id.as_uuid())
        .fetch_one(pool)
        .await
        .expect("read the import")
        .get("interrupted_runs")
}

#[test]
fn three_interrupted_runs_are_the_limit() {
    // The tests below count to it; a change is a decision, not a tweak.
    assert_eq!(MAX_INTERRUPTED_IMPORT_RUNS, 3);
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run with --ignored"]
async fn an_import_whose_runs_keep_being_interrupted_is_given_up_on() {
    let _queue = exclusive_queue().await;
    let pool = pool().await;
    let id = queue(&pool).await;

    // The first run, and each time the server stopped during one and the
    // import was taken up again.
    assert_eq!(claim(&pool).await.map(|job| job.id), Some(id));
    assert_eq!(interrupted_runs(&pool, id).await, 0);
    for runs in 1..3 {
        lapse(&pool, id).await;
        assert_eq!(claim(&pool).await.map(|job| job.id), Some(id));
        assert_eq!(interrupted_runs(&pool, id).await, runs);
    }

    // Interrupted a third time: not taken up again, but failed for good,
    // and that sticks although nothing else was claimed.
    lapse(&pool, id).await;
    assert!(claim(&pool).await.is_none());
    let row = sqlx::query(
        "SELECT state, error_code, error_detail, lease_owner, completed_at, interrupted_runs
         FROM import_jobs WHERE id = $1",
    )
    .bind(id.as_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("state"), "failed_terminal");
    assert_eq!(row.get::<String, _>("error_code"), IMPORT_INTERRUPTED);
    assert!(
        row.get::<String, _>("error_detail")
            .contains("stopped 3 times")
    );
    assert!(row.get::<Option<String>, _>("lease_owner").is_none());
    assert!(
        row.get::<Option<time::OffsetDateTime>, _>("completed_at")
            .is_some()
    );
    assert_eq!(row.get::<i32, _>("interrupted_runs"), 3);
    assert!(claim(&pool).await.is_none());

    // A person retrying it starts the count again.
    assert_eq!(
        retry_import_job(&pool, id).await.unwrap(),
        Ok(ImportState::Requested)
    );
    assert_eq!(interrupted_runs(&pool, id).await, 0);
    assert_eq!(claim(&pool).await.map(|job| job.id), Some(id));
    assert_eq!(interrupted_runs(&pool, id).await, 0);
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run with --ignored"]
async fn the_claim_goes_on_past_an_import_it_gives_up_on() {
    let _queue = exclusive_queue().await;
    let pool = pool().await;
    let doomed = queue(&pool).await;
    let next = queue(&pool).await;
    assert_eq!(claim(&pool).await.map(|job| job.id), Some(doomed));
    sqlx::query("UPDATE import_jobs SET interrupted_runs = $2 WHERE id = $1")
        .bind(doomed.as_uuid())
        .bind(2)
        .execute(&pool)
        .await
        .unwrap();
    lapse(&pool, doomed).await;

    // The older one is given up on, and the newer one claimed in the same
    // claim.
    assert_eq!(claim(&pool).await.map(|job| job.id), Some(next));
    let state: String = sqlx::query_scalar("SELECT state FROM import_jobs WHERE id = $1")
        .bind(doomed.as_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "failed_terminal");
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run with --ignored"]
async fn waiting_is_not_an_interrupted_run() {
    let _queue = exclusive_queue().await;
    let pool = pool().await;
    let id = queue(&pool).await;

    // A retryable failure's delay, and a source not yet ready, each end by
    // their lease lapsing, as often as they need to: neither is the server
    // stopping.
    for _ in 0..4 {
        assert_eq!(claim(&pool).await.map(|job| job.id), Some(id));
        assert!(
            fail_import_job(&pool, id, true, "IMPORT_READ_FAILED", "try again", 30)
                .await
                .unwrap()
        );
        lapse(&pool, id).await;
        assert_eq!(claim(&pool).await.map(|job| job.id), Some(id));
        assert!(
            wait_for_import_source(&pool, id, None, None, 30, None)
                .await
                .unwrap()
        );
        lapse(&pool, id).await;
    }
    assert_eq!(claim(&pool).await.map(|job| job.id), Some(id));
    assert_eq!(interrupted_runs(&pool, id).await, 0);
}
