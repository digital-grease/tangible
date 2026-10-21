// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Diagnostics, through the real router.
//!
//! Administrators only; the checklist and its Markdown agree; nothing in it
//! names a path; and the bug report version is offered only when reports are
//! on.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::Path;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tangible_api::diagnostics::{DerivationStatus, DiagnosticsSettings};
use tangible_api::report::{BuildIdentity, DEFAULT_ISSUES_URL, ReportSettings};
use tangible_api::{ApiState, router};
use tangible_domain::Role;
use tower::ServiceExt as _;

mod support;

async fn get(
    router: axum::Router,
    path: &str,
    session: Option<&support::Session>,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::get(path).body(Body::empty()).expect("request");
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

fn build() -> BuildIdentity {
    BuildIdentity::current(
        Some("v0.1.0"),
        Some("76fa5157aea012aae85461e694082d553071a606"),
    )
}

fn settings(root: &Path, reports: ReportSettings) -> DiagnosticsSettings {
    DiagnosticsSettings {
        build: build(),
        storage_root: root.join("library"),
        staging_root: root.join("staging"),
        romm_export_root: None,
        derivation: DerivationStatus::Off,
        public_url_https: true,
        migrate_on_start: false,
        reports,
        decryption_allowed: false,
    }
}

fn on() -> ReportSettings {
    ReportSettings::new(DEFAULT_ISSUES_URL, true, build()).expect("settings")
}

/// A server with diagnostics, whose library and staging exist.
fn server(database: tangible_db::Database, root: &Path, reports: ReportSettings) -> axum::Router {
    std::fs::create_dir_all(root.join("library/objects/incoming")).expect("library");
    std::fs::create_dir_all(root.join("staging")).expect("staging");
    router(
        ApiState::new(database)
            .with_reports(reports.clone())
            .with_diagnostics(settings(root, reports)),
    )
}

fn check<'a>(body: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    body["sections"]
        .as_array()
        .expect("sections")
        .iter()
        .flat_map(|section| section["checks"].as_array().expect("checks"))
        .find(|check| check["name"] == name)
        .unwrap_or_else(|| panic!("no check {name} in {body}"))
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_administrator_gets_the_checklist_with_its_markdown() {
    let database = support::database().await;
    let admin = support::administrator(database.pool()).await;
    let root = tempfile::tempdir().expect("tempdir");
    let (status, body) = get(
        server(database, root.path(), on()),
        "/api/v1/diagnostics",
        Some(&admin),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["build"], "v0.1.0 (76fa515)");
    assert_eq!(check(&body, "Connection")["status"], "ok");
    assert_eq!(check(&body, "Schema")["status"], "ok");
    assert!(
        check(&body, "Library")["detail"]
            .as_str()
            .expect("detail")
            .starts_with("present, writable, ")
    );
    let markdown = body["markdown"].as_str().expect("markdown");
    assert!(markdown.starts_with("## Tangible diagnostics\n"));
    assert!(markdown.contains("- **OK** Connection: reachable"));
    let root_text = root.path().display().to_string();
    assert!(!body.to_string().contains(&root_text), "{body}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn only_administrators_may_look() {
    let database = support::database().await;
    let root = tempfile::tempdir().expect("tempdir");
    for role in [Role::Viewer, Role::Operator] {
        let session = support::sign_in(database.pool(), role).await;
        for path in ["/api/v1/diagnostics", "/api/v1/diagnostics/report"] {
            let (status, body) = get(
                server(database.clone(), root.path(), on()),
                path,
                Some(&session),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{role:?} {path}: {body}");
            assert_eq!(body["code"], "FORBIDDEN");
        }
    }
    let (status, _) = get(
        server(database, root.path(), on()),
        "/api/v1/diagnostics",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_report_is_a_scrubbed_bug_report_about_the_deployment() {
    let database = support::database().await;
    let admin = support::administrator(database.pool()).await;
    let root = tempfile::tempdir().expect("tempdir");
    let (status, report) = get(
        server(database, root.path(), on()),
        "/api/v1/diagnostics/report",
        Some(&admin),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["template"], "bug_report.yml");
    assert_eq!(report["long_field"], "logs");
    let fields = report["fields"].as_array().expect("fields");
    let field = |id: &str| {
        fields
            .iter()
            .find(|field| field["id"] == id)
            .and_then(|field| field["value"].as_str())
            .unwrap_or_else(|| panic!("no field {id}"))
            .to_owned()
    };
    assert_eq!(field("area"), "deployment");
    assert!(field("logs").starts_with("## Tangible diagnostics\n"));
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn no_bug_report_while_reports_are_off() {
    let database = support::database().await;
    let admin = support::administrator(database.pool()).await;
    let root = tempfile::tempdir().expect("tempdir");
    let (status, body) = get(
        server(database, root.path(), ReportSettings::disabled(build())),
        "/api/v1/diagnostics/report",
        Some(&admin),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "CONFLICT");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_server_without_diagnostics_says_so() {
    let database = support::database().await;
    let admin = support::administrator(database.pool()).await;
    let (status, body) = get(
        router(ApiState::new(database)),
        "/api/v1/diagnostics",
        Some(&admin),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["code"], "STORAGE_UNAVAILABLE");
}
