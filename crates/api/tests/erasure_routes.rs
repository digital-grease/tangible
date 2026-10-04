// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Erasing the disc in a drive: asking, taking, finishing, and refusing.
//!
//! Through the real router and PostgreSQL, because the properties worth
//! asserting span all of it: only an administrator may ask, only the drive's
//! own worker may take or finish, a drive never erases and burns at once,
//! and every step is audited.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use tangible_api::{ApiState, router};
use tangible_domain::Role;
use tower::ServiceExt as _;

mod support;

struct Harness {
    router: axum::Router,
    pool: PgPool,
    admin: support::Session,
}

async fn harness() -> Harness {
    let database = support::database().await;
    let pool = database.pool().clone();
    let admin = support::administrator(&pool).await;
    Harness {
        router: router(ApiState::new(database)),
        pool,
        admin,
    }
}

enum As<'a> {
    Person(&'a support::Session),
    Worker(&'a str),
}

struct Worker {
    id: String,
    credential: String,
    drive: String,
}

impl Harness {
    async fn send(
        &self,
        method: &str,
        uri: &str,
        who: &As<'_>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(uri);
        match who {
            As::Person(session) => {
                request = request
                    .header("cookie", session.cookie())
                    .header("x-csrf-token", &session.csrf_token);
            }
            As::Worker(credential) => {
                request = request.header("authorization", format!("Bearer {credential}"));
            }
        }
        let body = match body {
            Some(body) => {
                request = request.header("content-type", "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = self
            .router
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        // Large: the shared test database holds every drive earlier runs made.
        let bytes = axum::body::to_bytes(response.into_body(), 64 << 20)
            .await
            .unwrap();
        // axum's own rejections, such as a missing field, are plain text.
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (status, value)
    }

    async fn admin(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.send(method, uri, &As::Person(&self.admin), body).await
    }

    /// An enrolled worker with one drive.
    async fn worker(&self) -> Worker {
        let secret = format!("tgw_enroll_{}", uuid::Uuid::now_v7().simple());
        let token_hash = {
            use sha2::Digest as _;
            hex::encode(sha2::Sha256::digest(secret.as_bytes()))
        };
        sqlx::query(
            "INSERT INTO worker_enrollments (id, token_hash, expires_at, created_by)
             VALUES ($1, $2, now() + interval '1 hour', 'test')",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(&token_hash)
        .execute(&self.pool)
        .await
        .unwrap();
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/worker-enrollments/consume")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "enrollment_token": secret,
                    "name": format!("erasure-{}", uuid::Uuid::now_v7()),
                    "protocol_versions": ["1alpha1"],
                    "software_version": "0.1.0",
                })
                .to_string(),
            ))
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap(),
        )
        .unwrap();
        let id = body["worker_id"].as_str().unwrap().to_owned();
        let drive = uuid::Uuid::now_v7();
        sqlx::query(
            "INSERT INTO drives (id, worker_id, configured_name, device_alias)
             VALUES ($1, $2::uuid, 'sr0', '/dev/sr0')",
        )
        .bind(drive)
        .bind(&id)
        .execute(&self.pool)
        .await
        .unwrap();
        Worker {
            id,
            credential: body["credential"].as_str().unwrap().to_owned(),
            drive: drive.to_string(),
        }
    }

    async fn request(&self, drive: &str) -> String {
        let (status, body) = self
            .admin(
                "POST",
                &format!("/api/v1/drives/{drive}/erasures"),
                Some(json!({ "mode": "quick", "confirm_data_loss": true })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_owned()
    }

    async fn claim(&self, worker: &Worker) -> (StatusCode, Value) {
        self.send(
            "POST",
            &format!("/api/v1/workers/{}/erasure-claims", worker.id),
            &As::Worker(&worker.credential),
            Some(json!({ "drive_id": worker.drive })),
        )
        .await
    }

    async fn complete(&self, worker: &Worker, erasure: &str, body: Value) -> (StatusCode, Value) {
        self.send(
            "POST",
            &format!("/api/v1/erasures/{erasure}/complete"),
            &As::Worker(&worker.credential),
            Some(body),
        )
        .await
    }

    async fn audited(&self, erasure: &str) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT action, actor_id FROM audit_events
             WHERE target_type = 'media_erasure' AND target_id = $1 ORDER BY created_at",
        )
        .bind(erasure)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_administrator_asks_the_worker_erases_and_every_step_is_audited() {
    let harness = harness().await;
    let worker = harness.worker().await;

    let (status, drives) = harness.admin("GET", "/api/v1/drives", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        drives["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|drive| drive["id"] == worker.drive.as_str())
    );

    let erasure = harness.request(&worker.drive).await;
    let (_, queued) = harness
        .admin("GET", &format!("/api/v1/erasures/{erasure}"), None)
        .await;
    assert_eq!(queued["state"], "queued");
    assert_eq!(queued["requested_by"], harness.admin.user.username.as_str());

    let (status, claim) = harness.claim(&worker).await;
    assert_eq!(status, StatusCode::OK, "{claim}");
    assert_eq!(claim["erasure_id"], erasure.as_str());
    assert_eq!(claim["mode"], "quick");

    let report = json!({
        "outcome": "erased",
        "medium": { "profile": "DVD-RW sequential recording", "blank": false, "rewritable": true, "sessions": 1 },
        "duration_seconds": 42,
    });
    let (status, done) = harness.complete(&worker, &erasure, report.clone()).await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["state"], "erased");
    assert_eq!(done["touched_the_disc"], true);
    assert_eq!(done["is_open"], false);
    assert_eq!(done["medium_before"]["sessions"], 1);
    assert_eq!(done["duration_seconds"], 42);

    // A lost response retried is the same answer, not a conflict.
    let (status, again) = harness.complete(&worker, &erasure, report).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["state"], "erased");
    let (status, _) = harness
        .complete(&worker, &erasure, json!({ "outcome": "failed" }))
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "an ending cannot be rewritten"
    );

    let actions = harness.audited(&erasure).await;
    assert_eq!(
        actions,
        vec![
            (
                "media_erasure.requested".to_owned(),
                harness.admin.user.username.clone()
            ),
            ("media_erasure.started".to_owned(), worker.id.clone()),
            ("media_erasure.completed".to_owned(), worker.id.clone()),
        ]
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_request_must_confirm_the_data_loss_and_name_a_known_mode_and_drive() {
    let harness = harness().await;
    let worker = harness.worker().await;
    let uri = format!("/api/v1/drives/{}/erasures", worker.drive);

    for body in [
        json!({ "mode": "quick", "confirm_data_loss": false }),
        json!({ "mode": "thorough", "confirm_data_loss": true }),
    ] {
        let (status, problem) = harness.admin("POST", &uri, Some(body.clone())).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(problem["code"], "VALIDATION_FAILED");
    }
    let (status, _) = harness
        .admin("POST", &uri, Some(json!({ "mode": "quick" })))
        .await;
    assert!(
        status.is_client_error(),
        "a missing confirmation is refused: {status}"
    );

    let (status, _) = harness
        .admin(
            "POST",
            &format!("/api/v1/drives/{}/erasures", uuid::Uuid::now_v7()),
            Some(json!({ "mode": "quick", "confirm_data_loss": true })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    harness.request(&worker.drive).await;
    let (status, problem) = harness
        .admin(
            "POST",
            &uri,
            Some(json!({ "mode": "full", "confirm_data_loss": true })),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "one open erasure per drive: {problem}"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn only_an_administrator_may_erase_though_anyone_signed_in_may_look() {
    let harness = harness().await;
    let worker = harness.worker().await;
    let erasure = harness.request(&worker.drive).await;
    let body = json!({ "mode": "quick", "confirm_data_loss": true });

    for role in [Role::Viewer, Role::Operator] {
        let session = support::sign_in(&harness.pool, role).await;
        let who = As::Person(&session);
        let (status, _) = harness.send("GET", "/api/v1/drives", &who, None).await;
        assert_eq!(status, StatusCode::OK, "{role}");
        let (status, _) = harness.send("GET", "/api/v1/erasures", &who, None).await;
        assert_eq!(status, StatusCode::OK, "{role}");

        let (status, problem) = harness
            .send(
                "POST",
                &format!("/api/v1/drives/{}/erasures", worker.drive),
                &who,
                Some(body.clone()),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{role}");
        assert_eq!(problem["code"], "FORBIDDEN");
        let (status, _) = harness
            .send(
                "POST",
                &format!("/api/v1/erasures/{erasure}/cancel"),
                &who,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{role}");
    }

    // And a worker credential is nobody on the operator side.
    let (status, _) = harness
        .send(
            "POST",
            &format!("/api/v1/drives/{}/erasures", worker.drive),
            &As::Worker(&worker.credential),
            Some(body),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn only_the_drives_own_worker_may_take_or_finish_an_erasure() {
    let harness = harness().await;
    let owner = harness.worker().await;
    let other = harness.worker().await;
    let erasure = harness.request(&owner.drive).await;

    // The other worker naming the owner's drive gets nothing, and learns
    // nothing about whether it exists.
    let (status, _) = harness
        .send(
            "POST",
            &format!("/api/v1/workers/{}/erasure-claims", other.id),
            &As::Worker(&other.credential),
            Some(json!({ "drive_id": owner.drive })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Its own drive has nothing queued.
    let (status, _) = harness.claim(&other).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = harness.claim(&owner).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = harness
        .complete(&other, &erasure, json!({ "outcome": "erased" }))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (_, still) = harness
        .admin("GET", &format!("/api/v1/erasures/{erasure}"), None)
        .await;
    assert_eq!(still["state"], "erasing");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_queued_erasure_can_be_withdrawn_and_a_running_one_cannot() {
    let harness = harness().await;
    let worker = harness.worker().await;

    let first = harness.request(&worker.drive).await;
    let (status, canceled) = harness
        .admin("POST", &format!("/api/v1/erasures/{first}/cancel"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(canceled["state"], "canceled");
    assert_eq!(canceled["touched_the_disc"], false);
    let (status, _) = harness.claim(&worker).await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "a withdrawn erasure is not taken"
    );

    let second = harness.request(&worker.drive).await;
    harness.claim(&worker).await;
    let (status, problem) = harness
        .admin("POST", &format!("/api/v1/erasures/{second}/cancel"), None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_worker_asking_again_mid_erase_has_abandoned_it_and_the_record_says_so() {
    // A worker asks for work only when it is doing nothing, so an erasure it
    // still holds is one it lost by restarting. The disc may be half erased,
    // and the operator has to hear that.
    let harness = harness().await;
    let worker = harness.worker().await;
    let erasure = harness.request(&worker.drive).await;
    harness.claim(&worker).await;

    let (status, _) = harness.claim(&worker).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, record) = harness
        .admin("GET", &format!("/api/v1/erasures/{erasure}"), None)
        .await;
    assert_eq!(record["state"], "failed");
    assert_eq!(record["error_code"], "WORKER_RESTARTED");
    assert_eq!(record["touched_the_disc"], true);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_drive_never_burns_while_it_erases() {
    let harness = harness().await;
    let worker = harness.worker().await;
    harness.request(&worker.drive).await;
    harness.claim(&worker).await;

    let (status, problem) = harness
        .send(
            "POST",
            &format!("/api/v1/workers/{}/claims", worker.id),
            &As::Worker(&worker.credential),
            Some(json!({ "drive_id": worker.drive, "engine": "fake", "engine_version": "0" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_worker_cannot_burn_with_a_drive_that_is_not_its_own() {
    let harness = harness().await;
    let owner = harness.worker().await;
    let other = harness.worker().await;
    let (status, _) = harness
        .send(
            "POST",
            &format!("/api/v1/workers/{}/claims", other.id),
            &As::Worker(&other.credential),
            Some(json!({ "drive_id": owner.drive, "engine": "fake", "engine_version": "0" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_worker_report_is_checked_before_it_is_recorded() {
    let harness = harness().await;
    let worker = harness.worker().await;
    let erasure = harness.request(&worker.drive).await;
    harness.claim(&worker).await;

    for body in [
        json!({ "outcome": "erasing" }),
        json!({ "outcome": "queued" }),
        json!({ "outcome": "canceled" }),
        json!({ "outcome": "failed", "error_code": "not a code" }),
    ] {
        let (status, _) = harness.complete(&worker, &erasure, body.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }
    let (status, done) = harness
        .complete(
            &worker,
            &erasure,
            json!({ "outcome": "refused", "error_code": "NOT_REWRITABLE",
                    "error_detail": "the disc in the drive is CD-R, which cannot be erased",
                    "medium": { "profile": "CD-R", "blank": false, "rewritable": false, "sessions": 1 } }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(done["state"], "refused");
    assert_eq!(done["touched_the_disc"], false);
}
