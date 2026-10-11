// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Issue report settings, through the real router.
//!
//! The web UI decides whether to offer "report this" from this route, so it
//! must say "off" unless an address was configured, name the build, and be
//! readable by every signed-in account and by nobody else.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tangible_api::report::{BuildIdentity, DEFAULT_ISSUES_URL, ReportSettings};
use tangible_api::{ApiState, router};
use tangible_domain::Role;
use tower::ServiceExt as _;

mod support;

async fn get(
    router: axum::Router,
    session: Option<&support::Session>,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::get("/api/v1/report-settings")
        .body(Body::empty())
        .expect("request");
    if let Some(session) = session {
        session.apply(&mut request);
    }
    let response = router.oneshot(request).await.expect("route the request");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn reports_are_off_until_an_address_is_configured() {
    let database = support::database().await;
    let viewer = support::sign_in(database.pool(), Role::Viewer).await;
    let (status, body) = get(router(ApiState::new(database)), Some(&viewer)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["enabled"], false);
    assert!(body["issues_url"].is_null());
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_configured_server_offers_reports_and_names_its_build() {
    let database = support::database().await;
    let viewer = support::sign_in(database.pool(), Role::Viewer).await;
    let build = BuildIdentity::current(
        Some("v0.1.0"),
        Some("76fa5157aea012aae85461e694082d553071a606"),
    );
    let settings = ReportSettings::new(DEFAULT_ISSUES_URL, true, build).expect("settings");
    let state = ApiState::new(database).with_reports(settings);
    let (status, body) = get(router(state), Some(&viewer)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["enabled"], true);
    assert_eq!(body["issues_url"], DEFAULT_ISSUES_URL);
    assert_eq!(body["release"], "v0.1.0");
    assert_eq!(body["revision"], "76fa5157aea012aae85461e694082d553071a606");
    assert_eq!(body["build"], "v0.1.0 (76fa515)");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn nobody_signed_out_can_read_them() {
    let database = support::database().await;
    let (status, body) = get(router(ApiState::new(database)), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["code"], "UNAUTHENTICATED");
    // Refused in the middleware, before any handler, and still named, so the
    // refusal can be found in the log.
    let id = body["request_id"].as_str().expect("a request id");
    assert_eq!(
        uuid::Uuid::parse_str(id).expect("a UUID").get_version_num(),
        7
    );
}

async fn post_error(
    router: axum::Router,
    session: &support::Session,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::post("/api/v1/error-reports")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request");
    session.apply(&mut request);
    let response = router.oneshot(request).await.expect("route the request");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

fn configured(database: tangible_db::Database) -> axum::Router {
    let build = BuildIdentity::current(
        Some("v0.1.0"),
        Some("76fa5157aea012aae85461e694082d553071a606"),
    );
    let settings = ReportSettings::new(DEFAULT_ISSUES_URL, true, build).expect("settings");
    router(ApiState::new(database).with_reports(settings))
}

fn field<'a>(report: &'a serde_json::Value, id: &str) -> &'a str {
    report["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .find(|field| field["id"] == id)
        .and_then(|field| field["value"].as_str())
        .unwrap_or_else(|| panic!("no field {id} in {report}"))
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_viewer_can_have_an_error_written_up_scrubbed() {
    let database = support::database().await;
    let viewer = support::sign_in(database.pool(), Role::Viewer).await;
    let (status, report) = post_error(
        configured(database),
        &viewer,
        serde_json::json!({
            "code": "INTERNAL",
            "title": "Internal server error",
            "detail": "could not read /var/lib/tangible/library/objects/ab from 10.0.0.5",
            "status": 500,
            "request_id": "01a126e9-2507-7133-8bfa-b724fc014c1f",
            "endpoint": "GET /api/v1/artifacts/{artifact_id}",
            "area": "library",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["template"], "bug_report.yml");
    assert_eq!(field(&report, "version"), "v0.1.0 (76fa515)");
    assert_eq!(field(&report, "area"), "library");
    let logs = field(&report, "logs");
    assert!(
        logs.contains("request id: 01a126e9-2507-7133-8bfa-b724fc014c1f"),
        "{logs}"
    );
    assert!(
        !logs.contains("/var/lib") && !logs.contains("10.0.0.5"),
        "{logs}"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_error_report_repeats_only_a_real_request_id() {
    let database = support::database().await;
    let viewer = support::sign_in(database.pool(), Role::Viewer).await;
    let (status, report) = post_error(
        configured(database),
        &viewer,
        serde_json::json!({
            "code": "INTERNAL",
            "title": "Internal server error",
            "detail": "x",
            "request_id": "see https://example.org/free-prizes",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert!(!field(&report, "logs").contains("request id"), "{report}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_error_report_refuses_what_is_not_shaped_like_a_problem() {
    let database = support::database().await;
    let viewer = support::sign_in(database.pool(), Role::Viewer).await;
    let app = configured(database);
    for body in [
        serde_json::json!({ "code": "not a code", "title": "t", "detail": "d" }),
        serde_json::json!({ "code": "INTERNAL", "title": "t".repeat(201), "detail": "d" }),
        serde_json::json!({ "code": "INTERNAL", "title": "t", "detail": "d".repeat(4001) }),
    ] {
        let (status, reply) = post_error(app.clone(), &viewer, body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{reply}");
        assert_eq!(reply["code"], "VALIDATION_FAILED");
    }
    let (status, _) = post_error(
        app,
        &viewer,
        serde_json::json!({ "code": "INTERNAL", "title": "t", "detail": "d", "extra": 1 }),
    )
    .await;
    assert!(status.is_client_error(), "an unknown field is refused");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_server_with_reports_off_writes_no_error_report() {
    let database = support::database().await;
    let viewer = support::sign_in(database.pool(), Role::Viewer).await;
    let (status, reply) = post_error(
        router(ApiState::new(database)),
        &viewer,
        serde_json::json!({ "code": "INTERNAL", "title": "t", "detail": "d" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
}
