// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! What diagnostics reads, against a real PostgreSQL.
//!
//! The database is shared with every other test, so each test looks only for
//! the rows it wrote, which carry codes and names nothing else uses.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use sqlx::{Executor as _, PgPool};
use tangible_db::diagnostics::{FailureKind, list_workers, recent_failures};
use tangible_db::{Database, DbConfig};

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

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn workers_come_with_their_engines_and_versions() {
    let pool = pool().await;
    let id = uuid::Uuid::now_v7();
    let version = format!("diag-{}", id.simple());
    pool.execute(
        format!(
            "INSERT INTO workers (id, name, status, software_version, protocol_version,
                                  credential_hash, capabilities_json, last_seen_at)
             VALUES ('{id}', 'diag-{id}', 'online', '{version}', '1alpha1', 'hash-{id}',
                     '{{\"engines\": [{{\"name\": \"xorriso\", \"version\": \"1.5.6\"}}]}}', now())"
        )
        .as_str(),
    )
    .await
    .expect("insert worker");

    let workers = list_workers(&pool).await.expect("list workers");
    let worker = workers
        .iter()
        .find(|worker| worker.software_version == version)
        .expect("the worker is listed");
    assert_eq!(worker.status, "online");
    assert_eq!(worker.protocol_version, "1alpha1");
    assert!(worker.last_seen_at.is_some());
    assert_eq!(
        worker.engines,
        serde_json::json!([{ "name": "xorriso", "version": "1.5.6" }])
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_worker_that_reported_no_engines_has_an_empty_list() {
    let pool = pool().await;
    let id = uuid::Uuid::now_v7();
    let version = format!("diag-{}", id.simple());
    pool.execute(
        format!(
            "INSERT INTO workers (id, name, software_version, protocol_version, credential_hash)
             VALUES ('{id}', 'diag-{id}', '{version}', '1alpha1', 'hash-{id}')"
        )
        .as_str(),
    )
    .await
    .expect("insert worker");

    let workers = list_workers(&pool).await.expect("list workers");
    let worker = workers
        .iter()
        .find(|worker| worker.software_version == version)
        .expect("the worker is listed");
    assert_eq!(worker.status, "pending");
    assert_eq!(worker.engines, serde_json::json!([]));
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn failures_are_codes_and_times_newest_first() {
    let pool = pool().await;
    let tag = uuid::Uuid::now_v7().simple().to_string().to_uppercase();
    let older = format!("DIAG_{tag}_OLDER_FAILED");
    let newer = format!("DIAG_{tag}_NEWER_FAILED");
    // In the future, so nothing another test writes meanwhile is newer.
    for (code, offset) in [(&older, "1 hour"), (&newer, "2 hours")] {
        pool.execute(
            format!(
                "INSERT INTO import_jobs (id, source_kind, source_descriptor_json, state,
                                          error_code, error_detail, created_by, updated_at)
                 VALUES ('{}', 'upload', '{{}}', 'failed_terminal', '{code}',
                         'detail that must not appear', 'test', now() + interval '{offset}')",
                uuid::Uuid::now_v7()
            )
            .as_str(),
        )
        .await
        .expect("insert failed import");
    }

    let failures = recent_failures(&pool, 1000).await.expect("recent failures");
    pool.execute(
        format!("DELETE FROM import_jobs WHERE error_code IN ('{older}', '{newer}')").as_str(),
    )
    .await
    .expect("remove the test's imports");

    let position = |code: &str| {
        failures
            .iter()
            .position(|failure| failure.code == code)
            .unwrap_or_else(|| panic!("{code} is among the recent failures"))
    };
    let (newer_at, older_at) = (position(&newer), position(&older));
    assert!(newer_at < older_at, "newest first");
    assert_eq!(failures[newer_at].kind, FailureKind::Import);
    assert!(failures[newer_at].at > failures[older_at].at);
    assert!(
        failures.windows(2).all(|pair| pair[0].at >= pair[1].at),
        "ordered by time throughout"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_negative_limit_returns_nothing() {
    let pool = pool().await;
    assert!(recent_failures(&pool, -1).await.expect("query").is_empty());
}
