// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The worker protocol, exercised through the real router.
//!
//! Requests go through axum's routing, the credential extractor and the
//! repositories against a real PostgreSQL, because the properties worth
//! testing here (that a credential authorises acting as exactly one worker,
//! that an enrollment token works once) are properties of the whole path and
//! not of any one function on it.
//!
//! Fixtures bind their parameters rather than formatting them into the SQL.
//! Nothing here is untrusted, but test files get copied.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sqlx::PgPool;
use tangible_api::{ApiState, router};
use tangible_db::{Database, DbConfig};
use tower::ServiceExt as _;

mod support;

/// Serialises tests that assert on the state of the whole queue.
///
/// Same reason as the repository tests: claiming takes any queued job, so a
/// test asserting "nothing to do" races anything that queues work.
static QUEUE: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

async fn exclusive_queue() -> tokio::sync::MutexGuard<'static, ()> {
    QUEUE
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

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
}

async fn harness() -> Harness {
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
        // 204 carries no body; represent it as null rather than failing to parse.
        let body = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).expect("a JSON body")
        };
        (status, body)
    }

    async fn post(
        &self,
        path: &str,
        credential: Option<&str>,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json");
        if let Some(credential) = credential {
            request = request.header("authorization", format!("Bearer {credential}"));
        }
        self.send(
            request
                .body(Body::from(body.to_string()))
                .expect("build the request"),
        )
        .await
    }
}

