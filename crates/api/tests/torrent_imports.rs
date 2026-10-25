// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Torrents handed to qBittorrent and imported when they finish
//! (docs/plans/acquisition.md A4), through the real router and runner.
//!
//! qBittorrent is a fake on 127.0.0.1 speaking the part of WebUI API v2
//! Tangible uses, as recorded from qBittorrent 5.1.2 in
//! fixtures/tool-output/qbittorrent: `Ok.` and `Fails.`, an `SID` cookie,
//! 403 for a session it does not know, torrents/info as JSON. Its download
//! folder, `/downloads/tangible` as it reports it, is a temporary directory
//! mounted here as the watched root `downloads`.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use sqlx::PgPool;
use tangible_api::secrets::SecretKey;
use tangible_api::{
    ApiState, ImportContext, ImportPipeline, ImportRunner, ImportRunnerSettings, router,
};
use tangible_domain::Role;
use tangible_integrations::outbound::{OutboundClient, OutboundPolicy};
use tangible_integrations::torrent::infohash_of_torrent;
use tangible_storage::{FilesystemStore, ManifestStore, StagingManager, WatchRoots};
use tempfile::TempDir;
use tower::ServiceExt as _;

mod support;

const PASSWORD: &str = "the-right-password";
const SESSION: &str = "fake-session-id";

/// What the fake qBittorrent holds.
#[derive(Default)]
struct Fake {
    /// Torrents by hash, as torrents/info answers them.
    torrents: HashMap<String, Value>,
    /// The hash of the torrent the next add is for.
    adding: Option<String>,
    /// The content types and bodies of the adds it took.
    adds: Vec<(String, String)>,
}

type Shared = Arc<Mutex<Fake>>;

fn signed_in(headers: &HeaderMap) -> bool {
    headers
        .get("cookie")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|cookie| cookie.contains(&format!("SID={SESSION}")))
}

async fn login(axum::Form(fields): axum::Form<HashMap<String, String>>) -> impl IntoResponse {
    if fields.get("password").map(String::as_str) == Some(PASSWORD) {
        (
            [(
                "set-cookie",
                format!("SID={SESSION}; HttpOnly; SameSite=Strict; path=/"),
            )],
            "Ok.",
        )
            .into_response()
    } else {
        "Fails.".into_response()
    }
}

async fn version(headers: HeaderMap) -> impl IntoResponse {
    if !signed_in(&headers) {
        return (StatusCode::FORBIDDEN, "Forbidden").into_response();
    }
    "2.11.4".into_response()
}

async fn add(
    State(fake): State<Shared>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    // A .torrent part is binary; kept as text only for the assertions.
    let body = String::from_utf8_lossy(&body).into_owned();
    if !signed_in(&headers) {
        return (StatusCode::FORBIDDEN, "Forbidden").into_response();
    }
    let mut fake = fake.lock().unwrap();
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    fake.adds.push((content_type, body));
    let Some(hash) = fake.adding.clone() else {
        return "Fails.".into_response();
    };
    if fake.torrents.contains_key(&hash) {
        return "Fails.".into_response();
    }
    fake.torrents
        .insert(hash.clone(), torrent(&hash, "metaDL", 0.0, ""));
    "Ok.".into_response()
}

async fn info(
    State(fake): State<Shared>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    if !signed_in(&headers) {
        return (StatusCode::FORBIDDEN, "Forbidden").into_response();
    }
    let fake = fake.lock().unwrap();
    let wanted = query.get("hashes").cloned().unwrap_or_default();
    let list: Vec<Value> = fake
        .torrents
        .iter()
        .filter(|(hash, _)| wanted.is_empty() || **hash == wanted)
        .map(|(_, torrent)| torrent.clone())
        .collect();
    axum::Json(list).into_response()
}

/// A torrent as qBittorrent 5.1.2 lists it, the fields Tangible reads.
fn torrent(hash: &str, state: &str, progress: f64, content_path: &str) -> Value {
    let size = 300_000_i64;
    json!({
        "hash": hash,
        "state": state,
        "progress": progress,
        "content_path": content_path,
        "size": if content_path.is_empty() { 0 } else { size },
        "amount_left": if content_path.is_empty() { 0 } else {
            // Truncation is fine for a test's progress figure.
            #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
            { size - (size as f64 * progress) as i64 }
        },
        "has_metadata": !content_path.is_empty(),
        "category": "tangible",
        "name": "probe-disc.iso",
    })
}

