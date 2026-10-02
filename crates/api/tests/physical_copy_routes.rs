// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The disc inventory, through the real router.
//!
//! The discs here are produced the way real ones are: by completing a burn
//! attempt through the worker protocol. Nothing inserts a physical copy
//! directly, because the database refuses that anyway: a disc record without
//! a write behind it is the thing the trigger exists to prevent.
//!
//! What is asserted is mostly what cannot be done. A disc cannot be called
//! verified by typing the word, a destroyed disc accepts nothing further, and
//! a check that fails takes the disc's condition with it in the same
//! transaction.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use sqlx::PgPool;
use tangible_api::{ApiState, router};
use tangible_db::{Database, DbConfig};
use tower::ServiceExt as _;

mod support;

/// Serialises tests that let a worker claim: a claim takes any queued job.
static QUEUE: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

async fn exclusive_queue() -> tokio::sync::MutexGuard<'static, ()> {
    QUEUE
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

struct Harness {
    session: support::Session,
    router: axum::Router,
    pool: PgPool,
    _queue: tokio::sync::MutexGuard<'static, ()>,
}

async fn harness() -> Harness {
    let queue = exclusive_queue().await;
    let url = std::env::var("TANGIBLE_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TANGIBLE_DATABASE_URL"))
        .expect("set TANGIBLE_TEST_DATABASE_URL to run integration tests");
    let database = Database::connect(&DbConfig::new(url))
        .await
        .expect("connect");
    database.migrate().await.expect("migrate");
    let pool = database.pool().clone();

    sqlx::query(
        "UPDATE burn_jobs SET state = 'canceled', completed_at = now() WHERE state = 'queued'",
    )
    .execute(&pool)
    .await
    .expect("drain the burn queue");

    let session = support::administrator(&pool).await;
    Harness {
        session,
        router: router(ApiState::new(database)),
        pool,
        _queue: queue,
    }
}

