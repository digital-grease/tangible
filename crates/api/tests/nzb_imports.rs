// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! NZBs handed to SABnzbd or NZBGet and imported when they finish
//! (docs/plans/acquisition.md A5), through the real router and runner.
//!
//! NZBGet, further down, is a fake JSON-RPC endpoint speaking what was
//! recorded from NZBGet 25.3 in fixtures/tool-output/nzbget: basic
//! authentication, 401 for a wrong password, an NZBID for every append,
//! listgroups then history.
//!
//! SABnzbd is a fake on 127.0.0.1 speaking the part of its API Tangible
//! uses, as recorded from SABnzbd 4.5.3 in fixtures/tool-output/sabnzbd:
//! every mode a POST with the key in the body, 403 for a wrong key, the
//! category quietly replaced when unknown (so Tangible checks first), queue
//! then history. Its finished-download folder, `/downloads/complete/tangible`
//! as it reports it, is a temporary directory mounted here as the watched
//! root `downloads`.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use sqlx::PgPool;
use tangible_api::secrets::SecretKey;
use tangible_api::{
    ApiState, ImportContext, ImportPipeline, ImportRunner, ImportRunnerSettings, router,
};
use tangible_domain::Role;
use tangible_integrations::outbound::{OutboundClient, OutboundPolicy};
use tangible_storage::{FilesystemStore, ManifestStore, StagingManager, WatchRoots};
use tempfile::TempDir;
use tower::ServiceExt as _;

mod support;

const API_KEY: &str = "the-right-api-key";

/// What the fake SABnzbd holds.
#[derive(Default)]
struct Fake {
    /// Its categories.
    categories: Vec<String>,
    /// Jobs in the queue, by id, as queue slots.
    queue: HashMap<String, Value>,
    /// Jobs in history, by id, as history slots.
    history: HashMap<String, Value>,
    /// Whether the next add is declined, as for a file it cannot read.
    decline: bool,
    /// The bodies of the adds it took.
    adds: Vec<String>,
    /// Every request URI, to show the key never travels in one.
    uris: Vec<String>,
}

type Shared = Arc<Mutex<Fake>>;

/// A field of a urlencoded or multipart body.
fn field(body: &str, content_type: &str, name: &str) -> Option<String> {
    if content_type.starts_with("multipart/form-data") {
        let marker = format!("name=\"{name}\"\r\n\r\n");
        let start = body.find(&marker)? + marker.len();
        let end = body[start..].find("\r\n")? + start;
        return Some(body[start..end].to_owned());
    }
    // Enough of urlencoded for the fields Tangible sends: the values here
    // have no characters that need decoding beyond what is checked.
    body.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| value.replace('+', " ").replace("%2F", "/"))
    })
}

async fn api(
    State(fake): State<Shared>,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let body = String::from_utf8_lossy(&body).into_owned();
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let mut fake = fake.lock().unwrap();
    fake.uris.push(uri.to_string());
    let mode = field(&body, &content_type, "mode").unwrap_or_default();
    if mode == "version" {
        return axum::Json(json!({ "version": "4.5.3" })).into_response();
    }
    match field(&body, &content_type, "apikey").as_deref() {
        Some(API_KEY) => {}
        Some(_) => return (StatusCode::FORBIDDEN, "API Key Incorrect").into_response(),
        None => return (StatusCode::FORBIDDEN, "API Key Required").into_response(),
    }
    let wanted = field(&body, &content_type, "nzo_ids").unwrap_or_default();
    match mode.as_str() {
        "get_cats" => {
            let mut categories = vec!["*".to_owned()];
            categories.extend(fake.categories.clone());
            axum::Json(json!({ "categories": categories })).into_response()
        }
        "addfile" => {
            fake.adds.push(body.clone());
            if fake.decline {
                return axum::Json(json!({ "status": false, "nzo_ids": [] })).into_response();
            }
            let id = format!("SABnzbd_nzo_test{}", fake.adds.len());
            fake.queue.insert(
                id.clone(),
                json!({ "nzo_id": id, "status": "Queued", "mb": "0.29", "mbleft": "0.29" }),
            );
            axum::Json(json!({ "status": true, "nzo_ids": [id] })).into_response()
        }
        "queue" => {
            let slots: Vec<Value> = fake
                .queue
                .iter()
                .filter(|(id, _)| wanted.is_empty() || **id == wanted)
                .map(|(_, slot)| slot.clone())
                .collect();
            axum::Json(json!({ "queue": { "slots": slots } })).into_response()
        }
        "history" => {
            let slots: Vec<Value> = fake
                .history
                .iter()
                .filter(|(id, _)| wanted.is_empty() || **id == wanted)
                .map(|(_, slot)| slot.clone())
                .collect();
            axum::Json(json!({ "history": { "slots": slots } })).into_response()
        }
        _ => (StatusCode::OK, "error: not implemented").into_response(),
    }
}

