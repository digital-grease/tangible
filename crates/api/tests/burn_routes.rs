// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The operator-facing burn routes, exercised through the real router.
//!
//! These go through axum's routing, the repositories and a real PostgreSQL,
//! because what is worth asserting here spans all three: that a retried create
//! cannot spend a second disc, that a cancellation is refused once a laser is
//! running, and that a worker's events move a job without ever moving it
//! backwards past the point of no return.
//!
//! Fixtures bind their parameters rather than formatting them into the SQL.
//! Nothing here is untrusted, but test files get copied.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use sqlx::PgPool;
use tangible_api::{ApiState, router};
use tangible_db::{Database, DbConfig};
use tower::ServiceExt as _;

mod support;

/// Serialises the tests in this file: a claim takes any queued job rather
/// than one scoped to the caller, and a drain cancels every queued job.
static QUEUE: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

async fn exclusive_queue() -> tokio::sync::MutexGuard<'static, ()> {
    QUEUE
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Empty the claimable queue so a test sees only what it queues itself.
///
/// Cancelled rather than deleted: attempts reference their job with RESTRICT,
/// and a record of work performed must not vanish because someone tidied up.
async fn drain_queue(pool: &PgPool) {
    sqlx::query(
        "UPDATE burn_jobs SET state = 'canceled', completed_at = now() WHERE state = 'queued'",
    )
    .execute(pool)
    .await
    .expect("drain the queue");
}

struct Harness {
    session: support::Session,
    router: axum::Router,
    pool: PgPool,
    /// Held for the whole test. Creating a job inserts it and reads it back,
    /// and another test draining the queue between the two turned a fresh
    /// job into a canceled one about one run in five.
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
                .expect("build the request"),
        )
        .await
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        self.post_with(path, body, None).await
    }

    /// POST with an optional idempotency key or worker credential.
    async fn post_with(
        &self,
        path: &str,
        body: serde_json::Value,
        headers: Option<(&str, &str)>,
    ) -> (StatusCode, serde_json::Value) {
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json");
        if let Some((name, value)) = headers {
            request = request.header(name, value);
        }
        self.send(
            request
                .body(Body::from(body.to_string()))
                .expect("build the request"),
        )
        .await
    }
}

/// A catalog chain a burn can be queued against.
struct World {
    disc: String,
    artifact: String,
}

async fn seed(pool: &PgPool) -> World {
    let ids: Vec<uuid::Uuid> = (0..5).map(|_| uuid::Uuid::now_v7()).collect();
    let digest = uuid::Uuid::now_v7().simple().to_string().repeat(2);

    sqlx::query("INSERT INTO cas_objects (sha256, size_bytes) VALUES ($1, 10)")
        .bind(&digest)
        .execute(pool)
        .await
        .expect("seed a stored object");
    sqlx::query("INSERT INTO titles (id, display_title, sort_title) VALUES ($1, 'T', 'T')")
        .bind(ids[0])
        .execute(pool)
        .await
        .expect("seed a title");
    sqlx::query("INSERT INTO editions (id, title_id, display_name) VALUES ($1, $2, 'E')")
        .bind(ids[1])
        .bind(ids[0])
        .execute(pool)
        .await
        .expect("seed an edition");
    sqlx::query("INSERT INTO disc_sets (id, edition_id, name) VALUES ($1, $2, 'S')")
        .bind(ids[2])
        .bind(ids[1])
        .execute(pool)
        .await
        .expect("seed a disc set");
    sqlx::query("INSERT INTO discs (id, disc_set_id, sequence_number) VALUES ($1, $2, 1)")
        .bind(ids[3])
        .bind(ids[2])
        .execute(pool)
        .await
        .expect("seed a disc");
    sqlx::query(
        "INSERT INTO artifacts
             (id, origin, manifest_version, total_bytes, component_count, validation_state)
         VALUES ($1, 'imported_original', 'v1alpha1', 10, 1, 'valid')",
    )
    .bind(ids[4])
    .execute(pool)
    .await
    .expect("seed an artifact");

    World {
        disc: ids[3].to_string(),
        artifact: ids[4].to_string(),
    }
}

fn create_body(world: &World) -> serde_json::Value {
    json!({ "disc_id": world.disc, "artifact_id": world.artifact })
}