async fn fake_qbittorrent() -> (SocketAddr, Shared) {
    let fake: Shared = Arc::default();
    let app = axum::Router::new()
        .route("/api/v2/auth/login", axum::routing::post(login))
        .route("/api/v2/app/webapiVersion", axum::routing::get(version))
        .route("/api/v2/torrents/add", axum::routing::post(add))
        .route("/api/v2/torrents/info", axum::routing::get(info))
        .with_state(Arc::clone(&fake));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (address, fake)
}

static QUEUE: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

struct Harness {
    pool: PgPool,
    state: ApiState,
    runner: ImportRunner,
    downloads: PathBuf,
    fake: Shared,
    qbittorrent: SocketAddr,
    admin: support::Session,
    operator: support::Session,
    _dir: TempDir,
    _queue: tokio::sync::MutexGuard<'static, ()>,
}

async fn harness() -> Harness {
    harness_with(None).await
}

/// A harness whose watched root `downloads` is `downloads`, or a temporary
/// directory.
async fn harness_with(downloads: Option<PathBuf>) -> Harness {
    let queue = QUEUE
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let database = support::database().await;
    let pool = database.pool().clone();
    let dir = TempDir::new().unwrap();
    let objects = FilesystemStore::open(dir.path().join("library"))
        .await
        .unwrap();
    let manifests = ManifestStore::open(objects.clone()).await.unwrap();
    let staging = StagingManager::open(dir.path().join("staging"))
        .await
        .unwrap();
    let pipeline = ImportPipeline::new(staging, objects, manifests.clone());
    let downloads = downloads.unwrap_or_else(|| {
        let downloads = dir.path().join("downloads");
        std::fs::create_dir(&downloads).unwrap();
        downloads
    });
    let mut roots = BTreeMap::new();
    roots.insert("downloads".to_owned(), downloads.clone());
    let roots = WatchRoots::new(roots);
    // The fake listens on loopback, which only an allowed block lets in.
    let client = OutboundClient::new(
        OutboundPolicy::for_integrations(vec!["127.0.0.1/32".parse().unwrap()], None),
        "tangible-test",
    )
    .unwrap();
    let key = Arc::new(SecretKey::from_base64(&SecretKey::generate().unwrap()).unwrap());
    let state = ApiState::with_manifests(database.clone(), manifests)
        .with_imports(ImportContext {
            pipeline: pipeline.clone(),
            roots: roots.clone(),
            max_upload_bytes: 1 << 30,
            outbound: None,
            upload_part_bytes: 4096,
        })
        .with_secret_key(Arc::clone(&key))
        .with_integration_client(client.clone());
    let runner = ImportRunner::new(
        database,
        pipeline,
        ImportRunnerSettings {
            owner: format!("test-{}", uuid::Uuid::now_v7()),
            roots,
            integrations: Some(client),
            secret_key: Some(key),
            torrent_poll: Duration::from_secs(1),
            retry_backoff: Duration::ZERO,
            ..ImportRunnerSettings::default()
        },
    );
    // Anything an earlier run left claimable belongs to directories and
    // fakes that are gone.
    sqlx::query(
        "UPDATE import_jobs SET state = 'canceled', lease_owner = NULL, lease_expires_at = NULL
         WHERE state IN ('requested', 'acquiring', 'staged', 'hashing', 'inspecting',
                         'registering', 'failed_retryable')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (qbittorrent, fake) = fake_qbittorrent().await;
    Harness {
        admin: support::administrator(&pool).await,
        operator: support::sign_in(&pool, Role::Operator).await,
        pool,
        state,
        runner,
        downloads,
        fake,
        qbittorrent,
        _dir: dir,
        _queue: queue,
    }
}

impl Harness {
    async fn send(
        &self,
        session: &support::Session,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(path);
        if body.is_some() {
            request = request.header("content-type", "application/json");
        }
        let mut request = request
            .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
            .unwrap();
        session.apply(&mut request);
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

    /// An integration pointing at the fake, with `password`.
    async fn integration(&self, password: &str) -> String {
        let (status, made) = self
            .send(
                &self.admin,
                Method::POST,
                "/api/v1/integrations",
                Some(json!({
                    "kind": "qbittorrent",
                    "name": format!("fake-{}", uuid::Uuid::now_v7().simple()),
                    "settings": {
                        "base_url": format!("http://{}/", self.qbittorrent),
                        "username": "admin",
                        "client_path": "/downloads/tangible",
                        "local_root": "downloads",
                    },
                    "secrets": { "password": password },
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
        made["id"].as_str().unwrap().to_owned()
    }

    async fn import(&self, id: &str) -> Value {
        self.send(
            &self.admin,
            Method::GET,
            &format!("/api/v1/imports/{id}"),
            None,
        )
        .await
        .1
    }

    /// Run the queue until it has nothing claimable.
    async fn drain(&self) {
        for _ in 0..20 {
            if !self.runner.step().await.unwrap() {
                return;
            }
        }
    }

    fn set_torrent(&self, hash: &str, torrent: Value) {
        self.fake
            .lock()
            .unwrap()
            .torrents
            .insert(hash.to_owned(), torrent);
    }

    async fn cancel(&self, import_id: &str) {
        let _ = self
            .send(
                &self.admin,
                Method::POST,
                &format!("/api/v1/imports/{import_id}/cancel"),
                None,
            )
            .await;
    }
}

fn fixture_torrent() -> (Vec<u8>, String) {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/tool-output/qbittorrent/single-file.torrent"
    ))
    .unwrap();
    let hash = infohash_of_torrent(&bytes).unwrap().to_string();
    (bytes, hash)
}

fn base64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn iso_image() -> Vec<u8> {
    const SECTOR: usize = 2048;
    let mut bytes = vec![0_u8; 16 * SECTOR];
    let mut pvd = vec![0_u8; SECTOR];
    pvd[0] = 1;
    pvd[1..6].copy_from_slice(b"CD001");
    pvd[6] = 1;
    for slot in &mut pvd[40..72] {
        *slot = b' ';
    }
    pvd[40..44].copy_from_slice(b"TEST");
    pvd[80..84].copy_from_slice(&32_u32.to_le_bytes());
    pvd[84..88].copy_from_slice(&32_u32.to_be_bytes());
    pvd[128..130].copy_from_slice(&2048_u16.to_le_bytes());
    pvd[130..132].copy_from_slice(&2048_u16.to_be_bytes());
    bytes.extend_from_slice(&pvd);
    let mut terminator = vec![0_u8; SECTOR];
    terminator[0] = 255;
    terminator[1..6].copy_from_slice(b"CD001");
    terminator[6] = 1;
    bytes.extend_from_slice(&terminator);
    bytes.resize(32 * SECTOR, 0);
    bytes
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
#[allow(clippy::too_many_lines)]
async fn a_torrent_is_handed_over_followed_and_imported_when_it_finishes() {
    let harness = harness().await;
    let integration = harness.integration(PASSWORD).await;
    let (file, hash) = fixture_torrent();
    harness.fake.lock().unwrap().adding = Some(hash.clone());

    // Operators see the client among the import sources.
    let (_, sources) = harness
        .send(
            &harness.operator,
            Method::GET,
            "/api/v1/import-sources",
            None,
        )
        .await;
    assert!(
        sources["download_clients"]
            .as_array()
            .unwrap()
            .iter()
            .any(|client| client["id"] == integration.as_str() && client["kind"] == "qbittorrent"),
        "{sources}"
    );

    let jobs = format!("/api/v1/integrations/{integration}/jobs");
    let (status, job) = harness
        .send(
            &harness.operator,
            Method::POST,
            &jobs,
            Some(json!({ "torrent": base64(&file) })),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    assert_eq!(job["outcome"], "created");
    assert_eq!(job["infohash"], hash.as_str());
    let import_id = job["import_id"].as_str().unwrap().to_owned();
    {
        let fake = harness.fake.lock().unwrap();
        let (content_type, body) = &fake.adds[0];
        assert!(
            content_type.starts_with("multipart/form-data"),
            "{content_type}"
        );
        assert!(body.contains("name=\"category\"\r\n\r\ntangible"));
        assert!(body.contains("name=\"autoTMM\"\r\n\r\ntrue"));
    }

    // Halfway: held in acquiring, with its progress, and asked again later.
    harness.set_torrent(
        &hash,
        torrent(&hash, "downloading", 0.5, "/downloads/tangible/disc.iso"),
    );
    harness.drain().await;
    let held = harness.import(&import_id).await;
    assert_eq!(held["state"], "acquiring", "{held}");
    assert_eq!(held["bytes_received"], 150_000);
    assert_eq!(held["bytes_expected"], 300_000);
    assert!(held["error_code"].is_null(), "{held}");
    assert!(
        !harness.runner.step().await.unwrap(),
        "held, not claimable at once"
    );

    // Submitting again answers the import already under way.
    let (status, again) = harness
        .send(
            &harness.operator,
            Method::POST,
            &jobs,
            Some(json!({ "torrent": base64(&file) })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["outcome"], "already_active");
    assert_eq!(again["import_id"], import_id.as_str());

    // Finished: copied in from the watched root and imported.
    std::fs::write(harness.downloads.join("disc.iso"), iso_image()).unwrap();
    harness.set_torrent(
        &hash,
        torrent(&hash, "stalledUP", 1.0, "/downloads/tangible/disc.iso"),
    );
    tokio::time::sleep(Duration::from_millis(1100)).await;
    harness.drain().await;
    let done = harness.import(&import_id).await;
    assert_eq!(done["state"], "complete", "{done}");
    assert_eq!(done["source_reference"], format!("qbittorrent:{hash}"));
    let artifact = done["artifact_id"].as_str().unwrap().to_owned();
    assert!(
        harness.downloads.join("disc.iso").exists(),
        "the client's copy is never touched"
    );

    let (status, imported) = harness
        .send(
            &harness.operator,
            Method::POST,
            &jobs,
            Some(json!({ "magnet": format!("magnet:?xt=urn:btih:{hash}") })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{imported}");
    assert_eq!(imported["outcome"], "already_imported");
    assert_eq!(imported["artifact_id"], artifact.as_str());

    let (_, view) = harness
        .send(
            &harness.admin,
            Method::GET,
            &format!("/api/v1/integrations/{integration}"),
            None,
        )
        .await;
    assert_eq!(view["status"], "ok", "{view}");
    let state: Option<String> =
        sqlx::query_scalar("SELECT client_state FROM integration_jobs WHERE import_job_id = $1")
            .bind(uuid::Uuid::parse_str(&import_id).unwrap())
            .fetch_one(&harness.pool)
            .await
            .unwrap();
    assert_eq!(state.as_deref(), Some("stalledUP"));
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_torrent_outside_the_folder_or_failed_in_the_client_ends_the_import() {
    let harness = harness().await;
    let integration = harness.integration(PASSWORD).await;
    let jobs = format!("/api/v1/integrations/{integration}/jobs");
    let mut imports = Vec::new();
    for (hash, torrent, code) in [
        (
            "1".repeat(40),
            torrent(&"1".repeat(40), "stalledUP", 1.0, "/elsewhere/disc.iso"),
            "INTEGRATION_PATH_OUTSIDE_ROOT",
        ),
        (
            "2".repeat(40),
            torrent(&"2".repeat(40), "stalledUP", 1.0, "/downloads/tangible"),
            "INTEGRATION_PATH_OUTSIDE_ROOT",
        ),
        (
            "3".repeat(40),
            torrent(&"3".repeat(40), "error", 0.4, "/downloads/tangible/x.iso"),
            "INTEGRATION_JOB_FAILED",
        ),
    ] {
        harness.fake.lock().unwrap().adding = Some(hash.clone());
        let (status, job) = harness
            .send(
                &harness.operator,
                Method::POST,
                &jobs,
                Some(json!({ "magnet": format!("magnet:?xt=urn:btih:{hash}") })),
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{job}");
        harness.set_torrent(&hash, torrent);
        imports.push((job["import_id"].as_str().unwrap().to_owned(), code));
    }
    {
        let fake = harness.fake.lock().unwrap();
        assert!(
            fake.adds[0]
                .0
                .starts_with("application/x-www-form-urlencoded")
        );
        assert!(
            fake.adds[0]
                .1
                .starts_with("urls=magnet%3A%3Fxt%3Durn%3Abtih%3A1111")
        );
    }
    harness.drain().await;
    for (import_id, code) in imports {
        let ended = harness.import(&import_id).await;
        assert_eq!(ended["state"], "failed_terminal", "{ended}");
        assert_eq!(ended["error_code"], code, "{ended}");
        assert!(
            !ended.to_string().contains("elsewhere"),
            "a path the client reported is not repeated: {ended}"
        );
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_client_refusing_to_sign_in_or_turned_off_holds_the_import_and_says_why() {
    let harness = harness().await;
    let integration = harness.integration(PASSWORD).await;
    let hash = "4".repeat(40);
    harness.fake.lock().unwrap().adding = Some(hash.clone());
    let (status, job) = harness
        .send(
            &harness.operator,
            Method::POST,
            &format!("/api/v1/integrations/{integration}/jobs"),
            Some(json!({ "magnet": format!("magnet:?xt=urn:btih:{hash}") })),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let import_id = job["import_id"].as_str().unwrap().to_owned();

    // The password changes in the client; the import waits, saying why.
    let path = format!("/api/v1/integrations/{integration}");
    let (status, _) = harness
        .send(
            &harness.admin,
            Method::PATCH,
            &path,
            Some(json!({ "secrets": { "password": "a-stale-password" } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    harness.drain().await;
    let held = harness.import(&import_id).await;
    assert_eq!(held["state"], "acquiring", "{held}");
    assert_eq!(held["error_code"], "INTEGRATION_AUTH_FAILED", "{held}");
    assert!(!held.to_string().contains("a-stale-password"));
    let (_, view) = harness.send(&harness.admin, Method::GET, &path, None).await;
    assert_eq!(view["status"], "failing", "{view}");
    assert_eq!(view["last_error_code"], "INTEGRATION_AUTH_FAILED");

    // Turned off: still held, not failed.
    let (status, _) = harness
        .send(
            &harness.admin,
            Method::PATCH,
            &path,
            Some(json!({ "enabled": false })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    harness.drain().await;
    let held = harness.import(&import_id).await;
    assert_eq!(held["state"], "acquiring", "{held}");
    assert_eq!(held["error_code"], "INTEGRATION_OFF", "{held}");
    harness.cancel(&import_id).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn submissions_that_will_not_do_are_refused() {
    let harness = harness().await;
    let integration = harness.integration(PASSWORD).await;
    let jobs = format!("/api/v1/integrations/{integration}/jobs");
    for (body, says) in [
        (json!({}), "either"),
        (
            json!({ "magnet": "magnet:?xt=urn:btih:abc", "torrent": "AAAA" }),
            "either",
        ),
        (json!({ "torrent": "not base64!" }), "base64"),
        (json!({ "torrent": base64(b"not bencode") }), ".torrent"),
        (
            json!({ "magnet": "http://example.com/x.torrent" }),
            "magnet",
        ),
    ] {
        let (status, problem) = harness
            .send(&harness.operator, Method::POST, &jobs, Some(body.clone()))
            .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body}: {problem}"
        );
        assert!(
            problem["detail"].as_str().unwrap().contains(says),
            "{body}: {problem}"
        );
    }
    // The client declines a torrent it then does not have.
    harness.fake.lock().unwrap().adding = None;
    let (status, problem) = harness
        .send(
            &harness.operator,
            Method::POST,
            &jobs,
            Some(json!({ "magnet": format!("magnet:?xt=urn:btih:{}", "5".repeat(40)) })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert!(problem["detail"].as_str().unwrap().contains("declined"));

    // It has the torrent under another category.
    let other = "6".repeat(40);
    let mut held = torrent(&other, "downloading", 0.1, "/downloads/movies/x");
    held["category"] = json!("movies");
    harness.set_torrent(&other, held);
    let (status, problem) = harness
        .send(
            &harness.operator,
            Method::POST,
            &jobs,
            Some(json!({ "magnet": format!("magnet:?xt=urn:btih:{other}") })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("another category")
    );

    // A viewer may not; a wrong password is the client's refusal.
    let viewer = support::sign_in(&harness.pool, Role::Viewer).await;
    let (status, _) = harness
        .send(&viewer, Method::POST, &jobs, Some(json!({ "magnet": "x" })))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let wrong = harness.integration("not-the-password").await;
    let (status, problem) = harness
        .send(
            &harness.operator,
            Method::POST,
            &format!("/api/v1/integrations/{wrong}/jobs"),
            Some(json!({ "magnet": format!("magnet:?xt=urn:btih:{}", "7".repeat(40)) })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{problem}");
    assert_eq!(problem["code"], "INTEGRATION_AUTH_FAILED");
    assert!(!problem.to_string().contains("not-the-password"));

    // Turned off.
    let (status, _) = harness
        .send(
            &harness.admin,
            Method::PATCH,
            &format!("/api/v1/integrations/{integration}"),
            Some(json!({ "enabled": false })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, problem) = harness
        .send(
            &harness.operator,
            Method::POST,
            &jobs,
            Some(json!({ "magnet": format!("magnet:?xt=urn:btih:{}", "8".repeat(40)) })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(problem["code"], "INTEGRATION_OFF");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn trying_an_integration_signs_in_and_records_the_version() {
    let harness = harness().await;
    let good = harness.integration(PASSWORD).await;
    let (status, tried) = harness
        .send(
            &harness.admin,
            Method::POST,
            &format!("/api/v1/integrations/{good}/test"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{tried}");
    assert_eq!(tried["ok"], true);
    assert_eq!(tried["api_version"], "2.11.4");
    let (_, view) = harness
        .send(
            &harness.admin,
            Method::GET,
            &format!("/api/v1/integrations/{good}"),
            None,
        )
        .await;
    assert_eq!(view["client_version"], "2.11.4");
    assert_eq!(view["status"], "ok");

    let bad = harness.integration("not-the-password").await;
    let (status, tried) = harness
        .send(
            &harness.admin,
            Method::POST,
            &format!("/api/v1/integrations/{bad}/test"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{tried}");
    assert_eq!(tried["ok"], false);
    assert_eq!(tried["error_code"], "INTEGRATION_AUTH_FAILED");

    let (status, _) = harness
        .send(
            &harness.operator,
            Method::POST,
            &format!("/api/v1/integrations/{good}/test"),
            None,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "trying is an administrator's"
    );
}

/// Against a real qBittorrent, when one is named: the fixture torrent whose
/// file is already in the client's category folder, so it finishes at once.
///
/// `TANGIBLE_TEST_QBITTORRENT_URL` and `_PASSWORD` as in the integrations
/// crate's live test, plus `TANGIBLE_TEST_QBITTORRENT_DOWNLOADS`, where the
/// client's `/downloads/tangible` is on this machine. Skips without them.
#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_real_qbittorrent_torrent_is_imported() {
    let (Ok(url), Ok(password), Ok(downloads)) = (
        std::env::var("TANGIBLE_TEST_QBITTORRENT_URL"),
        std::env::var("TANGIBLE_TEST_QBITTORRENT_PASSWORD"),
        std::env::var("TANGIBLE_TEST_QBITTORRENT_DOWNLOADS"),
    ) else {
        eprintln!("skipping: set TANGIBLE_TEST_QBITTORRENT_URL, _PASSWORD and _DOWNLOADS");
        return;
    };
    // The real client's folder is the watched root here.
    let harness = harness_with(Some(PathBuf::from(&downloads))).await;
    let (status, made) = harness
        .send(
            &harness.admin,
            Method::POST,
            "/api/v1/integrations",
            Some(json!({
                "kind": "qbittorrent",
                "name": format!("real-{}", uuid::Uuid::now_v7().simple()),
                "settings": {
                    "base_url": url,
                    "username": std::env::var("TANGIBLE_TEST_QBITTORRENT_USER")
                        .unwrap_or_else(|_| "admin".to_owned()),
                    "client_path": "/downloads/tangible",
                    "local_root": "downloads",
                },
                "secrets": { "password": password },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let integration = made["id"].as_str().unwrap();
    let (file, hash) = fixture_torrent();
    let (status, job) = harness
        .send(
            &harness.operator,
            Method::POST,
            &format!("/api/v1/integrations/{integration}/jobs"),
            Some(json!({ "torrent": base64(&file) })),
        )
        .await;
    assert!(status.is_success(), "{job}");
    assert_eq!(job["infohash"], hash.as_str());
    let import_id = job["import_id"].as_str().unwrap().to_owned();
    let mut done = Value::Null;
    for _ in 0..30 {
        harness.drain().await;
        done = harness.import(&import_id).await;
        if done["state"] != "acquiring" && done["state"] != "requested" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1100)).await;
    }
    assert_eq!(done["state"], "complete", "{done}");
    assert_eq!(done["source_reference"], format!("qbittorrent:{hash}"));
}