async fn fake_sabnzbd() -> (SocketAddr, Shared) {
    let fake: Shared = Arc::new(Mutex::new(Fake {
        categories: vec!["tangible".to_owned()],
        ..Fake::default()
    }));
    let app = axum::Router::new()
        .route("/api", axum::routing::post(api))
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
    sabnzbd: SocketAddr,
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
    sqlx::query(
        "UPDATE import_jobs SET state = 'canceled', lease_owner = NULL, lease_expires_at = NULL
         WHERE state IN ('requested', 'acquiring', 'staged', 'hashing', 'inspecting',
                         'registering', 'failed_retryable')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (sabnzbd, fake) = fake_sabnzbd().await;
    Harness {
        admin: support::administrator(&pool).await,
        operator: support::sign_in(&pool, Role::Operator).await,
        pool,
        state,
        runner,
        downloads,
        fake,
        sabnzbd,
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

    /// A SABnzbd integration pointing at `base_url` with `api_key`.
    async fn integration_at(&self, base_url: &str, api_key: &str, client_path: &str) -> String {
        let (status, made) = self
            .send(
                &self.admin,
                Method::POST,
                "/api/v1/integrations",
                Some(json!({
                    "kind": "sabnzbd",
                    "name": format!("sab-{}", uuid::Uuid::now_v7().simple()),
                    "settings": {
                        "base_url": base_url,
                        "client_path": client_path,
                        "local_root": "downloads",
                    },
                    "secrets": { "api_key": api_key },
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
        made["id"].as_str().unwrap().to_owned()
    }

    async fn integration(&self, api_key: &str) -> String {
        self.integration_at(
            &format!("http://{}/", self.sabnzbd),
            api_key,
            "/downloads/complete/tangible",
        )
        .await
    }

    async fn submit(&self, integration: &str, body: Value) -> (StatusCode, Value) {
        self.send(
            &self.operator,
            Method::POST,
            &format!("/api/v1/integrations/{integration}/jobs"),
            Some(body),
        )
        .await
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

    async fn drain(&self) {
        for _ in 0..20 {
            if !self.runner.step().await.unwrap() {
                return;
            }
        }
    }

    /// Move job `id` from the queue to history as `slot`.
    fn finish(&self, id: &str, slot: Value) {
        let mut fake = self.fake.lock().unwrap();
        fake.queue.remove(id);
        fake.history.insert(id.to_owned(), slot);
    }
}

fn probe_nzb() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/tool-output/sabnzbd/probe.nzb"
    ))
    .unwrap()
}

/// Another NZB, so tests sharing the database do not find each other's.
fn unique_nzb() -> Vec<u8> {
    String::from_utf8(probe_nzb())
        .unwrap()
        .replace(
            "Tangible Probe Disc",
            &format!("Tangible {}", uuid::Uuid::now_v7().simple()),
        )
        .into_bytes()
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
async fn an_nzb_is_handed_over_followed_and_imported_when_it_finishes() {
    let harness = harness().await;
    let integration = harness.integration(API_KEY).await;
    let nzb = unique_nzb();
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
            .any(|client| client["id"] == integration.as_str() && client["kind"] == "sabnzbd"),
        "operators see the SABnzbd: {sources}"
    );

    let (status, job) = harness
        .submit(&integration, json!({ "nzb": base64(&nzb) }))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    assert_eq!(job["outcome"], "created");
    let nzo_id = job["client_job_id"].as_str().unwrap().to_owned();
    let identity = job["identity"].as_str().unwrap().to_owned();
    assert_eq!(identity.len(), 64, "the NZB's SHA-256");
    let import_id = job["import_id"].as_str().unwrap().to_owned();
    {
        let fake = harness.fake.lock().unwrap();
        let add = &fake.adds[0];
        assert!(add.contains("name=\"cat\"\r\n\r\ntangible\r\n"));
        assert!(
            add.contains("name=\"script\"\r\n\r\nNone\r\n"),
            "no script, explicitly"
        );
        assert!(
            add.contains(&format!("filename=\"tangible-{}.nzb\"", &identity[..12])),
            "handed over under a name made from its digest"
        );
        assert!(
            fake.uris
                .iter()
                .all(|uri| !uri.contains(API_KEY) && !uri.contains('?')),
            "the key never travels in a URL: {:?}",
            fake.uris
        );
    }

    // Queued: held in acquiring with its size.
    harness.drain().await;
    let held = harness.import(&import_id).await;
    assert_eq!(held["state"], "acquiring", "{held}");
    assert_eq!(held["bytes_expected"], 304_087);

    // The same NZB again: answered without asking SABnzbd, which would add
    // it twice.
    let (status, again) = harness
        .submit(&integration, json!({ "nzb": base64(&nzb) }))
        .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["outcome"], "already_active");
    assert_eq!(harness.fake.lock().unwrap().adds.len(), 1);

    // Completed: the file SABnzbd renamed is copied in from the watched root.
    let folder = harness
        .downloads
        .join(format!("tangible-{}", &identity[..12]));
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(folder.join("disc.iso"), iso_image()).unwrap();
    harness.finish(
        &nzo_id,
        json!({
            "nzo_id": nzo_id,
            "status": "Completed",
            "storage": format!("/downloads/complete/tangible/tangible-{}/disc.iso", &identity[..12]),
            "bytes": 65536,
        }),
    );
    tokio::time::sleep(Duration::from_millis(1100)).await;
    harness.drain().await;
    let done = harness.import(&import_id).await;
    assert_eq!(done["state"], "complete", "{done}");
    assert_eq!(done["source_kind"], "sabnzbd_job");
    assert_eq!(done["source_reference"], format!("sabnzbd:{nzo_id}"));
    assert!(
        folder.join("disc.iso").exists(),
        "the client's copy is left"
    );
    let state: Option<String> =
        sqlx::query_scalar("SELECT client_state FROM integration_jobs WHERE import_job_id = $1")
            .bind(uuid::Uuid::parse_str(&import_id).unwrap())
            .fetch_one(&harness.pool)
            .await
            .unwrap();
    assert_eq!(state.as_deref(), Some("Completed"));

    let (status, imported) = harness
        .submit(&integration, json!({ "nzb": base64(&nzb) }))
        .await;
    assert_eq!(status, StatusCode::OK, "{imported}");
    assert_eq!(imported["outcome"], "already_imported");
    assert_eq!(imported["artifact_id"], done["artifact_id"]);
    assert_eq!(harness.fake.lock().unwrap().adds.len(), 1);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_failed_download_ends_the_import_saying_why() {
    let harness = harness().await;
    let integration = harness.integration(API_KEY).await;
    let (status, job) = harness
        .submit(&integration, json!({ "nzb": base64(&unique_nzb()) }))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let nzo_id = job["client_job_id"].as_str().unwrap();
    harness.finish(
        nzo_id,
        json!({
            "nzo_id": nzo_id,
            "status": "Failed",
            "fail_message": "Download failed - Not on your server(s)",
            "storage": "/downloads/incomplete/x",
        }),
    );
    harness.drain().await;
    let ended = harness.import(job["import_id"].as_str().unwrap()).await;
    assert_eq!(ended["state"], "failed_terminal", "{ended}");
    assert_eq!(ended["error_code"], "INTEGRATION_JOB_FAILED");
    assert!(
        ended["error_detail"]
            .as_str()
            .unwrap()
            .contains("Not on your server"),
        "{ended}"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_missing_category_is_refused_before_anything_is_added() {
    let harness = harness().await;
    let integration = harness.integration(API_KEY).await;
    harness.fake.lock().unwrap().categories.clear();
    let (status, problem) = harness
        .submit(&integration, json!({ "nzb": base64(&unique_nzb()) }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(problem["code"], "INTEGRATION_MISCONFIGURED");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("no category tangible")
    );
    assert!(harness.fake.lock().unwrap().adds.is_empty());

    let (status, tried) = harness
        .send(
            &harness.admin,
            Method::POST,
            &format!("/api/v1/integrations/{integration}/test"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{tried}");
    assert_eq!(tried["ok"], false);
    assert_eq!(tried["api_version"], "4.5.3");
    assert_eq!(tried["error_code"], "INTEGRATION_MISCONFIGURED");

    harness.fake.lock().unwrap().categories = vec!["tangible".to_owned()];
    let (_, tried) = harness
        .send(
            &harness.admin,
            Method::POST,
            &format!("/api/v1/integrations/{integration}/test"),
            None,
        )
        .await;
    assert_eq!(tried["ok"], true, "{tried}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn submissions_that_will_not_do_are_refused() {
    let harness = harness().await;
    let integration = harness.integration(API_KEY).await;
    for (body, says) in [
        (json!({}), "nzb"),
        (json!({ "nzb": "AAAA", "magnet": "magnet:?x" }), "nzb"),
        (json!({ "nzb": "not base64!" }), "base64"),
        (json!({ "nzb": base64(b"<html>no</html>") }), "not an NZB"),
        (json!({ "torrent": base64(b"d4:infoe") }), "takes an NZB"),
        (
            json!({ "magnet": "magnet:?xt=urn:btih:abc" }),
            "takes an NZB",
        ),
    ] {
        let (status, problem) = harness.submit(&integration, body.clone()).await;
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

    // SABnzbd cannot read it.
    harness.fake.lock().unwrap().decline = true;
    let (status, problem) = harness
        .submit(&integration, json!({ "nzb": base64(&unique_nzb()) }))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("could not read")
    );

    // A wrong key is SABnzbd's refusal, and never repeated back.
    let wrong = harness.integration("a-wrong-api-key").await;
    let (status, problem) = harness
        .submit(&wrong, json!({ "nzb": base64(&unique_nzb()) }))
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{problem}");
    assert_eq!(problem["code"], "INTEGRATION_AUTH_FAILED");
    assert!(!problem.to_string().contains("a-wrong-api-key"));

    // An NZB is not for qBittorrent.
    let (status, made) = harness
        .send(
            &harness.admin,
            Method::POST,
            "/api/v1/integrations",
            Some(json!({
                "kind": "qbittorrent",
                "name": format!("qbt-{}", uuid::Uuid::now_v7().simple()),
                "settings": {
                    "base_url": format!("http://{}/", harness.sabnzbd),
                    "username": "admin",
                    "client_path": "/downloads/tangible",
                    "local_root": "downloads",
                },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let (status, problem) = harness
        .submit(
            made["id"].as_str().unwrap(),
            json!({ "nzb": base64(&unique_nzb()) }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert!(problem["detail"].as_str().unwrap().contains("not an NZB"));

    // A SABnzbd without its key cannot be used.
    let (status, keyless) = harness
        .send(
            &harness.admin,
            Method::POST,
            "/api/v1/integrations",
            Some(json!({
                "kind": "sabnzbd",
                "name": format!("sab-{}", uuid::Uuid::now_v7().simple()),
                "settings": {
                    "base_url": format!("http://{}/", harness.sabnzbd),
                    "client_path": "/downloads/complete/tangible",
                    "local_root": "downloads",
                },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{keyless}");
    let (status, problem) = harness
        .submit(
            keyless["id"].as_str().unwrap(),
            json!({ "nzb": base64(&unique_nzb()) }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(problem["code"], "INTEGRATION_MISCONFIGURED");
}

/// Against a real SABnzbd, when one is named: the probe NZB, which its news
/// server can serve, downloaded and imported end to end.
///
/// `TANGIBLE_TEST_SABNZBD_URL`, `TANGIBLE_TEST_SABNZBD_API_KEY`,
/// `TANGIBLE_TEST_SABNZBD_CLIENT_PATH` (the `tangible` category's finished
/// folder as SABnzbd sees it) and `TANGIBLE_TEST_SABNZBD_DOWNLOADS` (the
/// same folder here). Skips without them.
#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_real_sabnzbd_download_is_imported() {
    let vars = [
        "TANGIBLE_TEST_SABNZBD_URL",
        "TANGIBLE_TEST_SABNZBD_API_KEY",
        "TANGIBLE_TEST_SABNZBD_CLIENT_PATH",
        "TANGIBLE_TEST_SABNZBD_DOWNLOADS",
    ]
    .map(|name| std::env::var(name).ok());
    let [Some(url), Some(api_key), Some(client_path), Some(downloads)] = vars else {
        eprintln!("skipping: set TANGIBLE_TEST_SABNZBD_URL, _API_KEY, _CLIENT_PATH and _DOWNLOADS");
        return;
    };
    let harness = harness_with(Some(PathBuf::from(&downloads))).await;
    let integration = harness.integration_at(&url, &api_key, &client_path).await;
    let (status, job) = harness
        .submit(&integration, json!({ "nzb": base64(&probe_nzb()) }))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
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
    assert_eq!(done["source_kind"], "sabnzbd_job");
}

// --- NZBGet -------------------------------------------------------------------

const NZBGET_USER: &str = "tangible";
const NZBGET_PASSWORD: &str = "the-right-password";

/// What the fake NZBGet holds.
#[derive(Default)]
struct FakeNzbget {
    categories: Vec<String>,
    /// Jobs in the queue, by NZBID, as listgroups entries.
    queue: HashMap<i64, Value>,
    /// Jobs in history, by NZBID, as history entries.
    history: HashMap<i64, Value>,
    /// The params of the appends it took.
    appends: Vec<Value>,
    /// What the next append answers, when not a new NZBID.
    refuse: bool,
}

type SharedNzbget = Arc<Mutex<FakeNzbget>>;

async fn jsonrpc(
    State(fake): State<SharedNzbget>,
    headers: HeaderMap,
    axum::Json(call): axum::Json<Value>,
) -> impl IntoResponse {
    use base64::Engine as _;
    let expected = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD
            .encode(format!("{NZBGET_USER}:{NZBGET_PASSWORD}"))
    );
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some(expected.as_str()) {
        return (StatusCode::UNAUTHORIZED, "").into_response();
    }
    let mut fake = fake.lock().unwrap();
    let reply = |result: Value| axum::Json(json!({ "version": "1.1", "id": 1, "result": result }));
    match call["method"].as_str().unwrap_or_default() {
        "version" => reply(json!("25.3")).into_response(),
        "config" => {
            let mut options = vec![json!({ "Name": "ControlPassword", "Value": "never-kept" })];
            for (n, name) in fake.categories.iter().enumerate() {
                options.push(json!({ "Name": format!("Category{}.Name", n + 1), "Value": name }));
            }
            reply(json!(options)).into_response()
        }
        "append" => {
            fake.appends.push(call["params"].clone());
            if fake.refuse {
                return reply(json!(0)).into_response();
            }
            let id = i64::try_from(fake.appends.len()).unwrap();
            fake.queue.insert(
                id,
                json!({ "NZBID": id, "Status": "QUEUED", "FileSizeLo": 300_000, "FileSizeHi": 0,
                        "RemainingSizeLo": 100_000, "RemainingSizeHi": 0 }),
            );
            reply(json!(id)).into_response()
        }
        "listgroups" => {
            reply(json!(fake.queue.values().cloned().collect::<Vec<_>>())).into_response()
        }
        "history" => {
            reply(json!(fake.history.values().cloned().collect::<Vec<_>>())).into_response()
        }
        _ => axum::Json(json!({ "version": "1.1", "id": 1,
            "error": { "name": "JSONRPCError", "code": 1, "message": "Invalid procedure" } }))
        .into_response(),
    }
}

async fn fake_nzbget() -> (SocketAddr, SharedNzbget) {
    let fake: SharedNzbget = Arc::new(Mutex::new(FakeNzbget {
        // NZBGet matches category names regardless of case.
        categories: vec!["Movies".to_owned(), "Tangible".to_owned()],
        ..FakeNzbget::default()
    }));
    let app = axum::Router::new()
        .route("/jsonrpc", axum::routing::post(jsonrpc))
        .with_state(Arc::clone(&fake));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (address, fake)
}

impl Harness {
    /// An NZBGet integration pointing at `base_url`.
    async fn nzbget_at(
        &self,
        base_url: &str,
        user: &str,
        password: &str,
        client_path: &str,
    ) -> String {
        let (status, made) = self
            .send(
                &self.admin,
                Method::POST,
                "/api/v1/integrations",
                Some(json!({
                    "kind": "nzbget",
                    "name": format!("nzbget-{}", uuid::Uuid::now_v7().simple()),
                    "settings": {
                        "base_url": base_url,
                        "username": user,
                        "client_path": client_path,
                        "local_root": "downloads",
                    },
                    "secrets": { "password": password },
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
        made["id"].as_str().unwrap().to_owned()
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
#[allow(clippy::too_many_lines)]
async fn an_nzb_is_handed_to_nzbget_and_imported_when_it_succeeds() {
    let harness = harness().await;
    let (address, fake) = fake_nzbget().await;
    let integration = harness
        .nzbget_at(
            &format!("http://{address}/"),
            NZBGET_USER,
            NZBGET_PASSWORD,
            "/downloads/completed/tangible",
        )
        .await;
    let nzb = unique_nzb();
    let (status, job) = harness
        .submit(&integration, json!({ "nzb": base64(&nzb) }))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    let identity = job["identity"].as_str().unwrap().to_owned();
    let nzbid = job["client_job_id"].as_str().unwrap().to_owned();
    let import_id = job["import_id"].as_str().unwrap().to_owned();
    {
        let fake = fake.lock().unwrap();
        let params = &fake.appends[0];
        assert_eq!(params[0], format!("tangible-{}.nzb", &identity[..12]));
        assert_eq!(params[1], base64(&nzb), "the NZB itself, base64");
        assert_eq!(params[2], "tangible");
        assert_eq!(params[5], false, "not paused");
        assert_eq!(params[8], "FORCE", "Tangible decides what is a repeat");
        assert_eq!(params[9], json!([]), "no post-processing parameters");
    }

    // Queued: held in acquiring with its progress.
    harness.drain().await;
    let held = harness.import(&import_id).await;
    assert_eq!(held["state"], "acquiring", "{held}");
    assert_eq!(held["bytes_expected"], 300_000);
    assert_eq!(held["bytes_received"], 200_000);

    let (status, again) = harness
        .submit(&integration, json!({ "nzb": base64(&nzb) }))
        .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["outcome"], "already_active");
    assert_eq!(fake.lock().unwrap().appends.len(), 1);

    // Succeeded: its folder copied in from the watched root.
    let folder = harness
        .downloads
        .join(format!("tangible-{}", &identity[..12]));
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(folder.join("disc.iso"), iso_image()).unwrap();
    {
        let mut fake = fake.lock().unwrap();
        let id: i64 = nzbid.parse().unwrap();
        fake.queue.remove(&id);
        fake.history.insert(
            id,
            json!({ "NZBID": id, "Status": "SUCCESS/ALL", "FinalDir": "",
                    "DestDir": format!("/downloads/completed/tangible/tangible-{}", &identity[..12]),
                    "FileSizeLo": 65536, "FileSizeHi": 0 }),
        );
    }
    tokio::time::sleep(Duration::from_millis(1100)).await;
    harness.drain().await;
    let done = harness.import(&import_id).await;
    assert_eq!(done["state"], "complete", "{done}");
    assert_eq!(done["source_kind"], "nzbget_job");
    assert_eq!(done["source_reference"], format!("nzbget:{nzbid}"));
    assert!(
        folder.join("disc.iso").exists(),
        "the client's copy is left"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn nzbget_failures_and_refusals_say_why() {
    let harness = harness().await;
    let (address, fake) = fake_nzbget().await;
    let base = format!("http://{address}/");
    let integration = harness
        .nzbget_at(
            &base,
            NZBGET_USER,
            NZBGET_PASSWORD,
            "/downloads/completed/tangible",
        )
        .await;

    // A job NZBGet failed ends the import with NZBGet's status.
    let (status, job) = harness
        .submit(&integration, json!({ "nzb": base64(&unique_nzb()) }))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    {
        let mut fake = fake.lock().unwrap();
        let id: i64 = job["client_job_id"].as_str().unwrap().parse().unwrap();
        fake.queue.remove(&id);
        fake.history.insert(
            id,
            json!({ "NZBID": id, "Status": "FAILURE/UNPACK", "DestDir": "/downloads/intermediate/x" }),
        );
    }
    harness.drain().await;
    let ended = harness.import(job["import_id"].as_str().unwrap()).await;
    assert_eq!(ended["state"], "failed_terminal", "{ended}");
    assert_eq!(ended["error_code"], "INTEGRATION_JOB_FAILED");
    assert!(
        ended["error_detail"]
            .as_str()
            .unwrap()
            .contains("FAILURE/UNPACK")
    );

    // Refused by NZBGet.
    fake.lock().unwrap().refuse = true;
    let (status, problem) = harness
        .submit(&integration, json!({ "nzb": base64(&unique_nzb()) }))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    fake.lock().unwrap().refuse = false;

    // No such category: nothing appended, and Test says so.
    fake.lock().unwrap().categories = vec!["Movies".to_owned()];
    let before = fake.lock().unwrap().appends.len();
    let (status, problem) = harness
        .submit(&integration, json!({ "nzb": base64(&unique_nzb()) }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(problem["code"], "INTEGRATION_MISCONFIGURED");
    assert_eq!(fake.lock().unwrap().appends.len(), before);
    let (_, tried) = harness
        .send(
            &harness.admin,
            Method::POST,
            &format!("/api/v1/integrations/{integration}/test"),
            None,
        )
        .await;
    assert_eq!(tried["ok"], false, "{tried}");
    assert_eq!(tried["api_version"], "25.3");

    // A wrong password is NZBGet's refusal, and never repeated back.
    let wrong = harness
        .nzbget_at(
            &base,
            NZBGET_USER,
            "a-wrong-password",
            "/downloads/completed/tangible",
        )
        .await;
    let (status, problem) = harness
        .submit(&wrong, json!({ "nzb": base64(&unique_nzb()) }))
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{problem}");
    assert_eq!(problem["code"], "INTEGRATION_AUTH_FAILED");
    assert!(!problem.to_string().contains("a-wrong-password"));

    // A torrent is not for NZBGet.
    let (status, problem) = harness
        .submit(&integration, json!({ "magnet": "magnet:?xt=urn:btih:abc" }))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("NZBGet takes an NZB")
    );
}

/// Against a real NZBGet, when one is named: the probe NZB, which its news
/// server can serve, downloaded and imported end to end.
///
/// `TANGIBLE_TEST_NZBGET_URL`, `_USER`, `_PASSWORD`, `_CLIENT_PATH` (the
/// `tangible` category's finished folder as NZBGet sees it) and
/// `_DOWNLOADS` (the same folder here). Skips without them.
#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_real_nzbget_download_is_imported() {
    let vars = [
        "TANGIBLE_TEST_NZBGET_URL",
        "TANGIBLE_TEST_NZBGET_USER",
        "TANGIBLE_TEST_NZBGET_PASSWORD",
        "TANGIBLE_TEST_NZBGET_CLIENT_PATH",
        "TANGIBLE_TEST_NZBGET_DOWNLOADS",
    ]
    .map(|name| std::env::var(name).ok());
    let [
        Some(url),
        Some(user),
        Some(password),
        Some(client_path),
        Some(downloads),
    ] = vars
    else {
        eprintln!(
            "skipping: set TANGIBLE_TEST_NZBGET_URL, _USER, _PASSWORD, _CLIENT_PATH and _DOWNLOADS"
        );
        return;
    };
    let harness = harness_with(Some(PathBuf::from(&downloads))).await;
    let integration = harness
        .nzbget_at(&url, &user, &password, &client_path)
        .await;
    let (status, job) = harness
        .submit(&integration, json!({ "nzb": base64(&probe_nzb()) }))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
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
    assert_eq!(done["source_kind"], "nzbget_job");
}
