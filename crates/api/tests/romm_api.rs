// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! RomM's API as an integration (docs/plans/romm-api.md M1), through the
//! real router, against a fake RomM serving what RomM 5.4.1 answered
//! (fixtures/tool-output/romm), and against a real one when
//! `TANGIBLE_TEST_ROMM_URL` names it.
//!
//! Trying it says, check by check, what Tangible can do with it: find
//! exported games, ask for scans, know platforms. A token is sent as a
//! bearer token and never answered back. There is one export, so there may
//! be one RomM API integration; the tests here take turns, and each removes
//! the one it made.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use sqlx::PgPool;
use tangible_api::secrets::SecretKey;
use tangible_api::{ApiState, router};
use tangible_integrations::outbound::{OutboundClient, OutboundPolicy};
use tangible_storage::{FilesystemStore, ManifestStore};
use tempfile::TempDir;
use tower::ServiceExt as _;

mod support;

const TOKEN: &str = "rmm_fake-token-for-the-fake-romm";

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/../../fixtures/tool-output/romm/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// What the fake RomM answers.
struct Fake {
    document: Value,
    config: Value,
    /// Scopes its token lacks: their routes answer 403.
    denied: Vec<&'static str>,
    /// Authorization headers it was sent, to check none leaks elsewhere.
    seen_tokens: Vec<String>,
}

type Shared = Arc<Mutex<Fake>>;

async fn answer(
    State(fake): State<Shared>,
    method: Method,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> axum::response::Response {
    let path = uri.path().to_owned();
    match path.as_str() {
        "/api/heartbeat" => {
            return (StatusCode::OK, fixture("heartbeat-5.4.1.json")).into_response();
        }
        "/openapi.json" => {
            let document = fake.lock().unwrap().document.to_string();
            return (StatusCode::OK, document).into_response();
        }
        "/api/config" => {
            let config = fake.lock().unwrap().config.to_string();
            return (StatusCode::OK, config).into_response();
        }
        _ => {}
    }
    let sent = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let mut fake = fake.lock().unwrap();
    fake.seen_tokens.push(sent.clone());
    if sent != format!("Bearer {TOKEN}") {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({ "detail": "Not authenticated" })),
        )
            .into_response();
    }
    let scope = match (method.as_str(), path.as_str()) {
        ("GET", "/api/platforms") => "platforms.read",
        ("GET", "/api/roms") => "roms.read",
        ("GET", "/api/tasks") => "tasks.run",
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    if fake.denied.contains(&scope) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(json!({ "detail": "Forbidden" })),
        )
            .into_response();
    }
    let body = match scope {
        "platforms.read" => fixture("platforms-5.4.1.json"),
        "roms.read" => fixture("roms-limit-1-5.4.1.json"),
        _ => json!({ "scheduled": [], "manual": [], "watcher": [] }).to_string(),
    };
    (StatusCode::OK, body).into_response()
}

async fn fake_romm() -> (SocketAddr, Shared) {
    let fake: Shared = Arc::new(Mutex::new(Fake {
        document: serde_json::from_str(&fixture("openapi-5.4.1-used.json")).unwrap(),
        config: serde_json::from_str(&fixture("config-5.4.1.json")).unwrap(),
        denied: Vec::new(),
        seen_tokens: Vec::new(),
    }));
    let app = axum::Router::new()
        .fallback(answer)
        .with_state(Arc::clone(&fake));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (address, fake)
}

static TURN: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

struct Harness {
    pool: PgPool,
    state: ApiState,
    admin: support::Session,
    romm: SocketAddr,
    fake: Shared,
    made: Vec<String>,
    _dir: TempDir,
    _turn: tokio::sync::MutexGuard<'static, ()>,
}

async fn harness() -> Harness {
    let turn = TURN
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let database = support::database().await;
    let pool = database.pool().clone();
    // One left by a test that panicked would stand in the way of the rest.
    remove_romm_api(&pool).await;
    let dir = TempDir::new().unwrap();
    let objects = FilesystemStore::open(dir.path().join("library"))
        .await
        .unwrap();
    let manifests = ManifestStore::open(objects).await.unwrap();
    let client = OutboundClient::new(
        OutboundPolicy::for_integrations(vec!["127.0.0.1/32".parse().unwrap()], None),
        "tangible-test",
    )
    .unwrap();
    let key = SecretKey::from_base64(&SecretKey::generate().unwrap()).unwrap();
    let state = ApiState::with_manifests(database, manifests)
        .with_secret_key(key)
        .with_integration_client(client);
    let (romm, fake) = fake_romm().await;
    Harness {
        admin: support::administrator(&pool).await,
        pool,
        state,
        romm,
        fake,
        made: Vec::new(),
        _dir: dir,
        _turn: turn,
    }
}