impl Harness {
    async fn send(&self, mut request: Request<Body>) -> (StatusCode, serde_json::Value) {
        self.session.apply(&mut request);
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("route the request");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("read the body");
        let body = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).expect("a JSON body")
        };
        (status, body)
    }

    async fn get(&self, path: &str) -> (StatusCode, serde_json::Value) {
        self.send(
            Request::builder()
                .method("GET")
                .uri(path)
                .body(Body::empty())
                .expect("build"),
        )
        .await
    }

    async fn send_json(
        &self,
        method: &str,
        path: &str,
        body: serde_json::Value,
        credential: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if let Some(credential) = credential {
            request = request.header("authorization", format!("Bearer {credential}"));
        }
        self.send(request.body(Body::from(body.to_string())).expect("build"))
            .await
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        self.send_json("POST", path, body, None).await
    }

    async fn patch(&self, path: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        self.send_json("PATCH", path, body, None).await
    }

    /// Burn a disc the way one is really made, and return its identifier.
    ///
    /// `verification` is what the worker reports finding: `match` produces a
    /// verified disc, `mismatch` one that exists and is wrong.
    ///
    /// Long because the whole chain is real. Nothing inserts a physical copy
    /// directly (the database refuses that), so a disc has to be produced by
    /// seeding a catalog, queueing a burn, enrolling a worker, claiming, and
    /// completing.
    #[allow(clippy::too_many_lines)]
    async fn burn_a_disc(&self, verification: &str) -> String {
        let ids: Vec<uuid::Uuid> = (0..6).map(|_| uuid::Uuid::now_v7()).collect();
        let digest = uuid::Uuid::now_v7().simple().to_string().repeat(2);

        for (statement, binds) in [
            (
                "INSERT INTO cas_objects (sha256, size_bytes) VALUES ($1, 10)",
                vec![],
            ),
            (
                "INSERT INTO titles (id, display_title, sort_title) VALUES ($1, 'T', 'T')",
                vec![ids[0]],
            ),
            (
                "INSERT INTO editions (id, title_id, display_name) VALUES ($1, $2, 'E')",
                vec![ids[1], ids[0]],
            ),
            (
                "INSERT INTO disc_sets (id, edition_id, name) VALUES ($1, $2, 'S')",
                vec![ids[2], ids[1]],
            ),
            (
                "INSERT INTO discs (id, disc_set_id, sequence_number) VALUES ($1, $2, 1)",
                vec![ids[3], ids[2]],
            ),
            (
                "INSERT INTO artifacts
                     (id, origin, manifest_version, total_bytes, component_count, validation_state)
                 VALUES ($1, 'imported_original', 'v1alpha1', 10, 1, 'valid')",
                vec![ids[4]],
            ),
            (
                "INSERT INTO burn_jobs (id, disc_id, artifact_id, created_by, priority)
                 VALUES ($1, $2, $3, 'test', 100)",
                vec![ids[5], ids[3], ids[4]],
            ),
        ] {
            let mut query = sqlx::query(statement);
            if binds.is_empty() {
                query = query.bind(&digest);
            }
            for id in binds {
                query = query.bind(id);
            }
            query.execute(&self.pool).await.expect("seed");
        }

        // Enroll a worker and let it claim, so the attempt is real.
        let secret = format!("tgw_enroll_{}", uuid::Uuid::now_v7().simple());
        let hash = {
            use sha2::Digest as _;
            let mut hasher = sha2::Sha256::new();
            hasher.update(secret.as_bytes());
            hex::encode(hasher.finalize())
        };
        sqlx::query(
            "INSERT INTO worker_enrollments (id, token_hash, expires_at, created_by)
             VALUES ($1, $2, now() + interval '1 hour', 'test')",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(hash)
        .execute(&self.pool)
        .await
        .expect("issue a token");

        let (_, enrolled) = self
            .post(
                "/api/v1/worker-enrollments/consume",
                json!({
                    "enrollment_token": secret,
                    "name": format!("copies-{}", uuid::Uuid::now_v7()),
                    "protocol_versions": ["1alpha1"],
                    "software_version": "0.1.0",
                }),
            )
            .await;
        let worker_id = enrolled["worker_id"].as_str().expect("worker").to_owned();
        let credential = enrolled["credential"]
            .as_str()
            .expect("credential")
            .to_owned();

        let drive = uuid::Uuid::now_v7();
        sqlx::query(
            "INSERT INTO drives (id, worker_id, configured_name, device_alias)
             VALUES ($1, $2::uuid, 'd', '/dev/disc-block')",
        )
        .bind(drive)
        .bind(&worker_id)
        .execute(&self.pool)
        .await
        .expect("attach a drive");

        let (status, lease) = self
            .send_json(
                "POST",
                &format!("/api/v1/workers/{worker_id}/claims"),
                json!({ "drive_id": drive, "engine": "fake", "engine_version": "0.1.0" }),
                Some(&credential),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{lease}");
        let attempt = lease["attempt_id"].as_str().expect("attempt").to_owned();
        let token = lease["lease_token"].as_str().expect("lease").to_owned();

        let (status, completion) = self
            .send_json(
                "POST",
                &format!("/api/v1/burn-attempts/{attempt}/complete"),
                json!({
                    "lease_token": token,
                    "last_sequence": 1,
                    "write_report": {
                        "state": "success",
                        "engine": "fake",
                        "engine_version": "0.1.0",
                        "started_at": "2026-01-01T00:00:00Z",
                        "completed_at": "2026-01-01T00:10:00Z",
                    },
                    "verification_report": {
                        "policy": "full_sector_readback",
                        "state": verification,
                        "bytes_read": 10,
                        "expected_sha256": "a".repeat(64),
                        "observed_sha256": "a".repeat(64),
                    },
                    "physical_medium": { "profile": "bd-r-25", "manufacturer_id": "SIMULATED" },
                }),
                Some(&credential),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{completion}");
        completion["physical_copy_id"]
            .as_str()
            .expect("a physical copy")
            .to_owned()
    }
}

// --- reading -------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_burned_disc_appears_in_the_inventory() {
    let harness = harness().await;
    let copy = harness.burn_a_disc("match").await;

    let (status, body) = harness
        .get(&format!("/api/v1/physical-copies/{copy}"))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "verified");
    assert_eq!(body["media_profile"], "bd-r-25");
    assert_eq!(body["manufacturer_id"], "SIMULATED");
    assert_eq!(body["should_be_destroyed"], false);
    assert_eq!(body["check_count"], 0);
    assert!(
        body["burn_attempt_id"].is_string(),
        "the evidence of how it was made"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_disc_that_failed_verification_is_flagged_for_destruction() {
    // The whole reason a failed burn still records a disc: it must be
    // findable so it can be destroyed rather than shelved and reused.
    let harness = harness().await;
    let copy = harness.burn_a_disc("mismatch").await;

    let (_, body) = harness
        .get(&format!("/api/v1/physical-copies/{copy}"))
        .await;
    assert_eq!(body["status"], "verification_failed");
    assert_eq!(body["should_be_destroyed"], true);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_inventory_can_be_filtered_by_condition() {
    let harness = harness().await;
    let good = harness.burn_a_disc("match").await;

    let (status, page) = harness.get("/api/v1/physical-copies?status=verified").await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = page["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|item| item["id"].as_str())
        .collect();
    assert!(ids.contains(&good.as_str()), "{page}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_unknown_condition_filter_is_refused() {
    let harness = harness().await;
    let (status, problem) = harness.get("/api/v1/physical-copies?status=melting").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["code"], "INVALID_PARAMETER");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_unknown_disc_is_not_found() {
    let harness = harness().await;
    let missing = uuid::Uuid::now_v7();
    let (status, problem) = harness
        .get(&format!("/api/v1/physical-copies/{missing}"))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(problem["code"], "NOT_FOUND");
}

// --- recording what an operator knows --------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_disc_can_be_labelled_and_shelved() {
    let harness = harness().await;
    let copy = harness.burn_a_disc("match").await;

    let (status, body) = harness
        .patch(
            &format!("/api/v1/physical-copies/{copy}"),
            json!({ "label": "Example Disc 1 of 2", "storage_location": "Shelf B, sleeve 14" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["label"], "Example Disc 1 of 2");
    assert_eq!(body["storage_location"], "Shelf B, sleeve 14");
    // Untouched fields stay as they were.
    assert_eq!(body["status"], "verified");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_absent_field_leaves_what_it_names_alone() {
    // A client editing a label must not blank a storage location it never
    // saw.
    let harness = harness().await;
    let copy = harness.burn_a_disc("match").await;
    let path = format!("/api/v1/physical-copies/{copy}");

    harness
        .patch(&path, json!({ "storage_location": "Shelf B" }))
        .await;
    let (_, body) = harness.patch(&path, json!({ "label": "Disc 1" })).await;
    assert_eq!(body["storage_location"], "Shelf B");
    assert_eq!(body["label"], "Disc 1");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn nothing_becomes_verified_by_typing_the_word() {
    // The strongest claim in the system must stay the most expensive one to
    // make: it means a disc was read back and matched.
    let harness = harness().await;
    let copy = harness.burn_a_disc("mismatch").await;

    let (status, problem) = harness
        .patch(
            &format!("/api/v1/physical-copies/{copy}"),
            json!({ "status": "verified" }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert_eq!(problem["code"], "VALIDATION_FAILED");
    assert!(
        problem["detail"]
            .as_str()
            .expect("detail")
            .contains("record a check"),
        "the refusal must say what to do instead: {problem}"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_operator_may_record_what_happened_to_a_disc() {
    let harness = harness().await;
    let copy = harness.burn_a_disc("match").await;

    for condition in ["degraded", "lost", "unknown"] {
        let (status, body) = harness
            .patch(
                &format!("/api/v1/physical-copies/{copy}"),
                json!({ "status": condition }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{condition}: {body}");
        assert_eq!(body["status"], condition);
    }
}

// --- checks -----------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_check_that_passes_is_what_makes_a_disc_verified() {
    let harness = harness().await;
    let copy = harness.burn_a_disc("match").await;
    // Take it out of verified first, so the check is doing the work.
    harness
        .patch(
            &format!("/api/v1/physical-copies/{copy}"),
            json!({ "status": "unknown" }),
        )
        .await;

    let (status, body) = harness
        .post(
            &format!("/api/v1/physical-copies/{copy}/checks"),
            json!({
                "method": "full_sector_readback",
                "result": "passed",
                "observed_sha256": "b".repeat(64),
                "bytes_read": 10,
                "checked_with": "PIONEER BDR-212",
                "notes": "read cleanly",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "verified");
    assert_eq!(body["check_count"], 1);
    assert!(body["last_checked_at"].is_string());
    assert_eq!(body["checks"][0]["result"], "passed");
    assert_eq!(body["checks"][0]["checked_with"], "PIONEER BDR-212");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_check_that_fails_takes_the_discs_condition_with_it() {
    // A check saying the disc failed, next to a record still calling it good,
    // is exactly the inconsistency the inventory exists to prevent.
    let harness = harness().await;
    let copy = harness.burn_a_disc("match").await;

    let (status, body) = harness
        .post(
            &format!("/api/v1/physical-copies/{copy}/checks"),
            json!({ "method": "full_sector_readback", "result": "failed" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "verification_failed");
    assert_eq!(body["should_be_destroyed"], true);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_inconclusive_check_leaves_the_condition_alone() {
    // It establishes neither that the disc is good nor that it is bad, and
    // overwriting a known condition with it would lose information.
    let harness = harness().await;
    let copy = harness.burn_a_disc("match").await;

    let (_, body) = harness
        .post(
            &format!("/api/v1/physical-copies/{copy}/checks"),
            json!({ "method": "filesystem_compare", "result": "partial" }),
        )
        .await;
    assert_eq!(body["status"], "verified");
    assert_eq!(body["check_count"], 1, "the check is still recorded");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn checks_accumulate_newest_first() {
    // A disc that read cleanly a year ago and fails today is the case the
    // inventory exists to catch, so both readings have to survive.
    let harness = harness().await;
    let copy = harness.burn_a_disc("match").await;
    let path = format!("/api/v1/physical-copies/{copy}/checks");

    harness
        .post(
            &path,
            json!({ "method": "full_sector_readback", "result": "passed" }),
        )
        .await;
    let (_, body) = harness
        .post(
            &path,
            json!({ "method": "full_sector_readback", "result": "failed" }),
        )
        .await;

    assert_eq!(body["check_count"], 2);
    assert_eq!(body["checks"][0]["result"], "failed", "newest first");
    assert_eq!(body["checks"][1]["result"], "passed");
    assert_eq!(body["status"], "verification_failed");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_check_this_server_does_not_know_is_refused() {
    let harness = harness().await;
    let copy = harness.burn_a_disc("match").await;

    for body in [
        json!({ "method": "sniff_test", "result": "passed" }),
        json!({ "method": "full_sector_readback", "result": "probably" }),
        json!({ "method": "full_sector_readback", "result": "passed", "observed_sha256": "ABC" }),
    ] {
        let (status, problem) = harness
            .post(&format!("/api/v1/physical-copies/{copy}/checks"), body)
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
        assert_eq!(problem["code"], "VALIDATION_FAILED");
    }
}

// --- destruction -------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_destroyed_disc_keeps_its_record_and_accepts_nothing_further() {
    // The record stays so the history of what was burned remains true; the
    // disc does not, so nothing further can be recorded against it.
    let harness = harness().await;
    let copy = harness.burn_a_disc("mismatch").await;

    let (status, body) = harness
        .post(
            &format!("/api/v1/physical-copies/{copy}/mark-destroyed"),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "destroyed");
    assert_eq!(body["is_settled"], true);

    let (status, problem) = harness
        .patch(
            &format!("/api/v1/physical-copies/{copy}"),
            json!({ "storage_location": "Shelf C" }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");

    let (status, problem) = harness
        .post(
            &format!("/api/v1/physical-copies/{copy}/checks"),
            json!({ "method": "full_sector_readback", "result": "passed" }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");

    // And the record is still there, still saying what it was.
    let (status, body) = harness
        .get(&format!("/api/v1/physical-copies/{copy}"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["media_profile"], "bd-r-25");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn destroying_a_disc_twice_is_not_an_error() {
    // What the operator asked for is already true.
    let harness = harness().await;
    let copy = harness.burn_a_disc("match").await;
    let path = format!("/api/v1/physical-copies/{copy}/mark-destroyed");

    harness.post(&path, json!({})).await;
    let (status, body) = harness.post(&path, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "destroyed");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_recorded_check_cannot_be_rewritten() {
    // Enforced by the database, not merely by the absence of a route: a
    // rewritten check would change the account of what somebody observed.
    let harness = harness().await;
    let copy = harness.burn_a_disc("match").await;
    harness
        .post(
            &format!("/api/v1/physical-copies/{copy}/checks"),
            json!({ "method": "full_sector_readback", "result": "failed" }),
        )
        .await;

    let refused = sqlx::query(
        "UPDATE physical_copy_checks SET result = 'passed' WHERE physical_copy_id = $1::uuid",
    )
    .bind(&copy)
    .execute(&harness.pool)
    .await;
    assert!(refused.is_err(), "the database must refuse the rewrite");
}
