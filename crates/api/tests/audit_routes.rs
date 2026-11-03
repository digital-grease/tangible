// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The audit log, read through the real router (docs/plans/audit-log.md).
//!
//! The database is shared, so each test writes events about a worker of its
//! own, through the repository that writes them in production, and reads
//! them back by that worker's id.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tangible_api::{ApiState, router};
use tangible_domain::{Role, WorkerId};
use tower::ServiceExt as _;

mod support;

async fn get(router: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4 << 20)
        .await
        .unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}

/// A worker with a drain, an enable and a revocation on record.
async fn audited_worker(database: &tangible_db::Database) -> WorkerId {
    let worker = WorkerId::generate();
    sqlx::query(
        "INSERT INTO workers (id, name, software_version, protocol_version, credential_hash)
         VALUES ($1, $2, '0.1.0', '1alpha1', $3)",
    )
    .bind(worker.as_uuid())
    .bind(format!("audit-{worker}"))
    .bind(format!("hash-{worker}"))
    .execute(database.pool())
    .await
    .unwrap();
    let pool = database.pool();
    tangible_db::workers::drain_worker(pool, worker, "alice")
        .await
        .unwrap();
    tangible_db::workers::enable_worker(pool, worker, "alice")
        .await
        .unwrap();
    tangible_db::workers::revoke_worker(pool, worker, "bob")
        .await
        .unwrap();
    worker
}

fn actions(page: &Value) -> Vec<&str> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["action"].as_str().unwrap())
        .collect()
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_administrator_reads_who_did_what_newest_first() {
    let database = support::database().await;
    let session = support::administrator(database.pool()).await;
    let router = support::signed_in(router(ApiState::new(database.clone())), session);
    let worker = audited_worker(&database).await;

    let (status, page) = get(&router, &format!("/api/v1/audit-events?target_id={worker}")).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(
        actions(&page),
        ["worker.revoked", "worker.enabled", "worker.drained"]
    );
    let newest = &page["items"][0];
    assert_eq!(newest["actor_type"], "user");
    assert_eq!(newest["actor_id"], "bob");
    assert_eq!(newest["target_type"], "worker");
    assert_eq!(newest["outcome"], "success");
    assert_eq!(newest["metadata"]["from"], "pending", "never heard from");

    // Filtered by action, by a kind of action, by who.
    let by = |query: String| {
        let router = router.clone();
        async move {
            get(
                &router,
                &format!("/api/v1/audit-events?target_id={worker}&{query}"),
            )
            .await
        }
    };
    assert_eq!(
        actions(&by("action=worker.enabled".to_owned()).await.1),
        ["worker.enabled"]
    );
    assert_eq!(actions(&by("action=worker.".to_owned()).await.1).len(), 3);
    assert_eq!(actions(&by("action=work".to_owned()).await.1).len(), 0);
    assert_eq!(actions(&by("actor_id=alice".to_owned()).await.1).len(), 2);

    // A page at a time, older behind the cursor.
    let (_, first) = by("limit=1".to_owned()).await;
    assert_eq!(actions(&first), ["worker.revoked"]);
    let cursor = first["next_cursor"].as_str().unwrap();
    let (_, second) = by(format!("limit=2&cursor={cursor}")).await;
    assert_eq!(actions(&second), ["worker.enabled", "worker.drained"]);

    // One event, and one that is not there.
    let id = newest["id"].as_str().unwrap();
    let (status, one) = get(&router, &format!("/api/v1/audit-events/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(one["action"], "worker.revoked");
    let (status, _) = get(
        &router,
        &format!("/api/v1/audit-events/{}", uuid::Uuid::now_v7()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A filter that will not do.
    let (status, refused) = by(format!("actor_id={}", "x".repeat(201))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn only_an_administrator_reads_the_audit_log() {
    let database = support::database().await;
    for role in [Role::Viewer, Role::Operator] {
        let session = support::sign_in(database.pool(), role).await;
        let router = support::signed_in(router(ApiState::new(database.clone())), session);
        let (status, body) = get(&router, "/api/v1/audit-events").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{role:?}: {body}");
    }
}