async fn remove_romm_api(pool: &PgPool) {
    sqlx::query(
        "DELETE FROM integration_secrets WHERE integration_id IN
             (SELECT id FROM integrations WHERE kind = 'romm_api')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM integrations WHERE kind = 'romm_api'")
        .execute(pool)
        .await
        .unwrap();
}

impl Harness {
    async fn send(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
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
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn make(&mut self, token: &str, settings: Value) -> (StatusCode, Value) {
        let (status, made) = self
            .send(
                Method::POST,
                "/api/v1/integrations",
                Some(json!({
                    "kind": "romm_api",
                    "name": format!("RomM API {}", uuid::Uuid::now_v7().simple()),
                    "enabled": true,
                    "settings": settings,
                    "secrets": { "token": token },
                })),
            )
            .await;
        if status == StatusCode::CREATED {
            self.made.push(made["id"].as_str().unwrap().to_owned());
        }
        (status, made)
    }

    fn settings(&self) -> Value {
        json!({ "base_url": format!("http://{}/", self.romm) })
    }

    async fn try_it(&self, id: &str) -> Value {
        let (status, tried) = self
            .send(
                Method::POST,
                &format!("/api/v1/integrations/{id}/test"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{tried}");
        tried
    }

    async fn done(self) {
        for id in &self.made {
            let (status, body) = self
                .send(Method::DELETE, &format!("/api/v1/integrations/{id}"), None)
                .await;
            assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
        }
    }
}

fn verdicts(tried: &Value) -> Vec<(String, bool)> {
    tried["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|check| {
            (
                check["name"].as_str().unwrap().to_owned(),
                check["ok"].as_bool().unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_romm_with_a_scoped_token_passes_every_check() {
    let mut harness = harness().await;
    let settings = harness.settings();
    let (status, made) = harness.make(TOKEN, settings).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(made["kind"], "romm_api");
    assert_eq!(made["secrets"], json!([{ "field": "token", "set": true }]));
    assert!(!made.to_string().contains(TOKEN), "{made}");
    let id = made["id"].as_str().unwrap().to_owned();

    let tried = harness.try_it(&id).await;
    assert_eq!(tried["ok"], true, "{tried}");
    assert_eq!(tried["api_version"], "5.4.1");
    assert_eq!(
        verdicts(&tried),
        [
            ("Client tokens".to_owned(), true),
            ("Finding exported games".to_owned(), true),
            ("Asking for scans".to_owned(), true),
            ("Known platforms".to_owned(), true),
            ("Export marker".to_owned(), true),
        ]
    );
    assert!(!tried.to_string().contains(TOKEN), "{tried}");
    // Every signed request carried the token as a bearer token.
    let seen = harness.fake.lock().unwrap().seen_tokens.clone();
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert!(seen.iter().all(|sent| *sent == format!("Bearer {TOKEN}")));

    let (_, listed) = harness
        .send(Method::GET, &format!("/api/v1/integrations/{id}"), None)
        .await;
    assert_eq!(listed["status"], "ok", "{listed}");
    assert_eq!(listed["client_version"], "5.4.1", "{listed}");

    // There is one export, so one RomM API.
    let settings = harness.settings();
    let (status, problem) = harness.make(TOKEN, settings).await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("RomM API integration already"),
        "{problem}"
    );
    harness.done().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_token_that_does_not_open_is_refused_and_not_answered() {
    let mut harness = harness().await;
    let settings = harness.settings();
    let (_, made) = harness.make("rmm_a-wrong-token", settings).await;
    let tried = harness.try_it(made["id"].as_str().unwrap()).await;
    assert_eq!(tried["ok"], false, "{tried}");
    assert_eq!(tried["error_code"], "INTEGRATION_AUTH_FAILED");
    assert!(
        tried["detail"]
            .as_str()
            .unwrap()
            .contains("wrong, expired or revoked"),
        "{tried}"
    );
    assert!(!tried.to_string().contains("a-wrong-token"), "{tried}");
    let failing: String = sqlx::query_scalar("SELECT status FROM integrations WHERE id = $1")
        .bind(uuid::Uuid::parse_str(made["id"].as_str().unwrap()).unwrap())
        .fetch_one(&harness.pool)
        .await
        .unwrap();
    assert_eq!(failing, "failing");
    harness.done().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn what_a_romm_cannot_do_is_said_check_by_check() {
    let mut harness = harness().await;
    let settings = harness.settings();
    let (_, made) = harness.make(TOKEN, settings).await;
    let id = made["id"].as_str().unwrap().to_owned();

    // A RomM that counts the export's marker as a game file is told how
    // to stop, and still found.
    harness.fake.lock().unwrap().config["EXCLUDED_MULTI_PARTS_FILES"] = json!([".DS_Store"]);
    let tried = harness.try_it(&id).await;
    assert_eq!(tried["ok"], true, "{tried}");
    assert_eq!(tried["checks"][4]["ok"], false, "{tried}");
    assert!(
        tried["checks"][4]["detail"]
            .as_str()
            .unwrap()
            .contains("exclude.roms.multi_file.parts.names"),
        "{tried}"
    );

    // A token without tasks.run still finds games, and says scans cannot
    // be asked for.
    harness.fake.lock().unwrap().denied = vec!["tasks.run"];
    let tried = harness.try_it(&id).await;
    assert_eq!(tried["ok"], true, "{tried}");
    assert_eq!(tried["checks"][2]["ok"], false, "{tried}");
    assert!(
        tried["checks"][2]["detail"]
            .as_str()
            .unwrap()
            .contains("tasks.run")
    );

    // RomM 5.0 to 5.3: no scan a token may ask for.
    harness.fake.lock().unwrap().denied.clear();
    harness.fake.lock().unwrap().document["paths"]
        .as_object_mut()
        .unwrap()
        .remove("/api/tasks/scan");
    let tried = harness.try_it(&id).await;
    assert_eq!(tried["ok"], true, "{tried}");
    assert!(
        tried["checks"][2]["detail"]
            .as_str()
            .unwrap()
            .contains("5.4 or later"),
        "{tried}"
    );

    // Without roms.read, exported games cannot be found: misconfigured.
    harness.fake.lock().unwrap().denied = vec!["roms.read"];
    let tried = harness.try_it(&id).await;
    assert_eq!(tried["ok"], false, "{tried}");
    assert_eq!(tried["error_code"], "INTEGRATION_MISCONFIGURED");
    assert!(tried["detail"].as_str().unwrap().contains("roms.read"));

    // Before 5.0 there are no client tokens: unsupported.
    harness.fake.lock().unwrap().denied.clear();
    harness.fake.lock().unwrap().document["paths"]
        .as_object_mut()
        .unwrap()
        .remove("/api/client-tokens");
    let tried = harness.try_it(&id).await;
    assert_eq!(tried["ok"], false, "{tried}");
    assert_eq!(tried["error_code"], "INTEGRATION_UNSUPPORTED");
    assert!(tried["detail"].as_str().unwrap().contains("5.0 or later"));
    harness.done().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn settings_and_secrets_that_will_not_do_are_refused() {
    let mut harness = harness().await;
    for (settings, says) in [
        (json!({ "base_url": "http://169.254.169.254/" }), "base_url"),
        (
            json!({ "base_url": "http://192.168.1.13:8080/", "web_url": "file:///etc" }),
            "web_url",
        ),
        (
            json!({ "base_url": "http://192.168.1.13:8080/", "username": "admin" }),
            "do not fit",
        ),
    ] {
        let (status, problem) = harness.make(TOKEN, settings.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{settings}");
        assert!(
            problem["detail"].as_str().unwrap().contains(says),
            "{settings}: {problem}"
        );
    }
    let (status, problem) = harness
        .send(
            Method::POST,
            "/api/v1/integrations",
            Some(json!({
                "kind": "romm_api",
                "name": "RomM API with a password",
                "settings": { "base_url": "http://192.168.1.13:8080/" },
                "secrets": { "password": "hunter2" },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert!(problem["detail"].as_str().unwrap().contains("takes token"));
    // Made without a token, it cannot be tried.
    let (status, made) = harness
        .send(
            Method::POST,
            "/api/v1/integrations",
            Some(json!({
                "kind": "romm_api",
                "name": format!("RomM API {}", uuid::Uuid::now_v7().simple()),
                "enabled": true,
                "settings": harness.settings(),
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let id = made["id"].as_str().unwrap().to_owned();
    harness.made.push(id.clone());
    let (status, problem) = harness
        .send(
            Method::POST,
            &format!("/api/v1/integrations/{id}/test"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(problem["code"], "INTEGRATION_MISCONFIGURED");
    assert!(problem["detail"].as_str().unwrap().contains("client token"));
    harness.done().await;
}

/// Against a real RomM, when `TANGIBLE_TEST_ROMM_URL` and
/// `TANGIBLE_TEST_ROMM_TOKEN` name one and a token with the three scopes,
/// as `tangible_integrations`' live test does; skipped otherwise. Its
/// `config.yml` must leave out the export's marker, as the deploy guide
/// says, for every check to pass.
#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_real_romm_passes_every_check() {
    let (Ok(url), Ok(token)) = (
        std::env::var("TANGIBLE_TEST_ROMM_URL"),
        std::env::var("TANGIBLE_TEST_ROMM_TOKEN"),
    ) else {
        eprintln!("skipping: set TANGIBLE_TEST_ROMM_URL and TANGIBLE_TEST_ROMM_TOKEN");
        return;
    };
    let mut harness = harness().await;
    let (status, made) = harness.make(&token, json!({ "base_url": url })).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let tried = harness.try_it(made["id"].as_str().unwrap()).await;
    assert_eq!(tried["ok"], true, "{tried}");
    assert!(
        tried["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|check| check["ok"] == true),
        "{tried}"
    );
    assert!(!tried.to_string().contains(&token));
    harness.done().await;
}
