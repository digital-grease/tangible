// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Worker credential rotation, against a real PostgreSQL
//! (docs/plans/credential-rotation.md).
//!
//! Each test makes its own worker, so they run in parallel against the shared
//! database.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use sqlx::{Executor as _, PgPool};
use tangible_db::repositories::{
    authenticate_worker, credential_rotation_due, issue_next_credential,
};
use tangible_db::workers::{WorkerChange, get_worker, request_credential_rotation, revoke_worker};
use tangible_db::{Database, DbConfig};
use tangible_domain::WorkerId;

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

/// A worker whose credential hashes to `old-{id}`.
async fn worker(pool: &PgPool) -> WorkerId {
    let worker = WorkerId::generate();
    pool.execute(sqlx::AssertSqlSafe(
        format!(
            "INSERT INTO workers (id, name, software_version, protocol_version, credential_hash) \
             VALUES ('{worker}', 'rotate-{worker}', '0.1.0', '1alpha1', 'old-{worker}')"
        )
        .as_str(),
    ))
    .await
    .expect("seed");
    worker
}

async fn audited(pool: &PgPool, worker: WorkerId) -> Vec<(String, String, String)> {
    sqlx::query_as(
        "SELECT action, actor_type, actor_id FROM audit_events WHERE target_id = $1 \
         ORDER BY created_at, id",
    )
    .bind(worker.to_string())
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_old_credential_works_until_the_new_one_is_first_used() {
    let pool = pool().await;
    let worker = worker(&pool).await;
    let old = format!("old-{worker}");
    let new = format!("new-{worker}");
    let before = get_worker(&pool, worker).await.unwrap().unwrap();

    issue_next_credential(&pool, worker, &new)
        .await
        .unwrap()
        .expect("issued");

    // Both work while the new one has not been presented: a worker that lost
    // the answer, or stopped before saving it, is not locked out.
    assert_eq!(
        authenticate_worker(&pool, &old).await.unwrap(),
        Some(worker)
    );
    // Presenting the new one makes it the credential...
    assert_eq!(
        authenticate_worker(&pool, &new).await.unwrap(),
        Some(worker)
    );
    // ...and the old one stops working, for good.
    assert_eq!(authenticate_worker(&pool, &old).await.unwrap(), None);
    assert_eq!(
        authenticate_worker(&pool, &new).await.unwrap(),
        Some(worker),
        "the new one keeps working"
    );

    let after = get_worker(&pool, worker).await.unwrap().unwrap();
    assert!(after.credential_issued_at > before.credential_issued_at);
    let next: (Option<String>, Option<time::OffsetDateTime>) = sqlx::query_as(
        "SELECT next_credential_hash, next_credential_issued_at FROM workers WHERE id = $1",
    )
    .bind(worker.as_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(next, (None, None));

    let worker_id = worker.to_string();
    assert_eq!(
        audited(&pool, worker).await,
        vec![
            (
                "worker.credential_issued".to_owned(),
                "worker".to_owned(),
                worker_id.clone()
            ),
            (
                "worker.credential_rotated".to_owned(),
                "worker".to_owned(),
                worker_id
            ),
        ]
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn asking_again_replaces_an_unused_new_credential() {
    let pool = pool().await;
    let worker = worker(&pool).await;
    issue_next_credential(&pool, worker, &format!("first-{worker}"))
        .await
        .unwrap()
        .expect("issued");
    issue_next_credential(&pool, worker, &format!("second-{worker}"))
        .await
        .unwrap()
        .expect("issued again");

    assert_eq!(
        authenticate_worker(&pool, &format!("first-{worker}"))
            .await
            .unwrap(),
        None,
        "the replaced one never works"
    );
    assert_eq!(
        authenticate_worker(&pool, &format!("second-{worker}"))
            .await
            .unwrap(),
        Some(worker)
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn two_requests_presenting_the_new_credential_at_once_both_succeed() {
    let pool = pool().await;
    let worker = worker(&pool).await;
    let new = format!("new-{worker}");
    issue_next_credential(&pool, worker, &new)
        .await
        .unwrap()
        .expect("issued");

    let (first, second) = tokio::join!(
        authenticate_worker(&pool, &new),
        authenticate_worker(&pool, &new)
    );
    assert_eq!(first.unwrap(), Some(worker));
    assert_eq!(second.unwrap(), Some(worker));
    let rotated: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events \
         WHERE target_id = $1 AND action = 'worker.credential_rotated'",
    )
    .bind(worker.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rotated, 1, "promoted once");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_revoked_worker_is_issued_nothing_and_its_new_credential_is_refused() {
    let pool = pool().await;
    let worker = worker(&pool).await;
    let new = format!("new-{worker}");
    issue_next_credential(&pool, worker, &new)
        .await
        .unwrap()
        .expect("issued");
    revoke_worker(&pool, worker, "alice").await.unwrap();

    assert_eq!(authenticate_worker(&pool, &new).await.unwrap(), None);
    assert_eq!(
        issue_next_credential(&pool, worker, &format!("later-{worker}"))
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        issue_next_credential(&pool, WorkerId::generate(), "nobody")
            .await
            .unwrap(),
        None,
        "nor is a worker that does not exist"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn rotation_is_due_when_asked_for_or_when_the_credential_is_old() {
    let pool = pool().await;
    let worker = worker(&pool).await;
    assert!(!credential_rotation_due(&pool, worker, 90).await.unwrap());

    // An administrator asks: due whatever its age, until it is rotated.
    let asked = request_credential_rotation(&pool, worker, "alice")
        .await
        .unwrap();
    let WorkerChange::Done(record) = &asked else {
        panic!("{asked:?}");
    };
    let requested_at = record.credential_rotation_requested_at.expect("requested");
    assert!(credential_rotation_due(&pool, worker, 0).await.unwrap());
    // Asking again keeps the first request's time.
    let again = request_credential_rotation(&pool, worker, "bob")
        .await
        .unwrap();
    let WorkerChange::Done(record) = &again else {
        panic!("{again:?}");
    };
    assert_eq!(record.credential_rotation_requested_at, Some(requested_at));

    // Rotating answers the request.
    issue_next_credential(&pool, worker, &format!("new-{worker}"))
        .await
        .unwrap()
        .expect("issued");
    authenticate_worker(&pool, &format!("new-{worker}"))
        .await
        .unwrap()
        .expect("promoted");
    assert!(!credential_rotation_due(&pool, worker, 90).await.unwrap());
    assert_eq!(
        get_worker(&pool, worker)
            .await
            .unwrap()
            .unwrap()
            .credential_rotation_requested_at,
        None
    );

    // An old credential is due by age, unless age is not a reason.
    sqlx::query(
        "UPDATE workers SET credential_issued_at = now() - interval '91 days' WHERE id = $1",
    )
    .bind(worker.as_uuid())
    .execute(&pool)
    .await
    .unwrap();
    assert!(credential_rotation_due(&pool, worker, 90).await.unwrap());
    assert!(!credential_rotation_due(&pool, worker, 0).await.unwrap());
    assert!(!credential_rotation_due(&pool, worker, 120).await.unwrap());

    let actions: Vec<String> = audited(&pool, worker)
        .await
        .into_iter()
        .map(|(action, _, _)| action)
        .collect();
    assert_eq!(
        actions,
        [
            "worker.credential_rotation_requested",
            "worker.credential_rotation_requested",
            "worker.credential_issued",
            "worker.credential_rotated",
        ]
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_revoked_worker_cannot_be_asked_to_rotate() {
    let pool = pool().await;
    let worker = worker(&pool).await;
    revoke_worker(&pool, worker, "alice").await.unwrap();
    assert_eq!(
        request_credential_rotation(&pool, worker, "alice")
            .await
            .unwrap(),
        WorkerChange::Refused("revoked".to_owned())
    );
    assert_eq!(
        request_credential_rotation(&pool, WorkerId::generate(), "alice")
            .await
            .unwrap(),
        WorkerChange::NotFound
    );
}
