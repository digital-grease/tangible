// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The web UI, served beside the API from the same origin.
//!
//! Needs no database: the UI's files are public, and the one API request
//! here is refused before any lookup because it carries no cookie.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tangible_api::{ApiState, WebUi, router};
use tangible_db::{Database, DbConfig};
use tempfile::TempDir;
use tower::ServiceExt as _;

const PAGE: &str = r#"<!doctype html><html><head><meta http-equiv="content-security-policy" content="default-src 'self'; script-src 'self' 'sha256-abc='"></head><body>tangible ui</body></html>"#;

fn database() -> Database {
    Database::connect_lazy(&DbConfig::new(
        "postgres://tangible:tangible@127.0.0.1:1/tangible",
    ))
    .expect("lazy pool")
}

/// A build with a page and one hashed asset, and a secret beside it that
/// must never be reachable.
async fn server() -> (TempDir, axum::Router) {
    let dir = TempDir::new().unwrap();
    let build = dir.path().join("build");
    std::fs::create_dir_all(build.join("_app/immutable/entry")).unwrap();
    std::fs::write(build.join("index.html"), PAGE).unwrap();
    std::fs::write(
        build.join("_app/immutable/entry/start.abc.js"),
        "export {};",
    )
    .unwrap();
    std::fs::write(dir.path().join("secret.txt"), "not for you").unwrap();
    let web = WebUi::open(&build).await.unwrap();
    (dir, router(ApiState::new(database()).with_web(web)))
}

struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: String,
}

async fn send(router: &axum::Router, method: &str, uri: &str) -> Reply {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    Reply {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

fn header_of(reply: &Reply, name: header::HeaderName) -> &str {
    reply
        .headers
        .get(name)
        .map(|value| value.to_str().unwrap())
        .unwrap_or_default()
}

#[tokio::test]
async fn the_ui_is_served_at_the_root_and_at_every_page_it_routes() {
    let (_dir, router) = server().await;
    for uri in ["/", "/library", "/burns/0198-abc", "/login?next=%2Fburns"] {
        let reply = send(&router, "GET", uri).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}");
        assert!(reply.body.contains("tangible ui"), "{uri}");
        assert!(header_of(&reply, header::CONTENT_TYPE).starts_with("text/html"));
    }
}

#[tokio::test]
async fn the_page_gets_its_own_policy_plus_frame_ancestors_and_is_revalidated() {
    let (_dir, router) = server().await;
    let reply = send(&router, "GET", "/library").await;
    assert_eq!(
        header_of(&reply, header::CONTENT_SECURITY_POLICY),
        "default-src 'self'; script-src 'self' 'sha256-abc='; frame-ancestors 'none'"
    );
    assert_eq!(header_of(&reply, header::CACHE_CONTROL), "no-cache");
    assert_eq!(header_of(&reply, header::X_CONTENT_TYPE_OPTIONS), "nosniff");
    assert_eq!(header_of(&reply, header::X_FRAME_OPTIONS), "DENY");
}

#[tokio::test]
async fn hashed_assets_are_cached_for_good_and_missing_ones_are_not_the_page() {
    let (_dir, router) = server().await;
    let asset = send(&router, "GET", "/_app/immutable/entry/start.abc.js").await;
    assert_eq!(asset.status, StatusCode::OK);
    assert_eq!(asset.body, "export {};");
    assert_eq!(
        header_of(&asset, header::CACHE_CONTROL),
        "public, max-age=31536000, immutable"
    );

    let missing = send(&router, "GET", "/_app/immutable/entry/start.old.js").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert!(!missing.body.contains("tangible ui"));
}

#[tokio::test]
async fn nothing_outside_the_build_is_reachable() {
    let (_dir, router) = server().await;
    for uri in [
        "/../secret.txt",
        "/%2e%2e/secret.txt",
        "/_app/%2e%2e/%2e%2e/secret.txt",
    ] {
        let reply = send(&router, "GET", uri).await;
        assert!(!reply.body.contains("not for you"), "{uri} leaked");
    }
}

#[tokio::test]
async fn the_api_keeps_its_own_answers_and_policy() {
    let (_dir, router) = server().await;

    // An API route still needs a session; the UI is no way round that.
    let refused = send(&router, "GET", "/api/v1/artifacts").await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        header_of(&refused, header::CONTENT_SECURITY_POLICY),
        "default-src 'none'; frame-ancestors 'none'"
    );
    assert_eq!(header_of(&refused, header::CACHE_CONTROL), "no-store");

    // A mistyped API route is a JSON 404, never the UI's page.
    let unknown = send(&router, "GET", "/api/v1/no-such-thing").await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert!(header_of(&unknown, header::CONTENT_TYPE).contains("problem+json"));
    assert!(unknown.body.contains("NOT_FOUND"));
    assert!(!unknown.body.contains("tangible ui"));

    let probe = send(&router, "GET", "/livez").await;
    assert_eq!(probe.status, StatusCode::OK);
}

#[tokio::test]
async fn the_ui_accepts_no_writes() {
    let (_dir, router) = server().await;
    let reply = send(&router, "POST", "/library").await;
    assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn a_server_without_a_ui_says_so_in_json() {
    let router = router(ApiState::new(database()));
    let reply = send(&router, "GET", "/").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(reply.body.contains("web UI is not installed"));
    assert_eq!(
        header_of(&reply, header::CONTENT_SECURITY_POLICY),
        "default-src 'none'; frame-ancestors 'none'"
    );
}
