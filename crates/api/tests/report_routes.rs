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
