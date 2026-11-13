// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Requests for items through the real router (ADR 0021,
//! docs/plans/requests.md Q1): who may ask, who decides, who sees what, and
//! every refusal.
//!
//! The database is shared, so each test signs in accounts of its own and
//! reads back only the requests they made.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tangible_api::routes::requests::{MAX_DETAILS, MAX_JSON_BYTES};
use tangible_api::{ApiState, router};
use tangible_domain::Role;
use tower::ServiceExt as _;

mod support;

async fn send(
    router: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
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

async fn get(router: &axum::Router, uri: &str) -> (StatusCode, Value) {
    send(router, "GET", uri, None).await
}

async fn post(router: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    send(router, "POST", uri, Some(body)).await
}

/// A request as the form sends it, attested.
fn asking(title: &str) -> Value {
    json!({
        "kind": "game",
        "title": title,
        "details": { "platform": "Sony PlayStation", "region": "USA" },
        "refs": [{ "provider": "dat", "external_id": title }],
        "note": "my copy is scratched",
        "attests_ownership": true,
    })
}

struct Harness {
    database: tangible_db::Database,
    app: axum::Router,
}

impl Harness {
    async fn new() -> Self {
        let database = support::database().await;
        // A state of its own, so the rate limit counts this test alone.
        let app = router(ApiState::new(database.clone()));
        Self { database, app }
    }

    async fn as_role(&self, role: Role) -> axum::Router {
        support::signed_in(
            self.app.clone(),
            support::sign_in(self.database.pool(), role).await,
        )
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_request_is_refused_without_the_attestation() {
    let harness = Harness::new().await;
    let viewer = harness.as_role(Role::Viewer).await;

    for attestation in [json!(false), Value::Null] {
        let mut body = asking("Unattested");
        match attestation {
            Value::Null => {
                body.as_object_mut().unwrap().remove("attests_ownership");
            }
            value => body["attests_ownership"] = value,
        }
        let (status, problem) = post(&viewer, "/api/v1/requests", body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(problem["code"], "ATTESTATION_REQUIRED");
        assert!(
            problem["detail"]
                .as_str()
                .unwrap()
                .contains("I legally own this item"),
            "the refusal says what is attested: {problem}"
        );
    }
    let (_, page) = get(&viewer, "/api/v1/requests").await;
    assert!(
        page["items"].as_array().unwrap().is_empty(),
        "nothing stored"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_viewers_request_waits_and_an_operators_is_approved_as_made() {
    let harness = Harness::new().await;
    let viewer = harness.as_role(Role::Viewer).await;
    let operator = harness.as_role(Role::Operator).await;

    let (status, made) = post(&viewer, "/api/v1/requests", asking("  Waiting Game ")).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(made["state"], "pending_approval");
    assert_eq!(made["title"], "Waiting Game", "trimmed");
    assert_eq!(made["kind"], "game");
    assert_eq!(made["details"]["platform"], "Sony PlayStation");
    assert_eq!(made["refs"][0]["provider"], "dat");
    assert_eq!(made["note"], "my copy is scratched");
    assert_eq!(made["decided_by"], Value::Null);
    assert_eq!(
        (
            made["can_approve"].as_bool(),
            made["can_decline"].as_bool(),
            made["can_cancel"].as_bool()
        ),
        (Some(false), Some(false), Some(true)),
        "a viewer may only withdraw their own"
    );
    assert!(
        made.get("attests_ownership").is_none(),
        "not stored, not shown"
    );

    let (status, own) = post(&operator, "/api/v1/requests", asking("Own Game")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(own["state"], "approved");
    assert!(own["decided_by"].as_str().unwrap().starts_with("test-"));
    assert!(own["decided_at"].is_string());
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn only_an_operator_or_administrator_decides() {
    let harness = Harness::new().await;
    let viewer = harness.as_role(Role::Viewer).await;
    let operator = harness.as_role(Role::Operator).await;
    let (_, made) = post(&viewer, "/api/v1/requests", asking("Decided Game")).await;
    let id = made["id"].as_str().unwrap();

    let (status, problem) = post(
        &viewer,
        &format!("/api/v1/requests/{id}/approve"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(problem["code"], "FORBIDDEN");
    let (status, _) = post(
        &viewer,
        &format!("/api/v1/requests/{id}/decline"),
        json!({ "reason": "no" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // The operator sees it, and may decide it.
    let (status, seen) = get(&operator, &format!("/api/v1/requests/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(seen["can_approve"], true);
    assert_eq!(seen["can_decline"], true);
    let (status, approved) = post(
        &operator,
        &format!("/api/v1/requests/{id}/approve"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    assert_eq!(approved["state"], "approved");
    assert_eq!(approved["can_approve"], false);
    assert_eq!(
        approved["can_decline"], true,
        "approved may still be declined"
    );

    // Approving twice is a conflict that says where it stands.
    let (status, problem) = post(
        &operator,
        &format!("/api/v1/requests/{id}/approve"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(problem["code"], "CONFLICT");
    assert!(problem["detail"].as_str().unwrap().contains("approved"));

    // The requester sees the decision.
    let (_, seen) = get(&viewer, &format!("/api/v1/requests/{id}")).await;
    assert_eq!(seen["state"], "approved");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_decline_says_why_and_ends_the_request() {
    let harness = Harness::new().await;
    let viewer = harness.as_role(Role::Viewer).await;
    let operator = harness.as_role(Role::Operator).await;
    let (_, made) = post(&viewer, "/api/v1/requests", asking("Declined Game")).await;
    let id = made["id"].as_str().unwrap();
    let decline = format!("/api/v1/requests/{id}/decline");

    for reason in [
        json!({ "reason": "   " }),
        json!({ "reason": "x".repeat(2001) }),
    ] {
        let (status, problem) = post(&operator, &decline, reason).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(problem["code"], "VALIDATION_FAILED");
    }
    let (status, declined) = post(
        &operator,
        &decline,
        json!({ "reason": "already in the library" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(declined["state"], "declined");

    let (_, seen) = get(&viewer, &format!("/api/v1/requests/{id}")).await;
    assert_eq!(seen["decline_reason"], "already in the library");
    assert_eq!(seen["can_cancel"], false, "an end is an end");
    let (status, _) = post(&viewer, &format!("/api/v1/requests/{id}/cancel"), json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = post(
        &operator,
        &format!("/api/v1/requests/{id}/approve"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_viewer_sees_and_withdraws_only_their_own() {
    let harness = Harness::new().await;
    let one = harness.as_role(Role::Viewer).await;
    let other = harness.as_role(Role::Viewer).await;
    let operator = harness.as_role(Role::Operator).await;
    let (_, mine) = post(&one, "/api/v1/requests", asking("Mine")).await;
    let (_, theirs) = post(&other, "/api/v1/requests", asking("Theirs")).await;
    let mine = mine["id"].as_str().unwrap().to_owned();
    let theirs = theirs["id"].as_str().unwrap().to_owned();

    let (_, page) = get(&one, "/api/v1/requests").await;
    let ids: Vec<&str> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [mine.as_str()]);

    // Another's request is not there at all, to read or to withdraw.
    let (status, problem) = get(&one, &format!("/api/v1/requests/{theirs}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(problem["code"], "NOT_FOUND");
    let (status, _) = post(
        &one,
        &format!("/api/v1/requests/{theirs}/cancel"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // An operator sees both, filtered by state too.
    let (_, page) = get(
        &operator,
        "/api/v1/requests?state=pending_approval&limit=500",
    )
    .await;
    let ids: Vec<&str> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&mine.as_str()) && ids.contains(&theirs.as_str()));
    let (status, problem) = get(&operator, "/api/v1/requests?state=searching").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["code"], "INVALID_PARAMETER");

    // Withdrawing: the requester their own, an operator anybody's.
    let (status, withdrawn) =
        post(&one, &format!("/api/v1/requests/{mine}/cancel"), json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(withdrawn["state"], "canceled");
    let (status, withdrawn) = post(
        &operator,
        &format!("/api/v1/requests/{theirs}/cancel"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(withdrawn["state"], "canceled");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_page_continues_after_its_cursor() {
    let harness = Harness::new().await;
    let viewer = harness.as_role(Role::Viewer).await;
    for n in 0..3 {
        post(&viewer, "/api/v1/requests", asking(&format!("Paged {n}"))).await;
    }
    let (_, first) = get(&viewer, "/api/v1/requests?limit=2").await;
    assert_eq!(first["items"].as_array().unwrap().len(), 2);
    assert_eq!(first["items"][0]["title"], "Paged 2", "newest first");
    let cursor = first["next_cursor"].as_str().unwrap();
    let (_, rest) = get(
        &viewer,
        &format!("/api/v1/requests?limit=2&cursor={cursor}"),
    )
    .await;
    assert_eq!(rest["items"].as_array().unwrap().len(), 1);
    assert_eq!(rest["items"][0]["title"], "Paged 0");
    assert_eq!(rest["next_cursor"], Value::Null);
    let (status, problem) = get(&viewer, "/api/v1/requests?cursor=nonsense").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["code"], "INVALID_CURSOR");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_largest_request_allowed_is_stored_and_one_byte_more_is_refused() {
    let harness = Harness::new().await;
    let viewer = harness.as_role(Role::Viewer).await;

    // Eight details of 40-character names whose compact JSON is exactly
    // MAX_JSON_BYTES: 2 braces, 7 commas, and per entry a quoted name, a
    // colon and a quoted value.
    let names: Vec<String> = (0..MAX_DETAILS)
        .map(|n| format!("{n}{}", "d".repeat(39)))
        .collect();
    let fixed = 2 + (MAX_DETAILS - 1) + MAX_DETAILS * (40 + 2 + 1 + 2);
    let value_bytes = MAX_JSON_BYTES - fixed;
    let disc = "\u{1f4bf}"; // four bytes, one character
    let value_of = |bytes: usize| format!("{}{}", disc.repeat(bytes / 4), "x".repeat(bytes % 4));
    let details = |extra: usize| -> Value {
        let mut map = serde_json::Map::new();
        for (n, name) in names.iter().enumerate() {
            let share = value_bytes / MAX_DETAILS + usize::from(n < value_bytes % MAX_DETAILS);
            let bytes = if n == 0 { share + extra } else { share };
            map.insert(name.clone(), Value::String(value_of(bytes)));
        }
        Value::Object(map)
    };
    let at_bound = details(0);
    assert_eq!(at_bound.to_string().len(), MAX_JSON_BYTES);

    let mut body = asking("Large Game");
    body["details"] = at_bound.clone();
    let (status, made) = post(&viewer, "/api/v1/requests", body.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(made["details"], at_bound, "stored as sent");

    body["details"] = details(1);
    let (status, problem) = post(&viewer, "/api/v1/requests", body).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem["code"], "VALIDATION_FAILED");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_account_asks_at_most_thirty_times_an_hour() {
    let harness = Harness::new().await;
    let viewer = harness.as_role(Role::Viewer).await;
    for n in 0..30 {
        let (status, _) = post(&viewer, "/api/v1/requests", asking(&format!("Limited {n}"))).await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/requests")
        .header("content-type", "application/json")
        .body(Body::from(asking("One too many").to_string()))
        .unwrap();
    let response = viewer.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().contains_key("retry-after"));

    // A refused request does not count against the allowance.
    let other = harness.as_role(Role::Viewer).await;
    let mut unattested = asking("Unattested");
    unattested["attests_ownership"] = json!(false);
    for _ in 0..40 {
        post(&other, "/api/v1/requests", unattested.clone()).await;
    }
    let (status, _) = post(&other, "/api/v1/requests", asking("Still allowed")).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_request_is_audited_and_its_attestation_is_not() {
    let harness = Harness::new().await;
    let viewer = harness.as_role(Role::Viewer).await;
    let operator = harness.as_role(Role::Operator).await;
    let (_, made) = post(&viewer, "/api/v1/requests", asking("Audited Game")).await;
    let id = made["id"].as_str().unwrap();
    post(
        &operator,
        &format!("/api/v1/requests/{id}/approve"),
        json!({}),
    )
    .await;

    let rows: Vec<(String, Value)> = sqlx::query_as(
        "SELECT action, metadata FROM audit_events WHERE target_id = $1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(harness.database.pool())
    .await
    .unwrap();
    let actions: Vec<&str> = rows.iter().map(|(action, _)| action.as_str()).collect();
    assert_eq!(actions, ["request.created", "request.approved"]);
    for (_, metadata) in &rows {
        assert!(
            !metadata.to_string().contains("attest"),
            "the attestation is not recorded: {metadata}"
        );
    }
}