/// Priority that wins any race with a job another test queued.
///
/// Tests share one database, and a claim takes whatever is queued rather than
/// something scoped to the caller. Draining the queue under the lock is not
/// quite enough on its own: a test that does not claim can queue a job in the
/// window between the drain and the claim. Claims are ordered by priority
/// first, so a job queued at this priority is always the one a claiming test
/// receives.
const CLAIMABLE_FIRST: i64 = 100;

/// Queue a burn through the API and return its identifier.
async fn queue(harness: &Harness, world: &World) -> String {
    let (status, body) = harness.post("/api/v1/burn-jobs", create_body(world)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().expect("id").to_owned()
}

/// Queue a burn that a claiming test in this file will receive.
async fn queue_claimable(harness: &Harness, world: &World) -> String {
    let (status, body) = harness
        .post(
            "/api/v1/burn-jobs",
            json!({
                "disc_id": world.disc,
                "artifact_id": world.artifact,
                "priority": CLAIMABLE_FIRST,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().expect("id").to_owned()
}

/// Enroll a worker and return its identity, credential and a drive.
async fn worker(harness: &Harness) -> (String, String, String) {
    let secret = format!("tgw_enroll_{}", uuid::Uuid::now_v7().simple());
    let token_hash = {
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
    .bind(&token_hash)
    .execute(&harness.pool)
    .await
    .expect("issue an enrollment token");

    let (status, body) = harness
        .post(
            "/api/v1/worker-enrollments/consume",
            json!({
                "enrollment_token": secret,
                "name": format!("burn-routes-{}", uuid::Uuid::now_v7()),
                "protocol_versions": ["1alpha1"],
                "software_version": "0.1.0",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let worker_id = body["worker_id"].as_str().expect("worker_id").to_owned();
    let credential = body["credential"].as_str().expect("credential").to_owned();

    let drive = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO drives (id, worker_id, configured_name, device_alias)
         VALUES ($1, $2::uuid, 'd1', '/dev/disc-block')",
    )
    .bind(drive)
    .bind(&worker_id)
    .execute(&harness.pool)
    .await
    .expect("attach a drive");

    (worker_id, credential, drive.to_string())
}

/// Claim the test's own job as a worker, returning the attempt and lease token.
async fn claim(
    harness: &Harness,
    worker_id: &str,
    credential: &str,
    drive: &str,
    expected_job: &str,
) -> (String, String) {
    let (status, body) = harness
        .post_with(
            &format!("/api/v1/workers/{worker_id}/claims"),
            json!({ "drive_id": drive, "engine": "fake", "engine_version": "0.1.0" }),
            Some(("authorization", &format!("Bearer {credential}"))),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["burn_job_id"], expected_job,
        "the claim took another test's job; the queue was not exclusive"
    );
    (
        body["attempt_id"].as_str().expect("attempt_id").to_owned(),
        body["lease_token"]
            .as_str()
            .expect("lease_token")
            .to_owned(),
    )
}

/// Submit one stage event as a worker.
async fn report(
    harness: &Harness,
    credential: &str,
    attempt: &str,
    sequence: i64,
    stage: &str,
) -> StatusCode {
    let (status, _) = harness
        .post_with(
            &format!("/api/v1/burn-attempts/{attempt}/events"),
            json!({
                "events": [{
                    "sequence": sequence,
                    "event_type": "stage_changed",
                    "stage": stage,
                    "code": "STAGE_CHANGED",
                    "progress": null,
                    "data": {},
                    "worker_time": "2026-01-01T00:00:00Z",
                }]
            }),
            Some(("authorization", &format!("Bearer {credential}"))),
        )
        .await;
    status
}

// --- creation ------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_queued_burn_appears_in_the_queue() {
    let harness = harness().await;
    let world = seed(&harness.pool).await;

    let (status, body) = harness.post("/api/v1/burn-jobs", create_body(&world)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["state"], "queued");
    assert_eq!(body["attempt_count"], 0);
    assert_eq!(body["has_started_writing"], false);
    // The default policy reads the disc back. Anything else would make
    // skipping verification the thing that happens when nobody chose.
    assert_eq!(body["verification_policy"][0], "full_sector_readback");
    assert_eq!(body["eject_policy"], "eject_on_success");

    let id = body["id"].as_str().expect("id");
    let (status, listed) = harness
        .get(&format!("/api/v1/burn-jobs?artifact_id={}", world.artifact))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["items"][0]["id"], id);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn every_burn_is_told_that_target_compatibility_is_unknown() {
    // The claim the project refuses to make, said on the response that queues
    // the work rather than buried in documentation.
    let harness = harness().await;
    let world = seed(&harness.pool).await;

    let (_, body) = harness.post("/api/v1/burn-jobs", create_body(&world)).await;
    let warnings = body["warnings"].as_array().expect("warnings");
    assert!(
        warnings.iter().any(|line| line
            .as_str()
            .is_some_and(|line| line.contains("target compatibility is unknown"))),
        "{warnings:?}"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_retried_create_does_not_spend_a_second_disc() {
    // The reason creation takes an idempotency key at all. A dropped response
    // must not cost a disc.
    let harness = harness().await;
    let world = seed(&harness.pool).await;
    let key = format!("burn-{}", uuid::Uuid::now_v7());

    let (first_status, first) = harness
        .post_with(
            "/api/v1/burn-jobs",
            create_body(&world),
            Some(("idempotency-key", &key)),
        )
        .await;
    assert_eq!(first_status, StatusCode::CREATED, "{first}");

    let (second_status, second) = harness
        .post_with(
            "/api/v1/burn-jobs",
            create_body(&world),
            Some(("idempotency-key", &key)),
        )
        .await;
    // 200 rather than 201: the job existed already.
    assert_eq!(second_status, StatusCode::OK, "{second}");
    assert_eq!(first["id"], second["id"]);

    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM burn_jobs WHERE artifact_id = $1::uuid")
            .bind(&world.artifact)
            .fetch_one(&harness.pool)
            .await
            .expect("count");
    assert_eq!(count, 1, "a retry created a second burn job");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_reused_key_with_a_different_burn_is_refused() {
    // Answering with the first job would burn the wrong disc; creating a
    // second would defeat the key. Neither is acceptable, so it is an error.
    let harness = harness().await;
    let world = seed(&harness.pool).await;
    let other = seed(&harness.pool).await;
    let key = format!("burn-{}", uuid::Uuid::now_v7());

    harness
        .post_with(
            "/api/v1/burn-jobs",
            create_body(&world),
            Some(("idempotency-key", &key)),
        )
        .await;

    let (status, body) = harness
        .post_with(
            "/api/v1/burn-jobs",
            create_body(&other),
            Some(("idempotency-key", &key)),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "IDEMPOTENCY_CONFLICT");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_missing_artifact_is_named_rather_than_reported_as_a_constraint() {
    let harness = harness().await;
    let world = seed(&harness.pool).await;

    let (status, body) = harness
        .post(
            "/api/v1/burn-jobs",
            json!({ "disc_id": world.disc, "artifact_id": uuid::Uuid::now_v7().to_string() }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "REFERENCE_NOT_FOUND");
    assert!(
        body["detail"]
            .as_str()
            .expect("detail")
            .contains("artifact"),
        "{body}"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_missing_disc_is_named_rather_than_reported_as_a_constraint() {
    let harness = harness().await;
    let world = seed(&harness.pool).await;

    let (status, body) = harness
        .post(
            "/api/v1/burn-jobs",
            json!({ "disc_id": uuid::Uuid::now_v7().to_string(), "artifact_id": world.artifact }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "REFERENCE_NOT_FOUND");
    assert!(
        body["detail"].as_str().expect("detail").contains("disc"),
        "{body}"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_quarantined_artifact_is_not_burned() {
    let harness = harness().await;
    let world = seed(&harness.pool).await;
    sqlx::query("UPDATE artifacts SET quarantine_state = 'confirmed' WHERE id = $1::uuid")
        .bind(&world.artifact)
        .execute(&harness.pool)
        .await
        .expect("quarantine the artifact");

    let (status, body) = harness.post("/api/v1/burn-jobs", create_body(&world)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "VALIDATION_FAILED");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_artifact_awaiting_deletion_is_not_burned() {
    let harness = harness().await;
    let world = seed(&harness.pool).await;
    sqlx::query("UPDATE artifacts SET pending_deletion = TRUE WHERE id = $1::uuid")
        .bind(&world.artifact)
        .execute(&harness.pool)
        .await
        .expect("mark the artifact for deletion");

    let (status, body) = harness.post("/api/v1/burn-jobs", create_body(&world)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "VALIDATION_FAILED");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_empty_verification_policy_is_refused() {
    let harness = harness().await;
    let world = seed(&harness.pool).await;

    let (status, body) = harness
        .post(
            "/api/v1/burn-jobs",
            json!({
                "disc_id": world.disc,
                "artifact_id": world.artifact,
                "verification_policy": [],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "VALIDATION_FAILED");
}

// --- reading -------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_unknown_job_is_not_found_rather_than_empty() {
    let harness = harness().await;
    let missing = uuid::Uuid::now_v7();

    let (status, body) = harness.get(&format!("/api/v1/burn-jobs/{missing}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "NOT_FOUND");

    // An empty attempt list would read as "never attempted", which is a
    // different and wrong answer.
    let (status, _) = harness
        .get(&format!("/api/v1/burn-jobs/{missing}/attempts"))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_malformed_identifier_is_rejected_as_a_parameter() {
    let harness = harness().await;
    let (status, body) = harness.get("/api/v1/burn-jobs/not-a-uuid").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "INVALID_PARAMETER");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_forged_cursor_is_rejected() {
    let harness = harness().await;
    let (status, body) = harness.get("/api/v1/burn-jobs?cursor=!!!!").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "INVALID_CURSOR");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_queue_lists_newest_first_and_pages_backwards() {
    // An operator reads a queue to see what was just asked for, so the newest
    // job is the one that must not be buried.
    let harness = harness().await;
    let world = seed(&harness.pool).await;

    let first = queue(&harness, &world).await;
    let second = queue(&harness, &world).await;

    let (status, page) = harness
        .get(&format!(
            "/api/v1/burn-jobs?artifact_id={}&limit=1",
            world.artifact
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"][0]["id"], second);

    let cursor = page["next_cursor"].as_str().expect("a cursor");
    let (_, next) = harness
        .get(&format!(
            "/api/v1/burn-jobs?artifact_id={}&limit=1&cursor={cursor}",
            world.artifact
        ))
        .await;
    assert_eq!(next["items"][0]["id"], first);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_state_filter_only_accepts_states_this_server_knows() {
    let harness = harness().await;
    let (status, body) = harness.get("/api/v1/burn-jobs?state=melting").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "INVALID_PARAMETER");
}

// --- cancellation ----------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_queued_burn_can_be_cancelled() {
    let harness = harness().await;
    let world = seed(&harness.pool).await;
    let job = queue(&harness, &world).await;

    let (status, body) = harness
        .post(&format!("/api/v1/burn-jobs/{job}/cancel"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "canceled");
    assert!(body["completed_at"].is_string());
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn cancelling_twice_is_not_an_error() {
    // A repeated request asks for a state the job is already in. Reporting
    // that as a failure would invite an operator to press again.
    let harness = harness().await;
    let world = seed(&harness.pool).await;
    let job = queue(&harness, &world).await;

    harness
        .post(&format!("/api/v1/burn-jobs/{job}/cancel"), json!({}))
        .await;
    let (status, body) = harness
        .post(&format!("/api/v1/burn-jobs/{job}/cancel"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "canceled");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn cancelling_a_claimed_burn_stops_the_attempt_too() {
    // Nothing physical has happened yet, so the attempt is cancelled with the
    // job rather than left occupying a drive.
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let world = seed(&harness.pool).await;
    let job = queue_claimable(&harness, &world).await;
    let (worker_id, credential, drive) = worker(&harness).await;
    let (attempt, _lease) = claim(&harness, &worker_id, &credential, &drive, &job).await;

    let (status, body) = harness
        .post(&format!("/api/v1/burn-jobs/{job}/cancel"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, detail) = harness
        .get(&format!("/api/v1/burn-attempts/{attempt}"))
        .await;
    assert_eq!(detail["state"], "canceled");
    assert_eq!(detail["consumed_media"], false);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_burn_that_has_started_writing_cannot_be_cancelled() {
    // The refusal this whole route exists for. Stopping a write ruins the
    // medium, so the attempt is allowed to finish and its outcome recorded.
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let world = seed(&harness.pool).await;
    let job = queue_claimable(&harness, &world).await;
    let (worker_id, credential, drive) = worker(&harness).await;
    let (attempt, _lease) = claim(&harness, &worker_id, &credential, &drive, &job).await;

    assert_eq!(
        report(&harness, &credential, &attempt, 1, "writing").await,
        StatusCode::OK
    );

    let (status, body) = harness
        .post(&format!("/api/v1/burn-jobs/{job}/cancel"), json!({}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "WRITE_IN_PROGRESS");

    // And the attempt is untouched: a refused cancellation changes nothing.
    let (_, detail) = harness
        .get(&format!("/api/v1/burn-attempts/{attempt}"))
        .await;
    assert_eq!(detail["state"], "writing");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_finished_burn_cannot_be_cancelled() {
    let harness = harness().await;
    let world = seed(&harness.pool).await;
    let job = queue(&harness, &world).await;
    sqlx::query("UPDATE burn_jobs SET state = 'complete' WHERE id = $1::uuid")
        .bind(&job)
        .execute(&harness.pool)
        .await
        .expect("finish the job");

    let (status, body) = harness
        .post(&format!("/api/v1/burn-jobs/{job}/cancel"), json!({}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "CONFLICT");
}

// --- retry -------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_failed_burn_can_be_requeued_without_losing_its_history() {
    // A retry spends another disc. The record of the discs already spent is
    // what tells an operator that, so it must survive.
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let world = seed(&harness.pool).await;
    let job = queue_claimable(&harness, &world).await;
    let (worker_id, credential, drive) = worker(&harness).await;
    let (attempt, lease) = claim(&harness, &worker_id, &credential, &drive, &job).await;

    let (status, body) = harness
        .post_with(
            &format!("/api/v1/burn-attempts/{attempt}/complete"),
            json!({
                "lease_token": lease,
                "last_sequence": 1,
                "write_report": {
                    "state": "failed",
                    "engine": "fake",
                    "engine_version": "0.1.0",
                    "started_at": "2026-01-01T00:00:00Z",
                    "completed_at": "2026-01-01T00:01:00Z",
                },
                "verification_report": null,
                "physical_medium": { "profile": "bd-r-25" },
            }),
            Some(("authorization", &format!("Bearer {credential}"))),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, retried) = harness
        .post(&format!("/api/v1/burn-jobs/{job}/retry"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    assert_eq!(retried["state"], "queued");
    assert!(retried["completed_at"].is_null());
    assert_eq!(
        retried["attempts"].as_array().expect("attempts").len(),
        1,
        "the failed attempt must survive the retry"
    );
    assert_eq!(retried["attempts"][0]["state"], "write_failed");
    assert_eq!(retried["attempts"][0]["consumed_media"], true);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_completed_burn_is_repeated_rather_than_retried() {
    let harness = harness().await;
    let world = seed(&harness.pool).await;
    let job = queue(&harness, &world).await;
    sqlx::query("UPDATE burn_jobs SET state = 'complete' WHERE id = $1::uuid")
        .bind(&job)
        .execute(&harness.pool)
        .await
        .expect("finish the job");

    let (status, body) = harness
        .post(&format!("/api/v1/burn-jobs/{job}/retry"), json!({}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "NOT_RETRYABLE");
    assert!(
        body["detail"]
            .as_str()
            .expect("detail")
            .contains("new burn"),
        "the refusal must say what to do instead: {body}"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_burn_needing_attention_is_not_requeued() {
    // Its disc is unaccounted for. Queueing another write before that is
    // settled is how a second disc gets made for one intent.
    let harness = harness().await;
    let world = seed(&harness.pool).await;
    let job = queue(&harness, &world).await;
    sqlx::query("UPDATE burn_jobs SET state = 'needs_attention' WHERE id = $1::uuid")
        .bind(&job)
        .execute(&harness.pool)
        .await
        .expect("flag the job");

    let (status, body) = harness
        .post(&format!("/api/v1/burn-jobs/{job}/retry"), json!({}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "NOT_RETRYABLE");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_running_burn_is_not_requeued() {
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let world = seed(&harness.pool).await;
    let job = queue_claimable(&harness, &world).await;
    let (worker_id, credential, drive) = worker(&harness).await;
    claim(&harness, &worker_id, &credential, &drive, &job).await;

    let (status, body) = harness
        .post(&format!("/api/v1/burn-jobs/{job}/retry"), json!({}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "NOT_RETRYABLE");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_burn_needing_attention_is_released_by_a_person_and_then_retried() {
    // The dead end has a way out, and it runs through a person: they look at
    // the drive, record what happened to the disc, and say so. Only then does
    // a retry become available.
    let harness = harness().await;
    let world = seed(&harness.pool).await;
    let job = queue(&harness, &world).await;
    sqlx::query("UPDATE burn_jobs SET state = 'needs_attention' WHERE id = $1::uuid")
        .bind(&job)
        .execute(&harness.pool)
        .await
        .expect("flag the job");

    // Retrying first is still refused, and the refusal now says what to do.
    let (status, refused) = harness
        .post(&format!("/api/v1/burn-jobs/{job}/retry"), json!({}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        refused["detail"]
            .as_str()
            .expect("detail")
            .contains("resolve the attention"),
        "{refused}"
    );

    let (status, resolved) = harness
        .post(
            &format!("/api/v1/burn-jobs/{job}/resolve-attention"),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{resolved}");
    // Failed, not complete: whatever was found, this attempt produced no
    // verified disc.
    assert_eq!(resolved["state"], "failed");

    let (status, retried) = harness
        .post(&format!("/api/v1/burn-jobs/{job}/retry"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    assert_eq!(retried["state"], "queued");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_burn_that_is_not_asking_for_attention_has_none_to_resolve() {
    let harness = harness().await;
    let world = seed(&harness.pool).await;
    let job = queue(&harness, &world).await;

    let (status, problem) = harness
        .post(
            &format!("/api/v1/burn-jobs/{job}/resolve-attention"),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(problem["code"], "NOT_RETRYABLE");
}

// --- progress -----------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_worker_waiting_for_media_says_so_on_the_job() {
    // The state an operator has to see: the server is not stuck, it is
    // waiting for them to put a disc in.
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let world = seed(&harness.pool).await;
    let job = queue_claimable(&harness, &world).await;
    let (worker_id, credential, drive) = worker(&harness).await;
    let (attempt, _lease) = claim(&harness, &worker_id, &credential, &drive, &job).await;

    report(&harness, &credential, &attempt, 1, "staging").await;
    report(&harness, &credential, &attempt, 2, "waiting_for_media").await;

    let (status, body) = harness.get(&format!("/api/v1/burn-jobs/{job}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "waiting_for_media");
    assert_eq!(body["progress"]["stage"], "waiting_for_media");
    assert_eq!(body["has_started_writing"], false);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn preflight_rejecting_a_disc_moves_the_job_back_to_waiting() {
    // Before anything physical happens the worker's account is simply the
    // truth, in either direction. Showing "preflighting" while an operator is
    // being asked for another disc would be a lie.
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let world = seed(&harness.pool).await;
    let job = queue_claimable(&harness, &world).await;
    let (worker_id, credential, drive) = worker(&harness).await;
    let (attempt, _lease) = claim(&harness, &worker_id, &credential, &drive, &job).await;

    report(&harness, &credential, &attempt, 1, "preflighting").await;
    report(&harness, &credential, &attempt, 2, "waiting_for_media").await;

    let (_, body) = harness.get(&format!("/api/v1/burn-jobs/{job}")).await;
    assert_eq!(body["state"], "waiting_for_media");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_job_that_has_started_writing_never_goes_back() {
    // The safety-critical direction. Events arriving late or out of order
    // must not make a burn look as though it had not begun.
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let world = seed(&harness.pool).await;
    let job = queue_claimable(&harness, &world).await;
    let (worker_id, credential, drive) = worker(&harness).await;
    let (attempt, _lease) = claim(&harness, &worker_id, &credential, &drive, &job).await;

    report(&harness, &credential, &attempt, 1, "writing").await;
    report(&harness, &credential, &attempt, 2, "staging").await;

    let (_, body) = harness.get(&format!("/api/v1/burn-jobs/{job}")).await;
    assert_eq!(body["state"], "writing");
    assert_eq!(body["has_started_writing"], true);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_stage_this_server_does_not_know_leaves_the_job_where_it_was() {
    // A worker from a later release must not be able to move a job somewhere
    // this build cannot reason about.
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let world = seed(&harness.pool).await;
    let job = queue_claimable(&harness, &world).await;
    let (worker_id, credential, drive) = worker(&harness).await;
    let (attempt, _lease) = claim(&harness, &worker_id, &credential, &drive, &job).await;

    report(&harness, &credential, &attempt, 1, "staging").await;
    assert_eq!(
        report(&harness, &credential, &attempt, 2, "polishing").await,
        StatusCode::OK,
        "the event itself is still recorded"
    );

    let (_, body) = harness.get(&format!("/api/v1/burn-jobs/{job}")).await;
    assert_eq!(body["state"], "staging");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn events_are_read_back_in_sequence_and_page_forwards() {
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let world = seed(&harness.pool).await;
    let job = queue_claimable(&harness, &world).await;
    let (worker_id, credential, drive) = worker(&harness).await;
    let (attempt, _lease) = claim(&harness, &worker_id, &credential, &drive, &job).await;

    for (sequence, stage) in [(1, "staging"), (2, "preflighting"), (3, "writing")] {
        report(&harness, &credential, &attempt, sequence, stage).await;
    }

    let (status, page) = harness
        .get(&format!("/api/v1/burn-attempts/{attempt}/events?limit=2"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"][0]["sequence"], 1);
    assert_eq!(page["items"][1]["sequence"], 2);
    assert_eq!(page["next_after"], 2);

    let (_, rest) = harness
        .get(&format!(
            "/api/v1/burn-attempts/{attempt}/events?after=2&limit=2"
        ))
        .await;
    assert_eq!(rest["items"][0]["sequence"], 3);
    assert!(rest["next_after"].is_null());
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn events_for_an_unknown_attempt_are_not_an_empty_timeline() {
    let harness = harness().await;
    let missing = uuid::Uuid::now_v7();
    let (status, body) = harness
        .get(&format!("/api/v1/burn-attempts/{missing}/events"))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "NOT_FOUND");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_completed_attempt_carries_its_reports_and_its_disc() {
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let world = seed(&harness.pool).await;
    let job = queue_claimable(&harness, &world).await;
    let (worker_id, credential, drive) = worker(&harness).await;
    let (attempt, lease) = claim(&harness, &worker_id, &credential, &drive, &job).await;

    let (status, body) = harness
        .post_with(
            &format!("/api/v1/burn-attempts/{attempt}/complete"),
            json!({
                "lease_token": lease,
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
                    "state": "match",
                    "bytes_read": 10,
                    "expected_sha256": "a".repeat(64),
                    "observed_sha256": "a".repeat(64),
                },
                "physical_medium": { "profile": "bd-r-25" },
            }),
            Some(("authorization", &format!("Bearer {credential}"))),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, detail) = harness
        .get(&format!("/api/v1/burn-attempts/{attempt}"))
        .await;
    assert_eq!(detail["state"], "verified");
    assert_eq!(detail["write_report"]["state"], "success");
    assert_eq!(detail["verify_report"]["state"], "match");
    assert!(
        detail["physical_copy_id"].is_string(),
        "a verified attempt produced a disc: {detail}"
    );

    let (_, job_body) = harness.get(&format!("/api/v1/burn-jobs/{job}")).await;
    assert_eq!(job_body["state"], "complete");
    assert_eq!(job_body["attempt_count"], 1);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn events_arriving_after_completion_do_not_reopen_the_job() {
    // The server already holds the outcome. A late batch is history, not a
    // reason to think the burn is running again.
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let world = seed(&harness.pool).await;
    let job = queue_claimable(&harness, &world).await;
    let (worker_id, credential, drive) = worker(&harness).await;
    let (attempt, lease) = claim(&harness, &worker_id, &credential, &drive, &job).await;

    harness
        .post_with(
            &format!("/api/v1/burn-attempts/{attempt}/complete"),
            json!({
                "lease_token": lease,
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
                    "state": "match",
                    "bytes_read": 10,
                    "expected_sha256": "a".repeat(64),
                    "observed_sha256": "a".repeat(64),
                },
                "physical_medium": { "profile": "bd-r-25" },
            }),
            Some(("authorization", &format!("Bearer {credential}"))),
        )
        .await;

    assert_eq!(
        report(&harness, &credential, &attempt, 9, "writing").await,
        StatusCode::OK
    );

    let (_, body) = harness.get(&format!("/api/v1/burn-jobs/{job}")).await;
    assert_eq!(body["state"], "complete");
    let (_, detail) = harness
        .get(&format!("/api/v1/burn-attempts/{attempt}"))
        .await;
    assert_eq!(detail["state"], "verified");
}
