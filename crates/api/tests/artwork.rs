// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Cover art (ADR 0023) through the real router and the artwork runner,
//! with TMDB, its image service and the Cover Art Archive played by a fake
//! on a loopback port.
//!
//! There may be one integration of each metadata provider kind, and the
//! database is shared, so the providers are removed first and last. Other
//! tests' requests want covers too; the ids here are new each run, and the
//! runner is passed until these have all been tried.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::net::SocketAddr;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, Uri, header};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use sqlx::PgPool;
use tangible_api::artwork::ArtworkRunner;
use tangible_api::metadata::{Endpoints, Metadata};
use tangible_api::secrets::SecretKey;
use tangible_api::{ApiState, router};
use tangible_integrations::metadata::igdb;
use tangible_integrations::outbound::{OutboundClient, OutboundPolicy, Url};
use tower::ServiceExt as _;

mod support;

const TMDB_TOKEN: &str = "tmdb-read-access-token-for-the-fake";

/// A PNG's signature and a little more: what the fake serves as a poster.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR-a-poster";
/// A JPEG's start: what it serves as a Cover Art Archive front.
const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 16, b'J', b'F', b'I', b'F'];

/// TMDB and the image hosts, by path.
async fn answer(uri: Uri, headers: axum::http::HeaderMap) -> axum::response::Response {
    let path = uri.path();
    let json = |text: String| (StatusCode::OK, [("content-type", "application/json")], text);
    if let Some(rest) = path.strip_prefix("/3/") {
        let authorized = headers.get("authorization").and_then(|v| v.to_str().ok())
            == Some(&format!("Bearer {TMDB_TOKEN}"));
        if !authorized {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        // movie/1<n> has a poster; movie/2<n> has none; movie/3<n>'s poster
        // is not an image; movie/4<n> does not exist.
        if let Some(id) = rest.strip_prefix("movie/") {
            return match id.chars().next() {
                Some('1') => {
                    json(format!(r#"{{"id":{id},"poster_path":"/p{id}.png"}}"#)).into_response()
                }
                Some('2') => json(format!(r#"{{"id":{id},"poster_path":null}}"#)).into_response(),
                Some('3') => {
                    json(format!(r#"{{"id":{id},"poster_path":"/html{id}.png"}}"#)).into_response()
                }
                _ => StatusCode::NOT_FOUND.into_response(),
            };
        }
        if let Some(imdb) = rest.strip_prefix("find/") {
            return json(format!(
                r#"{{"movie_results":[{{"id":7,"poster_path":"/i{imdb}.png"}}],"tv_results":[]}}"#
            ))
            .into_response();
        }
        return StatusCode::NOT_FOUND.into_response();
    }
    if let Some(name) = path.strip_prefix("/images/w342/") {
        if name.starts_with("html") {
            return (
                StatusCode::OK,
                [("content-type", "image/png")],
                "<!doctype html>",
            )
                .into_response();
        }
        return (StatusCode::OK, [("content-type", "image/png")], PNG).into_response();
    }
    // The Cover Art Archive redirects to where the image is.
    if path.starts_with("/caa/release/") && path.ends_with("/front-500") {
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [(header::LOCATION, "/archive/front.jpg")],
        )
            .into_response();
    }
    if path == "/archive/front.jpg" {
        return (StatusCode::OK, [("content-type", "image/jpeg")], JPEG).into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}

async fn fake() -> SocketAddr {
    let app = axum::Router::new().fallback(answer);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    address
}

async fn remove_providers(pool: &PgPool) {
    sqlx::query(
        "DELETE FROM integration_secrets WHERE integration_id IN
             (SELECT id FROM integrations WHERE kind IN ('musicbrainz', 'tmdb', 'igdb'))",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM integrations WHERE kind IN ('musicbrainz', 'tmdb', 'igdb')")
        .execute(pool)
        .await
        .unwrap();
}

static TURN: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

struct Harness {
    pool: PgPool,
    state: ApiState,
    admin: support::Session,
    _turn: tokio::sync::MutexGuard<'static, ()>,
}

impl Harness {
    async fn new() -> Self {
        let turn = TURN
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let database = support::database().await;
        let pool = database.pool().clone();
        remove_providers(&pool).await;
        let address = fake().await;
        let base = |path: &str| Url::parse(&format!("http://{address}{path}")).unwrap();
        let client = OutboundClient::new(
            OutboundPolicy::for_integrations(vec!["127.0.0.1/32".parse().unwrap()], None),
            tangible_api::metadata::USER_AGENT,
        )
        .unwrap();
        let metadata = Metadata::new(
            client,
            Endpoints {
                musicbrainz: base("/"),
                tmdb: base("/3/"),
                igdb: igdb::Endpoints {
                    api: base("/v4/"),
                    token: base("/oauth2/token"),
                },
                artwork: tangible_integrations::artwork::Endpoints {
                    tmdb_images: base("/images/"),
                    cover_art_archive: base("/caa/"),
                    igdb_images: base("/igdb/"),
                    libretro: base("/libretro/"),
                },
            },
        );
        let key = SecretKey::from_base64(&SecretKey::generate().unwrap()).unwrap();
        let state = ApiState::new(database)
            .with_secret_key(key)
            .with_metadata(metadata);
        Self {
            admin: support::administrator(&pool).await,
            pool,
            state,
            _turn: turn,
        }
    }

    async fn send(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let (status, _, bytes) = self.raw(method, path, body).await;
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn raw(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let mut request = Request::builder().method(method).uri(path);
        if body.is_some() {
            request = request.header("content-type", "application/json");
        }
        let mut request = request
            .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
            .unwrap();
        self.admin.apply(&mut request);
        let response = router(self.state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 4 << 20)
            .await
            .unwrap()
            .to_vec();
        (status, headers, bytes)
    }

    async fn provider(&self, kind: &str, secrets: Value) {
        let (status, made) = self
            .send(
                Method::POST,
                "/api/v1/integrations",
                Some(json!({
                    "kind": kind,
                    "name": format!("{kind} {}", uuid::Uuid::now_v7().simple()),
                    "settings": {},
                    "secrets": secrets,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
    }

    async fn request(&self, title: &str, provider: &str, external_id: &str) -> String {
        let (status, made) = self
            .send(
                Method::POST,
                "/api/v1/requests",
                Some(json!({
                    "kind": "movie",
                    "title": title,
                    "refs": [{ "provider": provider, "external_id": external_id }],
                    "attests_ownership": true,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
        made["id"].as_str().unwrap().to_owned()
    }

    async fn state_of(&self, source: &str, external_id: &str) -> Option<String> {
        sqlx::query_scalar("SELECT state FROM artwork WHERE source = $1 AND external_id = $2")
            .bind(source)
            .bind(external_id)
            .fetch_optional(&self.pool)
            .await
            .unwrap()
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn covers_are_fetched_checked_and_served_for_requests() {
    let harness = Harness::new().await;
    harness
        .provider("tmdb", json!({ "read_access_token": TMDB_TOKEN }))
        .await;
    harness.provider("musicbrainz", json!({})).await;

    // Ids new each run, so this run's covers are this run's.
    let n = uuid::Uuid::now_v7().as_u128() % 1_000_000_000;
    let poster = format!("movie/1{n}");
    let none = format!("movie/2{n}");
    let html = format!("movie/3{n}");
    let imdb = format!("tt{:07}", n % 10_000_000);
    let release = uuid::Uuid::now_v7().to_string();
    let with_poster = harness.request("With a poster", "tmdb", &poster).await;
    let without = harness.request("Without one", "tmdb", &none).await;
    harness.request("Not an image", "tmdb", &html).await;
    let found_by_imdb = harness.request("By IMDb", "imdb", &imdb).await;
    let album = harness.request("An album", "musicbrainz", &release).await;

    let runner = ArtworkRunner::new(harness.state.clone());
    let mine = [
        ("tmdb", poster.as_str()),
        ("tmdb", none.as_str()),
        ("tmdb", html.as_str()),
        ("imdb", imdb.as_str()),
        ("musicbrainz", release.as_str()),
    ];
    for _ in 0..30 {
        runner.pass().await.unwrap();
        let mut settled = true;
        for (source, id) in mine {
            if harness.state_of(source, id).await.as_deref() == Some("pending") {
                settled = false;
            }
        }
        if settled {
            break;
        }
    }
    assert_eq!(
        harness.state_of("tmdb", &poster).await.as_deref(),
        Some("ready")
    );
    assert_eq!(
        harness.state_of("imdb", &imdb).await.as_deref(),
        Some("ready")
    );
    assert_eq!(
        harness.state_of("musicbrainz", &release).await.as_deref(),
        Some("ready"),
        "the Cover Art Archive's redirect was followed"
    );
    assert_eq!(
        harness.state_of("tmdb", &none).await.as_deref(),
        Some("missing")
    );
    assert_eq!(
        harness.state_of("tmdb", &html).await.as_deref(),
        Some("failed"),
        "what is not an image is not kept"
    );

    // A request shows its cover; one with none shows none.
    let (status, request) = harness
        .send(
            Method::GET,
            &format!("/api/v1/requests/{with_poster}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{request}");
    assert_eq!(request["cover"]["source"], "tmdb");
    let url = request["cover"]["url"].as_str().unwrap().to_owned();
    assert!(url.starts_with("/api/v1/artwork/"), "{url}");
    let (_, without) = harness
        .send(Method::GET, &format!("/api/v1/requests/{without}"), None)
        .await;
    assert!(without["cover"].is_null(), "{without}");
    let (_, listed) = harness
        .send(Method::GET, "/api/v1/requests?limit=50", None)
        .await;
    let shown = |id: &str| {
        listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == id)
            .map(|item| item["cover"]["source"].clone())
    };
    assert_eq!(shown(&found_by_imdb), Some(json!("imdb")));
    assert_eq!(shown(&album), Some(json!("musicbrainz")));

    served_as_checked(&harness, &url).await;
    remove_providers(&harness.pool).await;
}

/// The cover at `url` is served as checked, never sniffed, and kept as long
/// as a browser likes; one that does not exist is not.
async fn served_as_checked(harness: &Harness, url: &str) {
    let (status, headers, bytes) = harness.raw(Method::GET, url, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, PNG);
    assert_eq!(headers[header::CONTENT_TYPE], "image/png");
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert!(
        headers[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("immutable")
    );
    let (status, _) = harness
        .send(
            Method::GET,
            &format!("/api/v1/artwork/{}", uuid::Uuid::now_v7()),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_provider_not_configured_is_tried_again_later() {
    let harness = Harness::new().await;
    let n = uuid::Uuid::now_v7().as_u128() % 1_000_000_000;
    let poster = format!("movie/1{n}");
    harness.request("No TMDB yet", "tmdb", &poster).await;
    let runner = ArtworkRunner::new(harness.state.clone());
    for _ in 0..30 {
        runner.pass().await.unwrap();
        if harness.state_of("tmdb", &poster).await.as_deref() != Some("pending") {
            break;
        }
    }
    assert_eq!(
        harness.state_of("tmdb", &poster).await.as_deref(),
        Some("failed")
    );
    let (error, later): (String, bool) = sqlx::query_as(
        "SELECT error, next_attempt_at > now() + interval '50 minutes'
         FROM artwork WHERE source = 'tmdb' AND external_id = $1",
    )
    .bind(&poster)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert!(error.contains("not configured"), "{error}");
    assert!(later, "an hour from now, not at once");
}