/// Issue an enrollment token directly, as an administrator would.
async fn issue_token(pool: &PgPool) -> String {
    let secret = format!("tgw_enroll_{}", uuid::Uuid::now_v7().simple());
    sqlx::query(
        "INSERT INTO worker_enrollments (id, token_hash, expires_at, created_by)
         VALUES ($1, $2, now() + interval '1 hour', 'test')",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(hex_sha256(&secret))
    .execute(pool)
    .await
    .expect("issue an enrollment token");
    secret
}

/// The same hashing the server applies to a presented secret.
fn hex_sha256(value: &str) -> String {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(value.as_bytes());
    hex::encode(hasher.finalize())
}

/// Enroll a worker and return its identity and credential.
///
/// The name is unique per call: worker names are unique in the schema, and
/// tests share one database.
async fn enroll(harness: &Harness) -> (String, String) {
    let token = issue_token(&harness.pool).await;
    let (status, body) = harness
        .post(
            "/api/v1/worker-enrollments/consume",
            None,
            serde_json::json!({
                "enrollment_token": token,
                "name": format!("test-worker-{}", uuid::Uuid::now_v7()),
                "protocol_versions": ["1alpha1"],
                "software_version": "0.1.0",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    (
        body["worker_id"].as_str().expect("worker_id").to_owned(),
        body["credential"].as_str().expect("credential").to_owned(),
    )
}

// --- issuing a token ----------------------------------------------------------

/// Issue a token through the operator route, as a deployment does.
async fn issue_via_route(harness: &Harness, body: serde_json::Value) -> serde_json::Value {
    let (status, issued) = harness.post("/api/v1/worker-enrollments", None, body).await;
    assert_eq!(status, StatusCode::CREATED, "{issued}");
    issued
}

async fn consume(harness: &Harness, token: &str) -> StatusCode {
    harness
        .post(
            "/api/v1/worker-enrollments/consume",
            None,
            serde_json::json!({
                "enrollment_token": token,
                "name": format!("issued-worker-{}", uuid::Uuid::now_v7()),
                "protocol_versions": ["1alpha1"],
                "software_version": "0.1.0",
            }),
        )
        .await
        .0
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_issued_token_enrolls_a_worker_once() {
    // The route an operator was missing: until it existed, nothing but a test
    // could put a token in the table, so no real worker could ever enroll.
    let harness = harness().await;
    let issued = issue_via_route(&harness, serde_json::json!({})).await;
    let token = issued["enrollment_token"].as_str().expect("a token");
    assert!(token.starts_with("tgw_enroll_"), "{issued}");

    assert_eq!(consume(&harness, token).await, StatusCode::OK);
    assert_eq!(
        consume(&harness, token).await,
        StatusCode::UNAUTHORIZED,
        "one use"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_token_lasts_fifteen_minutes_unless_asked_and_never_past_an_hour() {
    let harness = harness().await;
    let lifetime = |issued: &serde_json::Value| {
        let expires = time::OffsetDateTime::parse(
            issued["expires_at"].as_str().expect("expires_at"),
            &time::format_description::well_known::Rfc3339,
        )
        .expect("RFC 3339");
        (expires - time::OffsetDateTime::now_utc()).whole_minutes()
    };

    let default = issue_via_route(&harness, serde_json::json!({})).await;
    assert!((14..=15).contains(&lifetime(&default)), "{default}");
    let hour = issue_via_route(&harness, serde_json::json!({"expires_in_minutes": 60})).await;
    assert!((59..=60).contains(&lifetime(&hour)), "{hour}");

    for minutes in [0, 61, 100_000] {
        let (status, problem) = harness
            .post(
                "/api/v1/worker-enrollments",
                None,
                serde_json::json!({"expires_in_minutes": minutes}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{minutes}: {problem}");
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn only_the_hash_is_kept_and_the_issue_is_audited() {
    let harness = harness().await;
    let issued = issue_via_route(&harness, serde_json::json!({})).await;
    let token = issued["enrollment_token"].as_str().expect("a token");
    let enrollment_id: uuid::Uuid = issued["enrollment_id"]
        .as_str()
        .expect("an id")
        .parse()
        .expect("a uuid");

    let stored: String =
        sqlx::query_scalar("SELECT token_hash FROM worker_enrollments WHERE id = $1")
            .bind(enrollment_id)
            .fetch_one(&harness.pool)
            .await
            .expect("the row");
    assert_ne!(stored, token);
    assert!(!stored.contains(token));

    let (actor, action, outcome): (String, String, String) = sqlx::query_as(
        "SELECT actor_id, action, outcome FROM audit_events
         WHERE target_type = 'worker_enrollment' AND target_id = $1",
    )
    .bind(enrollment_id.to_string())
    .fetch_one(&harness.pool)
    .await
    .expect("an audit row");
    assert_eq!(
        (actor.as_str(), action.as_str(), outcome.as_str()),
        (
            harness.session.user.username.as_str(),
            "worker_enrollment.issued",
            "success"
        ),
        "the audit row names the administrator who asked"
    );

    let metadata: serde_json::Value =
        sqlx::query_scalar("SELECT metadata FROM audit_events WHERE target_id = $1")
            .bind(enrollment_id.to_string())
            .fetch_one(&harness.pool)
            .await
            .expect("metadata");
    assert!(
        !metadata.to_string().contains(token),
        "the token is not in the audit log: {metadata}"
    );
}

// --- enrollment ------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn enrollment_returns_a_usable_credential() {
    let harness = harness().await;
    let (worker_id, credential) = enroll(&harness).await;

    assert!(credential.starts_with("tgw_live_"));

    // Usable means usable: the credential authenticates on a real route.
    let (status, _) = harness
        .post(
            &format!("/api/v1/workers/{worker_id}/heartbeat"),
            Some(&credential),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_enrollment_token_works_exactly_once() {
    let harness = harness().await;
    let token = issue_token(&harness.pool).await;
    let request = serde_json::json!({
        "enrollment_token": token,
        "name": format!("test-worker-{}", uuid::Uuid::now_v7()),
        "protocol_versions": ["1alpha1"],
        "software_version": "0.1.0",
    });

    let (first, _) = harness
        .post("/api/v1/worker-enrollments/consume", None, request.clone())
        .await;
    assert_eq!(first, StatusCode::OK);

    let (second, body) = harness
        .post("/api/v1/worker-enrollments/consume", None, request)
        .await;
    assert_eq!(second, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "UNAUTHENTICATED");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_unknown_and_a_spent_token_are_indistinguishable() {
    // A caller must not be able to probe for tokens that once existed.
    let harness = harness().await;
    let token = issue_token(&harness.pool).await;
    let body_for = |secret: &str| {
        serde_json::json!({
            "enrollment_token": secret,
            "name": format!("test-worker-{}", uuid::Uuid::now_v7()),
            "protocol_versions": ["1alpha1"],
            "software_version": "0.1.0",
        })
    };

    harness
        .post("/api/v1/worker-enrollments/consume", None, body_for(&token))
        .await;
    let (spent_status, spent) = harness
        .post("/api/v1/worker-enrollments/consume", None, body_for(&token))
        .await;
    let (unknown_status, unknown) = harness
        .post(
            "/api/v1/worker-enrollments/consume",
            None,
            body_for("tgw_enroll_never-existed"),
        )
        .await;

    assert_eq!(spent_status, unknown_status);
    assert_eq!(spent["code"], unknown["code"]);
    assert_eq!(spent["detail"], unknown["detail"]);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_unspeakable_protocol_is_refused_without_spending_the_token() {
    // Negotiation happens before the token is consumed, so a client that
    // cannot talk to this server does not burn its one-use token finding out.
    let harness = harness().await;
    let token = issue_token(&harness.pool).await;

    let (status, body) = harness
        .post(
            "/api/v1/worker-enrollments/consume",
            None,
            serde_json::json!({
                "enrollment_token": token,
                "name": format!("test-worker-{}", uuid::Uuid::now_v7()),
                "protocol_versions": ["99beta"],
                "software_version": "0.1.0",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "UNSUPPORTED_PROTOCOL");

    // The token still works.
    let (retry, _) = harness
        .post(
            "/api/v1/worker-enrollments/consume",
            None,
            serde_json::json!({
                "enrollment_token": token,
                "name": format!("test-worker-{}", uuid::Uuid::now_v7()),
                "protocol_versions": ["1alpha1"],
                "software_version": "0.1.0",
            }),
        )
        .await;
    assert_eq!(retry, StatusCode::OK);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_bad_token_answers_the_same_whether_or_not_the_name_is_taken() {
    // Enrollment is unauthenticated, so if a taken name answered differently
    // from a free one, anyone who could reach the server could enumerate
    // worker names by sending junk tokens. The token is settled first
    // precisely so this pair of requests is indistinguishable.
    let harness = harness().await;
    let taken = format!("test-worker-{}", uuid::Uuid::now_v7());
    let valid = issue_token(&harness.pool).await;
    let (created, _) = harness
        .post(
            "/api/v1/worker-enrollments/consume",
            None,
            serde_json::json!({
                "enrollment_token": valid,
                "name": taken,
                "protocol_versions": ["1alpha1"],
                "software_version": "0.1.0",
            }),
        )
        .await;
    assert_eq!(created, StatusCode::OK);

    let probe = |name: String| {
        serde_json::json!({
            "enrollment_token": "tgw_enroll_junk",
            "name": name,
            "protocol_versions": ["1alpha1"],
            "software_version": "0.1.0",
        })
    };
    let (taken_status, taken_body) = harness
        .post("/api/v1/worker-enrollments/consume", None, probe(taken))
        .await;
    let (free_status, free_body) = harness
        .post(
            "/api/v1/worker-enrollments/consume",
            None,
            probe(format!("test-worker-{}", uuid::Uuid::now_v7())),
        )
        .await;

    assert_eq!(taken_status, StatusCode::UNAUTHORIZED);
    assert_eq!(taken_status, free_status);
    assert_eq!(taken_body["code"], free_body["code"]);
    assert_eq!(taken_body["detail"], free_body["detail"]);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_taken_name_with_a_good_token_is_a_conflict() {
    // Once the token is known good, the name collision is the operator's own
    // mistake and is worth saying plainly.
    let harness = harness().await;
    let name = format!("test-worker-{}", uuid::Uuid::now_v7());
    let request = |token: String| {
        serde_json::json!({
            "enrollment_token": token,
            "name": name,
            "protocol_versions": ["1alpha1"],
            "software_version": "0.1.0",
        })
    };

    let first = issue_token(&harness.pool).await;
    let (created, _) = harness
        .post("/api/v1/worker-enrollments/consume", None, request(first))
        .await;
    assert_eq!(created, StatusCode::OK);

    let second = issue_token(&harness.pool).await;
    let (status, body) = harness
        .post(
            "/api/v1/worker-enrollments/consume",
            None,
            request(second.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "CONFLICT");

    // A rejected name must not spend the token.
    let (retry, _) = harness
        .post(
            "/api/v1/worker-enrollments/consume",
            None,
            serde_json::json!({
                "enrollment_token": second,
                "name": format!("{name}-2"),
                "protocol_versions": ["1alpha1"],
                "software_version": "0.1.0",
            }),
        )
        .await;
    assert_eq!(retry, StatusCode::OK);
}

// --- authentication --------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_worker_cannot_act_as_another_worker() {
    // The reason the path is checked against the identity at all. Without it,
    // any enrolled worker could drive somebody else's hardware by editing a
    // URL, and every worker in the fleet holds a valid credential.
    let harness = harness().await;
    let (_, credential_a) = enroll(&harness).await;
    let (worker_b, _) = enroll(&harness).await;

    let (status, body) = harness
        .post(
            &format!("/api/v1/workers/{worker_b}/heartbeat"),
            Some(&credential_a),
            serde_json::json!({}),
        )
        .await;

    // Not-found rather than forbidden: confirming that worker B exists is
    // information worker A has no business having.
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "NOT_FOUND");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn claiming_as_another_worker_is_refused() {
    // The same check on the route that actually starts hardware moving.
    let harness = harness().await;
    let (_, credential_a) = enroll(&harness).await;
    let (worker_b, _) = enroll(&harness).await;

    let (status, _) = harness
        .post(
            &format!("/api/v1/workers/{worker_b}/claims"),
            Some(&credential_a),
            serde_json::json!({
                "drive_id": uuid::Uuid::now_v7().to_string(),
                "engine": "fake",
                "engine_version": "0.1.0",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn requests_without_a_credential_are_refused() {
    let harness = harness().await;
    let (worker_id, _) = enroll(&harness).await;

    let (status, body) = harness
        .post(
            &format!("/api/v1/workers/{worker_id}/heartbeat"),
            None,
            serde_json::json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "UNAUTHENTICATED");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_credential_of_the_wrong_kind_is_refused() {
    // A user session token presented to a worker route must fail on kind,
    // before any lookup, whatever the store happens to contain.
    let harness = harness().await;
    let (worker_id, _) = enroll(&harness).await;

    let (status, _) = harness
        .post(
            &format!("/api/v1/workers/{worker_id}/heartbeat"),
            Some("tgu_live_some-user-session"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_revoked_worker_is_refused() {
    let harness = harness().await;
    let (worker_id, credential) = enroll(&harness).await;

    // Both columns together: the schema refuses a revocation with no record of
    // when it happened.
    sqlx::query("UPDATE workers SET status = 'revoked', revoked_at = now() WHERE id = $1::uuid")
        .bind(&worker_id)
        .execute(&harness.pool)
        .await
        .expect("revoke");

    let (status, _) = harness
        .post(
            &format!("/api/v1/workers/{worker_id}/heartbeat"),
            Some(&credential),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// --- claiming --------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_empty_queue_answers_no_content() {
    // Polling an empty queue is the common case and must not read as an error.
    let _queue = exclusive_queue().await;
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let (worker_id, credential) = enroll(&harness).await;
    let drive = seed_drive(&harness.pool, &worker_id).await;

    let (status, body) = harness
        .post(
            &format!("/api/v1/workers/{worker_id}/claims"),
            Some(&credential),
            serde_json::json!({
                "drive_id": drive,
                "engine": "fake",
                "engine_version": "0.1.0",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(body, serde_json::Value::Null);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_claim_leases_work_and_the_lease_can_be_renewed() {
    let _queue = exclusive_queue().await;
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let (worker_id, credential) = enroll(&harness).await;
    let drive = seed_drive(&harness.pool, &worker_id).await;
    queue_job(&harness.pool).await;

    let (status, claim) = harness
        .post(
            &format!("/api/v1/workers/{worker_id}/claims"),
            Some(&credential),
            serde_json::json!({
                "drive_id": drive,
                "engine": "fake",
                "engine_version": "0.1.0",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{claim}");
    assert_eq!(claim["attempt_number"], 1);

    let attempt = claim["attempt_id"].as_str().expect("attempt_id");
    let lease = claim["lease_token"].as_str().expect("lease_token");
    assert!(lease.starts_with("tgw_lease_"));

    let (renewed, body) = harness
        .post(
            &format!("/api/v1/burn-attempts/{attempt}/lease/renew"),
            Some(&credential),
            serde_json::json!({ "lease_token": lease }),
        )
        .await;
    assert_eq!(renewed, StatusCode::OK, "{body}");
    assert!(body["lease_expires_at"].is_string());
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_lease_cannot_be_renewed_without_its_token() {
    // Being *a* worker is not enough; renewal requires holding the lease.
    // Otherwise any worker could keep another's attempt alive indefinitely.
    let _queue = exclusive_queue().await;
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let (worker_id, credential) = enroll(&harness).await;
    let drive = seed_drive(&harness.pool, &worker_id).await;
    queue_job(&harness.pool).await;

    let (_, claim) = harness
        .post(
            &format!("/api/v1/workers/{worker_id}/claims"),
            Some(&credential),
            serde_json::json!({
                "drive_id": drive,
                "engine": "fake",
                "engine_version": "0.1.0",
            }),
        )
        .await;
    let attempt = claim["attempt_id"].as_str().expect("attempt_id");

    let (_, other_credential) = enroll(&harness).await;
    let (status, _) = harness
        .post(
            &format!("/api/v1/burn-attempts/{attempt}/lease/renew"),
            Some(&other_credential),
            serde_json::json!({ "lease_token": "tgw_lease_guessed" }),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_busy_drive_reports_a_conflict() {
    let _queue = exclusive_queue().await;
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let (worker_id, credential) = enroll(&harness).await;
    let drive = seed_drive(&harness.pool, &worker_id).await;
    queue_job(&harness.pool).await;
    queue_job(&harness.pool).await;

    let claim = serde_json::json!({
        "drive_id": drive,
        "engine": "fake",
        "engine_version": "0.1.0",
    });
    let path = format!("/api/v1/workers/{worker_id}/claims");

    let (first, _) = harness.post(&path, Some(&credential), claim.clone()).await;
    assert_eq!(first, StatusCode::OK);

    // One drive, one attempt: the second claim must not hand this worker a
    // second job for hardware already writing.
    let (second, body) = harness.post(&path, Some(&credential), claim).await;
    assert_eq!(second, StatusCode::CONFLICT);
    assert_eq!(body["code"], "CONFLICT");
}

// --- events ----------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn resubmitting_events_is_idempotent() {
    // What lets a worker resend everything unacknowledged after an outage
    // without knowing what arrived.
    let _queue = exclusive_queue().await;
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let attempt = claimed_attempt(&harness).await;
    let path = format!("/api/v1/burn-attempts/{}/events", attempt.id);

    let batch = serde_json::json!({
        "events": [
            event(1, "stage_changed", "preflight"),
            event(2, "progress", "write"),
        ]
    });

    let (first, ack) = harness
        .post(&path, Some(&attempt.credential), batch.clone())
        .await;
    assert_eq!(first, StatusCode::OK, "{ack}");
    assert_eq!(ack["accepted_through_sequence"], 2);

    let (second, again) = harness.post(&path, Some(&attempt.credential), batch).await;
    assert_eq!(second, StatusCode::OK);
    assert_eq!(again["accepted_through_sequence"], 2);

    let stored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM burn_events WHERE attempt_id = $1::uuid")
            .bind(&attempt.id)
            .fetch_one(&harness.pool)
            .await
            .expect("count events");
    assert_eq!(stored, 2, "a replayed batch must not duplicate rows");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn acknowledgement_stops_at_a_gap() {
    // Acknowledging the maximum would tell the worker to discard events it
    // still holds and the server does not.
    let _queue = exclusive_queue().await;
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let attempt = claimed_attempt(&harness).await;

    let (status, ack) = harness
        .post(
            &format!("/api/v1/burn-attempts/{}/events", attempt.id),
            Some(&attempt.credential),
            serde_json::json!({
                "events": [
                    event(1, "progress", "write"),
                    event(2, "progress", "write"),
                    event(5, "progress", "write"),
                ]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!(ack["accepted_through_sequence"], 2);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_oversized_batch_is_refused() {
    let harness = harness().await;
    let (_, credential) = enroll(&harness).await;
    let events: Vec<_> = (1..=600).map(|n| event(n, "progress", "write")).collect();

    let (status, body) = harness
        .post(
            &format!("/api/v1/burn-attempts/{}/events", uuid::Uuid::now_v7()),
            Some(&credential),
            serde_json::json!({ "events": events }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "INVALID_PARAMETER");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_malformed_identifier_is_a_bad_request_not_a_server_error() {
    let harness = harness().await;
    let (_, credential) = enroll(&harness).await;

    let (status, body) = harness
        .post(
            "/api/v1/burn-attempts/not-a-uuid/events",
            Some(&credential),
            serde_json::json!({ "events": [] }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "INVALID_PARAMETER");
}

// --- capabilities ------------------------------------------------------------

fn capability_body(alias: &str) -> serde_json::Value {
    serde_json::json!({
        "software_version": "0.1.0",
        "engines": [{ "name": "fake", "version": "0.1.0" }],
        "drive": {
            "device_alias": alias,
            "configured_name": "workshop",
            "vendor": "PIONEER",
            "model": "BDR-212",
            "status": "ready_empty",
            "capabilities": { "write_profiles": ["CD-R"] },
        },
        "cache_free_bytes": 500_000_000_000_i64,
    })
}

async fn put(
    harness: &Harness,
    path: &str,
    credential: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method("PUT")
        .uri(path)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {credential}"))
        .body(Body::from(body.to_string()))
        .expect("build the request");
    harness.send(request).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_worker_reporting_a_drive_is_given_an_identifier_for_it() {
    let harness = harness().await;
    let (worker_id, credential) = enroll(&harness).await;

    let (status, body) = put(
        &harness,
        &format!("/api/v1/workers/{worker_id}/capabilities"),
        &credential,
        capability_body("/dev/disc-block"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let drive_id = body["drive_id"].as_str().expect("a drive id");
    let (alias, status_value): (String, String) =
        sqlx::query_as("SELECT device_alias, status FROM drives WHERE id = $1::uuid")
            .bind(drive_id)
            .fetch_one(&harness.pool)
            .await
            .expect("the drive");
    assert_eq!(alias, "/dev/disc-block");
    assert_eq!(status_value, "ready_empty");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_restarting_worker_reports_the_same_drive_rather_than_a_second_one() {
    // Identity is the worker plus the alias. A new row per restart would
    // leave burn history pointing at drives nobody can find.
    let harness = harness().await;
    let (worker_id, credential) = enroll(&harness).await;
    let path = format!("/api/v1/workers/{worker_id}/capabilities");

    let (_, first) = put(
        &harness,
        &path,
        &credential,
        capability_body("/dev/disc-block"),
    )
    .await;
    let (_, second) = put(
        &harness,
        &path,
        &credential,
        capability_body("/dev/disc-block"),
    )
    .await;
    assert_eq!(first["drive_id"], second["drive_id"]);

    let drives: i64 = sqlx::query_scalar("SELECT count(*) FROM drives WHERE worker_id = $1::uuid")
        .bind(&worker_id)
        .fetch_one(&harness.pool)
        .await
        .expect("count");
    assert_eq!(drives, 1);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_second_drive_on_one_worker_is_its_own_drive() {
    let harness = harness().await;
    let (worker_id, credential) = enroll(&harness).await;
    let path = format!("/api/v1/workers/{worker_id}/capabilities");

    let (_, first) = put(&harness, &path, &credential, capability_body("/dev/disc-a")).await;
    let (_, second) = put(&harness, &path, &credential, capability_body("/dev/disc-b")).await;
    assert_ne!(first["drive_id"], second["drive_id"]);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_capability_report_for_another_worker_is_refused() {
    // Same rule as every other worker route: a credential authorises acting
    // as that worker and no other.
    let harness = harness().await;
    let (_, credential) = enroll(&harness).await;
    let (other_id, _) = enroll(&harness).await;

    let (status, _) = put(
        &harness,
        &format!("/api/v1/workers/{other_id}/capabilities"),
        &credential,
        capability_body("/dev/disc-block"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_unknown_drive_status_is_refused_rather_than_stored() {
    let harness = harness().await;
    let (worker_id, credential) = enroll(&harness).await;
    let mut body = capability_body("/dev/disc-block");
    body["drive"]["status"] = serde_json::json!("melting");

    let (status, problem) = put(
        &harness,
        &format!("/api/v1/workers/{worker_id}/capabilities"),
        &credential,
        body,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem["code"], "VALIDATION_FAILED");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_raw_serial_is_not_accepted_where_a_hash_belongs() {
    // The field is a hash because the raw serial identifies hardware an
    // operator may not want in an export. Accepting a plain string would
    // quietly store one.
    let harness = harness().await;
    let (worker_id, credential) = enroll(&harness).await;
    let mut body = capability_body("/dev/disc-block");
    body["drive"]["serial_hash"] = serde_json::json!("ABC123-SERIAL");

    let (status, problem) = put(
        &harness,
        &format!("/api/v1/workers/{worker_id}/capabilities"),
        &credential,
        body,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem["code"], "VALIDATION_FAILED");
}

// --- completion ------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_write_that_never_started_records_no_disc() {
    // A preflight that refused the medium. The laser never ran, so there is
    // no disc, and recording one would send an operator hunting for a
    // physical object that does not exist.
    let _queue = exclusive_queue().await;
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let attempt = claimed_attempt(&harness).await;

    let mut body = completion(&attempt.lease, "not_attempted", None);
    body["failure"] = serde_json::json!({
        "code": "PREFLIGHT_MEDIUM_NOT_BLANK",
        "detail": "the disc in the drive already holds one session",
    });

    let (status, response) = harness
        .post(
            &format!("/api/v1/burn-attempts/{}/complete", attempt.id),
            Some(&attempt.credential),
            body,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert!(
        response["physical_copy_id"].is_null(),
        "no media was consumed: {response}"
    );
    assert_eq!(response["eject"], false);

    let (state, code, detail): (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT state, error_code, error_detail FROM burn_attempts WHERE id = $1::uuid",
    )
    .bind(&attempt.id)
    .fetch_one(&harness.pool)
    .await
    .expect("read back the attempt");
    assert_eq!(state, "failed_before_write");
    // The worker's own code, not the generic one: it knows which check
    // stopped it.
    assert_eq!(code.as_deref(), Some("PREFLIGHT_MEDIUM_NOT_BLANK"));
    assert!(
        detail.expect("detail").contains("one session"),
        "the reason must survive to the operator"
    );

    let copies: i64 =
        sqlx::query_scalar("SELECT count(*) FROM physical_copies WHERE burn_attempt_id = $1::uuid")
            .bind(&attempt.id)
            .fetch_one(&harness.pool)
            .await
            .expect("count");
    assert_eq!(copies, 0);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_eject_answer_follows_the_jobs_policy() {
    // A disc that failed verification stays in the drive by default. An
    // operator who asked for `always` gets always, because it is their
    // decision and not the route's.
    let _queue = exclusive_queue().await;
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let attempt = claimed_attempt(&harness).await;
    sqlx::query(
        "UPDATE burn_jobs SET eject_policy = 'always'
         WHERE id = (SELECT burn_job_id FROM burn_attempts WHERE id = $1::uuid)",
    )
    .bind(&attempt.id)
    .execute(&harness.pool)
    .await
    .expect("set the policy");

    let (status, body) = harness
        .post(
            &format!("/api/v1/burn-attempts/{}/complete", attempt.id),
            Some(&attempt.credential),
            completion(&attempt.lease, "success", Some("mismatch")),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["eject"], true, "the operator asked for always");
    assert!(
        body["physical_copy_id"].is_string(),
        "the bad disc is still recorded"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_verified_completion_records_a_disc() {
    let _queue = exclusive_queue().await;
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let attempt = claimed_attempt(&harness).await;

    let (status, body) = harness
        .post(
            &format!("/api/v1/burn-attempts/{}/complete", attempt.id),
            Some(&attempt.credential),
            completion(&attempt.lease, "success", Some("match")),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["acknowledged"], true);
    assert_eq!(body["eject"], true);

    let copy_id = body["physical_copy_id"].as_str().expect("a physical copy");
    let (attempt_state, copy_status): (String, String) = sqlx::query_as(
        "SELECT a.state, c.status
         FROM burn_attempts a JOIN physical_copies c ON c.burn_attempt_id = a.id
         WHERE a.id = $1::uuid",
    )
    .bind(&attempt.id)
    .fetch_one(&harness.pool)
    .await
    .expect("read back the attempt");
    assert_eq!(attempt_state, "verified");
    assert_eq!(copy_status, "verified");

    // The job follows the attempt.
    let job_state: String = sqlx::query_scalar(
        "SELECT j.state FROM burn_jobs j
         JOIN burn_attempts a ON a.burn_job_id = j.id WHERE a.id = $1::uuid",
    )
    .bind(&attempt.id)
    .fetch_one(&harness.pool)
    .await
    .expect("read back the job");
    assert_eq!(job_state, "complete");

    // Retrying returns the same disc rather than recording a second one.
    let (retry, again) = harness
        .post(
            &format!("/api/v1/burn-attempts/{}/complete", attempt.id),
            Some(&attempt.credential),
            completion(&attempt.lease, "success", Some("match")),
        )
        .await;
    assert_eq!(retry, StatusCode::OK);
    assert_eq!(again["physical_copy_id"], copy_id);

    let copies: i64 =
        sqlx::query_scalar("SELECT count(*) FROM physical_copies WHERE burn_attempt_id = $1::uuid")
            .bind(&attempt.id)
            .fetch_one(&harness.pool)
            .await
            .expect("count copies");
    assert_eq!(
        copies, 1,
        "a retried completion must not burn a second disc"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_failed_write_is_still_recorded_as_a_disc() {
    // Media was being consumed when the write failed, so a disc exists. An
    // untracked ruined disc gets shelved and reused.
    let _queue = exclusive_queue().await;
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let attempt = claimed_attempt(&harness).await;

    let (status, body) = harness
        .post(
            &format!("/api/v1/burn-attempts/{}/complete", attempt.id),
            Some(&attempt.credential),
            completion(&attempt.lease, "failed", None),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["physical_copy_id"].is_string());

    // Not ejected: a bad disc should stay where an operator will find it.
    assert_eq!(body["eject"], false);

    let job_state: String = sqlx::query_scalar(
        "SELECT j.state FROM burn_jobs j
         JOIN burn_attempts a ON a.burn_job_id = j.id WHERE a.id = $1::uuid",
    )
    .bind(&attempt.id)
    .fetch_one(&harness.pool)
    .await
    .expect("read back the job");
    assert_eq!(job_state, "failed");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_completion_without_the_lease_is_refused() {
    // Completion creates the physical disc record. Anyone able to forge one
    // could invent burn history for hardware they never touched.
    let _queue = exclusive_queue().await;
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let attempt = claimed_attempt(&harness).await;

    let (status, _) = harness
        .post(
            &format!("/api/v1/burn-attempts/{}/complete", attempt.id),
            Some(&attempt.credential),
            completion("tgw_lease_guessed", "success", Some("match")),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let copies: i64 =
        sqlx::query_scalar("SELECT count(*) FROM physical_copies WHERE burn_attempt_id = $1::uuid")
            .bind(&attempt.id)
            .fetch_one(&harness.pool)
            .await
            .expect("count copies");
    assert_eq!(copies, 0);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn completing_frees_the_drive() {
    // The one-active-attempt-per-drive index blocks a second claim until the
    // first attempt is terminal, so completion has to actually release it.
    let _queue = exclusive_queue().await;
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let attempt = claimed_attempt(&harness).await;
    queue_job(&harness.pool).await;

    let claim = serde_json::json!({
        "drive_id": attempt.drive,
        "engine": "fake",
        "engine_version": "0.1.0",
    });
    let path = format!("/api/v1/workers/{}/claims", attempt.worker);

    let (busy, _) = harness
        .post(&path, Some(&attempt.credential), claim.clone())
        .await;
    assert_eq!(busy, StatusCode::CONFLICT);

    harness
        .post(
            &format!("/api/v1/burn-attempts/{}/complete", attempt.id),
            Some(&attempt.credential),
            completion(&attempt.lease, "success", Some("match")),
        )
        .await;

    let (freed, body) = harness.post(&path, Some(&attempt.credential), claim).await;
    assert_eq!(freed, StatusCode::OK, "{body}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_combined_worker_can_claim_and_the_attempt_names_the_engine_that_wrote() {
    // Regression: the first end-to-end run's worker ran `auto` and registered
    // as such. The engine column's constraint did not know the name, so every
    // claim rolled back and the job never left the queue.
    let _queue = exclusive_queue().await;
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let (worker_id, credential) = enroll(&harness).await;
    let drive = seed_drive(&harness.pool, &worker_id).await;
    queue_job(&harness.pool).await;

    let (status, claim) = harness
        .post(
            &format!("/api/v1/workers/{worker_id}/claims"),
            Some(&credential),
            serde_json::json!({
                "drive_id": drive,
                "engine": "auto",
                "engine_version": "xorriso 1.5.4; cdrdao 1.2.4",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{claim}");
    let attempt_id = claim["attempt_id"].as_str().expect("attempt_id").to_owned();
    let lease = claim["lease_token"].as_str().expect("lease").to_owned();

    let mut body = completion(&lease, "success", Some("match"));
    body["write_report"]["engine"] = serde_json::json!("xorriso");
    body["write_report"]["engine_version"] = serde_json::json!("1.5.4");
    let (status, done) = harness
        .post(
            &format!("/api/v1/burn-attempts/{attempt_id}/complete"),
            Some(&credential),
            body,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");

    let (engine, version): (String, String) =
        sqlx::query_as("SELECT engine, engine_version FROM burn_attempts WHERE id = $1::uuid")
            .bind(&attempt_id)
            .fetch_one(&harness.pool)
            .await
            .expect("the attempt");
    assert_eq!(
        (engine.as_str(), version.as_str()),
        ("xorriso", "1.5.4"),
        "a finished attempt names the engine that wrote, not the wrapper"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_disc_checked_track_by_track_is_recorded_as_partially_verified() {
    // On the physical copy: the data track compared, the audio track
    // only measured. The copy is good, and its record says how it was checked.
    let _queue = exclusive_queue().await;
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let attempt = claimed_attempt(&harness).await;

    let mut body = completion(&attempt.lease, "success", Some("partial"));
    body["verification_report"]["policy"] = serde_json::json!("track_hash_compare");
    body["verification_report"]["tracks"] = serde_json::json!([
        {"number": 1, "check": "byte_compare", "outcome": "match",
         "sectors_expected": 1161, "sectors_read": 1161,
         "expected_sha256": "a".repeat(64), "observed_sha256": "a".repeat(64)},
        {"number": 2, "check": "length_and_readable", "outcome": "match",
         "sectors_expected": 2250, "sectors_read": 2250},
    ]);
    let (status, done) = harness
        .post(
            &format!("/api/v1/burn-attempts/{}/complete", attempt.id),
            Some(&attempt.credential),
            body,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");

    let (copy_status, level, result): (String, String, String) = sqlx::query_as(
        "SELECT status, verification_level, verification_result FROM physical_copies
         WHERE burn_attempt_id = $1::uuid",
    )
    .bind(&attempt.id)
    .fetch_one(&harness.pool)
    .await
    .expect("a physical copy");
    assert_eq!(
        (copy_status.as_str(), level.as_str(), result.as_str()),
        ("verified", "track_hash_compare", "partial")
    );

    let tracks: serde_json::Value = sqlx::query_scalar(
        "SELECT verify_report_json -> 'tracks' FROM burn_attempts WHERE id = $1::uuid",
    )
    .bind(&attempt.id)
    .fetch_one(&harness.pool)
    .await
    .expect("the stored report");
    assert_eq!(tracks.as_array().map(Vec::len), Some(2), "{tracks}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_unknown_engine_on_a_write_report_does_not_fail_completion() {
    // Completion can arrive after a disc exists. A label the column does not
    // accept leaves the claimed engine in place rather than losing the record.
    let _queue = exclusive_queue().await;
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let attempt = claimed_attempt(&harness).await;

    let mut body = completion(&attempt.lease, "success", Some("match"));
    body["write_report"]["engine"] = serde_json::json!("something-new");
    let (status, done) = harness
        .post(
            &format!("/api/v1/burn-attempts/{}/complete", attempt.id),
            Some(&attempt.credential),
            body,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");

    let engine: String = sqlx::query_scalar("SELECT engine FROM burn_attempts WHERE id = $1::uuid")
        .bind(&attempt.id)
        .fetch_one(&harness.pool)
        .await
        .expect("the attempt");
    assert_eq!(engine, "fake");
}

// --- recovery --------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_recovery_never_permits_a_write() {
    let _queue = exclusive_queue().await;
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let attempt = claimed_attempt(&harness).await;

    for stage in ["claimed", "writing", "written", "verifying"] {
        let (status, body) = harness
            .post(
                &format!("/api/v1/workers/{}/recoveries", attempt.worker),
                Some(&attempt.credential),
                serde_json::json!({
                    "attempt_id": attempt.id,
                    "local_stage": stage,
                    "last_event_sequence": 4,
                    "engine_process_state": "not_running",
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["may_write"], false, "stage {stage} permitted a write");
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_attempt_abandoned_before_writing_is_closed_and_its_job_can_be_retried() {
    // Regression: the first end-to-end run restarted its worker mid-preflight.
    // The worker was told to discard, the server kept the attempt open on an
    // expired lease, and the job could never be claimed again.
    let _queue = exclusive_queue().await;
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let attempt = claimed_attempt(&harness).await;

    let (status, body) = harness
        .post(
            &format!("/api/v1/workers/{}/recoveries", attempt.worker),
            Some(&attempt.credential),
            serde_json::json!({
                "attempt_id": attempt.id,
                "local_stage": "staging",
                "last_event_sequence": 3,
                "engine_process_state": "not_running",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["directive"], "discard_prewrite_state");

    let (state, code, job_id, job_state): (String, Option<String>, uuid::Uuid, String) =
        sqlx::query_as(
            "SELECT a.state, a.error_code, j.id, j.state
             FROM burn_attempts a JOIN burn_jobs j ON j.id = a.burn_job_id
             WHERE a.id = $1::uuid",
        )
        .bind(&attempt.id)
        .fetch_one(&harness.pool)
        .await
        .expect("the attempt");
    assert_eq!(state, "failed_before_write");
    assert_eq!(code.as_deref(), Some("WORKER_RESTARTED"));
    assert_eq!(
        job_state, "failed",
        "failed, not requeued: another disc is the operator's call"
    );

    let (status, retried) = harness
        .post(
            &format!("/api/v1/burn-jobs/{job_id}/retry"),
            None,
            serde_json::json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    assert_eq!(retried["state"], "queued");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_recovery_never_closes_an_attempt_that_may_have_written() {
    let _queue = exclusive_queue().await;
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let attempt = claimed_attempt(&harness).await;

    let (status, body) = harness
        .post(
            &format!("/api/v1/workers/{}/recoveries", attempt.worker),
            Some(&attempt.credential),
            serde_json::json!({
                "attempt_id": attempt.id,
                "local_stage": "writing",
                "last_event_sequence": 3,
                "engine_process_state": "not_running",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["directive"], "mark_needs_attention");

    let state: String = sqlx::query_scalar("SELECT state FROM burn_attempts WHERE id = $1::uuid")
        .bind(&attempt.id)
        .fetch_one(&harness.pool)
        .await
        .expect("the attempt");
    assert_eq!(
        state, "claimed",
        "a disc may exist; nothing automatic touches it"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_worker_that_wrote_is_held_rather_than_told_to_discard() {
    // The server's record says the attempt was only claimed; the worker says
    // it was writing. The dangerous reading wins.
    let _queue = exclusive_queue().await;
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let attempt = claimed_attempt(&harness).await;

    let (status, body) = harness
        .post(
            &format!("/api/v1/workers/{}/recoveries", attempt.worker),
            Some(&attempt.credential),
            serde_json::json!({
                "attempt_id": attempt.id,
                "local_stage": "writing",
                "last_event_sequence": 9,
                "engine_process_state": "not_running",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["directive"], "mark_needs_attention");
    assert_eq!(body["discard_local_state"], false);
    assert_eq!(body["may_accept_new_work"], false);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_recovery_for_another_workers_attempt_is_not_answered() {
    // Answering with another worker's attempt state would let one worker
    // decide what another does with a disc.
    let _queue = exclusive_queue().await;
    let harness = harness().await;
    drain_queue(&harness.pool).await;
    let attempt = claimed_attempt(&harness).await;

    let (other_id, other_credential) = enroll(&harness).await;
    let (status, body) = harness
        .post(
            &format!("/api/v1/workers/{other_id}/recoveries"),
            Some(&other_credential),
            serde_json::json!({
                "attempt_id": attempt.id,
                "local_stage": "claimed",
                "last_event_sequence": 0,
                "engine_process_state": "not_running",
            }),
        )
        .await;

    // Not this worker's attempt, so it is answered as unknown rather than
    // with the real attempt's state.
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["directive"], "discard_prewrite_state");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn recovering_as_another_worker_is_refused() {
    let harness = harness().await;
    let (_, credential_a) = enroll(&harness).await;
    let (worker_b, _) = enroll(&harness).await;

    let (status, _) = harness
        .post(
            &format!("/api/v1/workers/{worker_b}/recoveries"),
            Some(&credential_a),
            serde_json::json!({
                "attempt_id": uuid::Uuid::now_v7().to_string(),
                "local_stage": "claimed",
                "last_event_sequence": 0,
                "engine_process_state": "not_running",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// --- fixtures --------------------------------------------------------------

fn event(sequence: i64, event_type: &str, stage: &str) -> serde_json::Value {
    serde_json::json!({
        "sequence": sequence,
        "event_type": event_type,
        "stage": stage,
        "code": "TEST",
        "progress": 0.5,
        "data": {},
        "worker_time": "2026-01-01T00:00:00Z",
    })
}

/// An attempt leased through the real claim route.
struct Attempt {
    id: String,
    credential: String,
    lease: String,
    drive: String,
    worker: String,
}

/// Enroll a worker, give it a drive and a job, and claim it.
///
/// Only safe to call while holding the queue lock.
async fn claimed_attempt(harness: &Harness) -> Attempt {
    let (worker_id, credential) = enroll(harness).await;
    let drive = seed_drive(&harness.pool, &worker_id).await;
    queue_job(&harness.pool).await;

    let (status, claim) = harness
        .post(
            &format!("/api/v1/workers/{worker_id}/claims"),
            Some(&credential),
            serde_json::json!({
                "drive_id": drive,
                "engine": "fake",
                "engine_version": "0.1.0",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{claim}");

    Attempt {
        id: claim["attempt_id"].as_str().expect("attempt_id").to_owned(),
        lease: claim["lease_token"]
            .as_str()
            .expect("lease_token")
            .to_owned(),
        credential,
        drive,
        worker: worker_id,
    }
}

/// A completion body.
fn completion(lease: &str, write: &str, verification: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "lease_token": lease,
        "last_sequence": 12,
        "write_report": {
            "state": write,
            "engine": "fake",
            "engine_version": "0.1.0",
            "started_at": "2026-01-01T00:00:00Z",
            "completed_at": "2026-01-01T00:10:00Z",
        },
        "verification_report": verification.map(|state| serde_json::json!({
            "policy": "full_sector_readback",
            "state": state,
            "bytes_read": 10,
            "expected_sha256": "a".repeat(64),
            "observed_sha256": "a".repeat(64),
        })),
        "physical_medium": { "profile": "bd-r-25" },
    })
}

/// Attach a drive to an enrolled worker.
async fn seed_drive(pool: &PgPool, worker_id: &str) -> String {
    let drive = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO drives (id, worker_id, configured_name, device_alias)
         VALUES ($1, $2::uuid, 'd1', '/dev/disc-block')",
    )
    .bind(drive)
    .bind(worker_id)
    .execute(pool)
    .await
    .expect("attach a drive");
    drive.to_string()
}

/// Queue one burn job against a fresh catalog chain.
async fn queue_job(pool: &PgPool) -> String {
    let ids: Vec<uuid::Uuid> = (0..6).map(|_| uuid::Uuid::now_v7()).collect();
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
    sqlx::query(
        "INSERT INTO burn_jobs (id, disc_id, artifact_id, created_by) VALUES ($1, $2, $3, 'test')",
    )
    .bind(ids[5])
    .bind(ids[3])
    .bind(ids[4])
    .execute(pool)
    .await
    .expect("queue a job");

    ids[5].to_string()
}
