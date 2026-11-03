// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The dashboard, through the real router (docs/plans/dashboard.md).
//!
//! The database is shared, so counts are whatever every test has left. What
//! is asserted is what this test planted: a burn job needing attention and
//! an ended attempt dated far ahead, so they lead the newest-first lists,
//! and workers that can only add to the counts they belong in.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use sqlx::{Executor as _, PgPool};
use tangible_api::{ApiState, router};
use tangible_domain::Role;
use tower::ServiceExt as _;

mod support;

async fn get(router: &axum::Router) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/dashboard")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// A title, disc, artifact, worker and drive; a burn job needing attention
/// and an ended attempt, both dated a century ahead. Answers the job's id.
async fn planted(pool: &PgPool, title: &str) -> uuid::Uuid {
    let id = || uuid::Uuid::now_v7();
    let (title_id, edition, set, disc, artifact, worker, drive, job, attempt) =
        (id(), id(), id(), id(), id(), id(), id(), id(), id());
    let digest = id().simple().to_string().repeat(2);
    for statement in [
        format!("INSERT INTO cas_objects (sha256, size_bytes) VALUES ('{digest}', 10)"),
        format!(
            "INSERT INTO titles (id, display_title, sort_title) VALUES ('{title_id}', '{title}', '{title}')"
        ),
        format!(
            "INSERT INTO editions (id, title_id, display_name) VALUES ('{edition}', '{title_id}', 'E')"
        ),
        format!("INSERT INTO disc_sets (id, edition_id, name) VALUES ('{set}', '{edition}', 'S')"),
        format!(
            "INSERT INTO discs (id, disc_set_id, sequence_number) VALUES ('{disc}', '{set}', 2)"
        ),
        format!(
            "INSERT INTO artifacts (id, origin, manifest_version, total_bytes, component_count, \
             validation_state) VALUES ('{artifact}', 'imported_original', 'v1alpha1', 10, 1, 'valid')"
        ),
        format!(
            "INSERT INTO workers (id, name, status, software_version, protocol_version, \
             credential_hash, last_seen_at) VALUES ('{worker}', 'dash-{worker}', 'online', '0.1.0', \
             '1alpha1', 'hash-{worker}', now() - interval '2 hours')"
        ),
        format!(
            "INSERT INTO drives (id, worker_id, configured_name, device_alias) \
             VALUES ('{drive}', '{worker}', 'd1', '/dev/disc-block')"
        ),
        format!(
            "INSERT INTO burn_jobs (id, disc_id, artifact_id, state, created_by, updated_at) \
             VALUES ('{job}', '{disc}', '{artifact}', 'needs_attention', 'test', \
                     now() + interval '100 years')"
        ),
        format!(
            "INSERT INTO burn_attempts (id, burn_job_id, worker_id, drive_id, attempt_number, \
             state, lease_token_hash, lease_expires_at, engine, engine_version, plan_json, \
             error_code, ended_at) VALUES ('{attempt}', '{job}', '{worker}', '{drive}', 1, \
             'verification_failed', 'lease-{attempt}', now(), 'fake', '0.1.0', '{{}}', \
             'VERIFY_MISMATCH', now() + interval '100 years')"
        ),
    ] {
        pool.execute(statement.as_str())
            .await
            .unwrap_or_else(|error| panic!("{statement}: {error}"));
    }
    job
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_dashboard_says_what_needs_doing_to_anyone_signed_in() {
    let database = support::database().await;
    let title = format!("Dashboard {}", uuid::Uuid::now_v7().simple());
    let job = planted(database.pool(), &title).await;

    let session = support::sign_in(database.pool(), Role::Viewer).await;
    let router = support::signed_in(router(ApiState::new(database.clone())), session);
    let (status, board) = get(&router).await;
    assert_eq!(status, StatusCode::OK, "{board}");

    let attention = &board["attention"][0];
    assert_eq!(attention["burn_job_id"], job.to_string());
    assert_eq!(attention["title"], title.as_str());
    assert_eq!(attention["disc_number"], 2);
    assert_eq!(attention["state"], "needs_attention");
    assert_eq!(
        attention["error_code"], "VERIFY_MISMATCH",
        "its latest attempt's"
    );
    let recent = &board["recent_burns"][0];
    assert_eq!(recent["burn_job_id"], job.to_string());
    assert_eq!(recent["state"], "verification_failed");

    assert!(board["burns"]["needing_attention"].as_i64().unwrap() >= 1);
    assert!(
        board["workers"]["silent"].as_i64().unwrap() >= 1,
        "online, unheard for hours"
    );
    assert!(board["storage"]["objects"].as_i64().unwrap() >= 1);
    assert!(board["recent_burns"].as_array().unwrap().len() <= 5);
    for count in ["active", "failed"] {
        assert!(board["imports"][count].is_i64(), "{board}");
    }
    assert!(board["integrity"]["damaged"].is_i64());

    // Dated a century ahead, they would lead every dashboard after this one.
    for statement in [
        format!("DELETE FROM burn_attempts WHERE burn_job_id = '{job}'"),
        format!("DELETE FROM burn_jobs WHERE id = '{job}'"),
    ] {
        database.pool().execute(statement.as_str()).await.unwrap();
    }
}
