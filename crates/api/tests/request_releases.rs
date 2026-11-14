// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! An approved request searched for through Prowlarr, a release picked and
//! grabbed, and the request fulfilled by the import that follows it (ADR
//! 0021, docs/plans/requests.md Q3), through the real router and runner.
//!
//! One fake on 127.0.0.1 is both. At `/prowlarr` it answers as Prowlarr
//! 2.7.0 was recorded answering (fixtures/tool-output/prowlarr), its
//! releases' links pointing back at it: a `.torrent` for a torrent, a 301
//! to a magnet link for a magnet-only one, a 301 to the indexer for an NZB,
//! 401 for a wrong key. At `/api/v2` it is the part of qBittorrent's WebUI
//! the handover uses, as `torrent_imports` has it.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path as UrlPath, Query, State};
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

const PASSWORD: &str = "the-qbittorrent-password";
const SESSION: &str = "fake-session-id";
const KEY: &str = "the-prowlarr-key-0123456789";
const MAGNET: &str = "magnet:?xt=urn:btih:ed0d831bcefc077847bc1fa808bd2ae5fd32ae9b&dn=Big%20Buck%20Bunny%20Soundtrack%20%282008%29%20%5BFLAC%20CD%20image%5D";
const MAGNET_HASH: &str = "ed0d831bcefc077847bc1fa808bd2ae5fd32ae9b";

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/../../fixtures/tool-output/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
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

/// What the fake holds and saw.
#[derive(Default)]
struct Fake {
    /// The base the releases' links point at.
    base: String,
    /// The query of each search, and its key header.
    searches: Vec<(HashMap<String, String>, Option<String>)>,
    /// Each release link fetched: its query, and whether a key header came.
    fetches: Vec<(String, bool)>,
    /// Torrents by hash, as qBittorrent lists them.
    torrents: HashMap<String, Value>,
    /// The hash the next add is for.
    adding: Option<String>,
    /// The bodies of the adds.
    adds: Vec<String>,
}

type Shared = Arc<Mutex<Fake>>;

fn keyed(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

async fn status(headers: HeaderMap) -> impl IntoResponse {
    if keyed(&headers).as_deref() != Some(KEY) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    (
        [("content-type", "application/json")],
        fixture("prowlarr/system-status-2.7.0.json"),
    )
        .into_response()
}

async fn indexers(headers: HeaderMap) -> impl IntoResponse {
    if keyed(&headers).as_deref() != Some(KEY) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    (
        [("content-type", "application/json")],
        fixture("prowlarr/indexers-2.7.0.json"),
    )
        .into_response()
}

async fn paused(headers: HeaderMap) -> impl IntoResponse {
    if keyed(&headers).as_deref() != Some(KEY) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // The Newznab indexer paused until well after the test.
    let until = time::OffsetDateTime::now_utc() + Duration::from_secs(600);
    let until = until
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    axum::Json(json!([{ "indexerId": 2, "disabledTill": until }])).into_response()
}

async fn search(
    State(fake): State<Shared>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let key = keyed(&headers);
    let mut fake = fake.lock().unwrap();
    fake.searches.push((query.clone(), key.clone()));
    if key.as_deref() != Some(KEY) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let words = query.get("query").cloned().unwrap_or_default();
    let recorded = if words.contains("Soundtrack") {
        "prowlarr/search-magnet-only-2.7.0.json"
    } else if words.contains("nothing") {
        return axum::Json(json!([])).into_response();
    } else if query.get("type").map(String::as_str) == Some("movie") {
        "prowlarr/search-imdb-2.7.0.json"
    } else {
        "prowlarr/search-movies-2.7.0.json"
    };
    // The recorded links point at the recording's Prowlarr; these point
    // here, with this key.
    let body = fixture(recorded)
        .replace(
            "http://127.0.0.1:19696/",
            &format!("{}/prowlarr/", fake.base),
        )
        .replace("REDACTED-API-KEY", KEY);
    ([("content-type", "application/json")], body).into_response()
}

async fn download(
    State(fake): State<Shared>,
    UrlPath(indexer): UrlPath<String>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    uri: axum::http::Uri,
) -> impl IntoResponse {
    let mut fake = fake.lock().unwrap();
    fake.fetches.push((
        uri.query().unwrap_or_default().to_owned(),
        headers.contains_key("x-api-key"),
    ));
    if query.get("apikey").map(String::as_str) != Some(KEY) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let file = query.get("file").cloned().unwrap_or_default();
    if file.contains("Soundtrack") {
        return (StatusCode::MOVED_PERMANENTLY, [("location", MAGNET)]).into_response();
    }
    if indexer == "2" {
        return (
            StatusCode::MOVED_PERMANENTLY,
            [("location", "/indexer/u1.nzb?apikey=indexer-key")],
        )
            .into_response();
    }
    let (bytes, _) = fixture_torrent();
    ([("content-type", "application/x-bittorrent")], bytes).into_response()
}

async fn indexer_nzb() -> impl IntoResponse {
    (
        [("content-type", "application/x-nzb")],
        fixture("sabnzbd/probe.nzb"),
    )
}

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

async fn add(
    State(fake): State<Shared>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    if !signed_in(&headers) {
        return (StatusCode::FORBIDDEN, "Forbidden").into_response();
    }
    let mut fake = fake.lock().unwrap();
    fake.adds.push(String::from_utf8_lossy(&body).into_owned());
    let Some(hash) = fake.adding.clone() else {
        return "Fails.".into_response();
    };
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

fn torrent(hash: &str, state: &str, progress: f64, content_path: &str) -> Value {
    let size = 65_536_i64;
    json!({
        "hash": hash,
        "state": state,
        "progress": progress,
        "content_path": content_path,
        "size": if content_path.is_empty() { 0 } else { size },
        "amount_left": if progress >= 1.0 { 0 } else { size },
        "has_metadata": !content_path.is_empty(),
        "category": "tangible",
        "name": "disc.iso",
    })
}

async fn fake() -> (SocketAddr, Shared) {
    let fake: Shared = Arc::default();
    let app = axum::Router::new()
        .route("/prowlarr/api/v1/system/status", axum::routing::get(status))
        .route("/prowlarr/api/v1/indexer", axum::routing::get(indexers))
        .route("/prowlarr/api/v1/indexerstatus", axum::routing::get(paused))
        .route("/prowlarr/api/v1/search", axum::routing::get(search))
        .route("/prowlarr/{indexer}/download", axum::routing::get(download))
        .route("/indexer/u1.nzb", axum::routing::get(indexer_nzb))
        .route("/api/v2/auth/login", axum::routing::post(login))
        .route("/api/v2/torrents/add", axum::routing::post(add))
        .route("/api/v2/torrents/info", axum::routing::get(info))
        .with_state(Arc::clone(&fake));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    fake.lock().unwrap().base = format!("http://{address}");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (address, fake)
}

static TURN: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

struct Harness {
    pool: PgPool,
    state: ApiState,
    runner: ImportRunner,
    downloads: PathBuf,
    fake: Shared,
    address: SocketAddr,
    admin: support::Session,
    operator: support::Session,
    viewer: support::Session,
    _dir: TempDir,
    _turn: tokio::sync::MutexGuard<'static, ()>,
}

/// Prowlarr integrations earlier runs left: there may be only one.
async fn remove_prowlarrs(pool: &PgPool) {
    sqlx::query(
        "DELETE FROM integration_secrets WHERE integration_id IN
             (SELECT id FROM integrations WHERE kind = 'prowlarr')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM integrations WHERE kind = 'prowlarr'")
        .execute(pool)
        .await
        .unwrap();
}

async fn harness() -> Harness {
    let turn = TURN
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let database = support::database().await;
    let pool = database.pool().clone();
    remove_prowlarrs(&pool).await;
    let dir = TempDir::new().unwrap();
    let objects = FilesystemStore::open(dir.path().join("library"))
        .await
        .unwrap();
    let manifests = ManifestStore::open(objects.clone()).await.unwrap();
    let staging = StagingManager::open(dir.path().join("staging"))
        .await
        .unwrap();
    let pipeline = ImportPipeline::new(staging, objects, manifests.clone());
    let downloads = dir.path().join("downloads");
    std::fs::create_dir(&downloads).unwrap();
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
    // Imports earlier runs left claimable belong to fakes that are gone.
    sqlx::query(
        "UPDATE import_jobs SET state = 'canceled', lease_owner = NULL, lease_expires_at = NULL
         WHERE state IN ('requested', 'acquiring', 'staged', 'hashing', 'inspecting',
                         'registering', 'failed_retryable')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (address, fake) = fake().await;
    Harness {
        admin: support::administrator(&pool).await,
        operator: support::sign_in(&pool, Role::Operator).await,
        viewer: support::sign_in(&pool, Role::Viewer).await,
        pool,
        state,
        runner,
        downloads,
        fake,
        address,
        _dir: dir,
        _turn: turn,
    }
}

impl Harness {
    async fn send(
        &self,
        session: &support::Session,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value, String) {
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
        let bytes = axum::body::to_bytes(response.into_body(), 4 << 20)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            text,
        )
    }

    async fn integration(&self, body: Value) -> String {
        let (status, made, _) = self
            .send(
                &self.admin,
                Method::POST,
                "/api/v1/integrations",
                Some(body),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
        made["id"].as_str().unwrap().to_owned()
    }

    async fn prowlarr(&self, key: &str) -> String {
        self.integration(json!({
            "kind": "prowlarr",
            "name": format!("prowlarr-{}", uuid::Uuid::now_v7().simple()),
            "settings": { "base_url": format!("http://{}/prowlarr", self.address) },
            "secrets": { "api_key": key },
        }))
        .await
    }

    async fn qbittorrent(&self) -> String {
        self.integration(json!({
            "kind": "qbittorrent",
            "name": format!("qbt-{}", uuid::Uuid::now_v7().simple()),
            "settings": {
                "base_url": format!("http://{}/", self.address),
                "username": "admin",
                "client_path": "/downloads/tangible",
                "local_root": "downloads",
            },
            "secrets": { "password": PASSWORD },
        }))
        .await
    }

    /// A film asked for by the viewer with its IMDb ID, and approved.
    async fn approved_film(&self) -> String {
        let (status, made, _) = self
            .send(
                &self.viewer,
                Method::POST,
                "/api/v1/requests",
                Some(json!({
                    "kind": "movie",
                    "title": "Big Buck Bunny",
                    "details": { "year": "2008" },
                    "refs": [{ "provider": "imdb", "external_id": "tt1254207" }],
                    "attests_ownership": true,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
        let id = made["id"].as_str().unwrap().to_owned();
        assert_eq!(made["can_search"], false);
        let (status, approved, _) = self
            .send(
                &self.operator,
                Method::POST,
                &format!("/api/v1/requests/{id}/approve"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{approved}");
        assert_eq!(approved["can_search"], true);
        id
    }

    async fn search(&self, id: &str, body: Value) -> (StatusCode, Value, String) {
        self.send(
            &self.operator,
            Method::POST,
            &format!("/api/v1/requests/{id}/search"),
            Some(body),
        )
        .await
    }

    async fn grab(&self, id: &str, body: Value) -> (StatusCode, Value, String) {
        self.send(
            &self.operator,
            Method::POST,
            &format!("/api/v1/requests/{id}/grab"),
            Some(body),
        )
        .await
    }

    async fn request(&self, id: &str) -> Value {
        self.send(
            &self.operator,
            Method::GET,
            &format!("/api/v1/requests/{id}"),
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
}

/// A small ISO 9660 image, unique to the test that makes it.
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
    // Bytes no other test's image has, past the descriptors.
    let unique = uuid::Uuid::now_v7();
    bytes[20 * SECTOR..20 * SECTOR + 16].copy_from_slice(unique.as_bytes());
    bytes
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
#[allow(clippy::too_many_lines)]
async fn a_request_is_searched_for_grabbed_and_fulfilled_by_its_import() {
    let harness = harness().await;
    let prowlarr = harness.prowlarr(KEY).await;
    let qbittorrent = harness.qbittorrent().await;
    let id = harness.approved_film().await;

    // Trying the Prowlarr asks its version and indexers.
    let (status, tried, _) = harness
        .send(
            &harness.admin,
            Method::POST,
            &format!("/api/v1/integrations/{prowlarr}/test"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{tried}");
    assert_eq!(tried["ok"], true, "{tried}");
    assert_eq!(tried["api_version"], "2.7.0.5723");

    // A viewer does not search.
    let (status, _, _) = harness
        .send(
            &harness.viewer,
            Method::POST,
            &format!("/api/v1/requests/{id}/search"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A film with an IMDb ID is searched for by it, in films.
    let (status, found, text) = harness.search(&id, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert_eq!(found["searched_by"], "imdb_id");
    assert_eq!(found["query"], "tt1254207");
    {
        let fake = harness.fake.lock().unwrap();
        let (query, key) = fake.searches.last().unwrap();
        assert_eq!(query["type"], "movie");
        assert_eq!(query["query"], "{ImdbId:tt1254207}");
        assert_eq!(query["categories"], "2000");
        assert_eq!(key.as_deref(), Some(KEY));
    }
    // No link, and no key, is ever answered.
    assert!(!text.contains(KEY) && !text.contains("/download"), "{text}");
    let results = found["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["title"], "Big Buck Bunny (2008) [Blu-ray ISO]");
    assert_eq!(results[0]["protocol"], "torrent");
    assert_eq!(results[0]["seeders"], 48);
    assert_eq!(results[0]["can_grab"], true);
    assert!(
        results[0]["reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("it names the year, 2008")),
        "{}",
        results[0]
    );
    assert_eq!(found["indexers_enabled"], 2);
    assert_eq!(found["indexers_paused"][0]["name"], "Fake Newznab");
    assert!(
        found["clients"]
            .as_array()
            .unwrap()
            .iter()
            .any(|client| client["id"] == qbittorrent.as_str() && client["protocol"] == "torrent")
    );

    // The search is held, and answered again as it was.
    let (status, held, _) = harness
        .send(
            &harness.operator,
            Method::GET,
            &format!("/api/v1/requests/{id}/search"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{held}");
    assert_eq!(held["results"][0]["id"], results[0]["id"]);

    // A result not held is refused, and nothing is fetched.
    let (status, refused, _) = harness
        .grab(
            &id,
            json!({ "result_id": uuid::Uuid::now_v7().to_string(), "integration_id": qbittorrent }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["code"], "SEARCH_EXPIRED");
    assert!(harness.fake.lock().unwrap().fetches.is_empty());

    // Grabbed: the .torrent fetched through Prowlarr's link and handed over.
    let (_, hash) = fixture_torrent();
    harness.fake.lock().unwrap().adding = Some(hash.clone());
    let (status, grabbed, text) = harness
        .grab(
            &id,
            json!({ "result_id": results[0]["id"], "integration_id": qbittorrent }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{grabbed}");
    assert!(!text.contains(KEY), "{text}");
    assert_eq!(grabbed["outcome"], "created");
    assert_eq!(grabbed["request"]["state"], "downloading");
    assert_eq!(grabbed["request"]["can_search"], false);
    assert_eq!(grabbed["integration_id"], qbittorrent.as_str());
    let import_id = grabbed["import_id"].as_str().unwrap().to_owned();
    assert_eq!(grabbed["request"]["import_id"], import_id.as_str());
    {
        let fake = harness.fake.lock().unwrap();
        let (query, key_header) = fake.fetches.last().unwrap();
        assert!(query.contains(&format!("apikey={KEY}")), "{query}");
        assert!(!key_header, "the link carries the key; no header is sent");
        let added = fake.adds.last().unwrap();
        assert!(
            added.contains("name=\"category\"\r\n\r\ntangible"),
            "{added}"
        );
    }

    // Downloading, it is not grabbed or searched for again.
    let (status, _, _) = harness
        .grab(
            &id,
            json!({ "result_id": results[1]["id"], "integration_id": qbittorrent }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _, _) = harness.search(&id, json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // The audit log has the search and the grab, and never the link.
    let audit: Vec<(String, Value)> = sqlx::query_as(
        "SELECT action, metadata FROM audit_events WHERE target_id = $1 ORDER BY id",
    )
    .bind(&id)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    let actions: Vec<&str> = audit.iter().map(|(action, _)| action.as_str()).collect();
    assert_eq!(
        actions,
        [
            "request.created",
            "request.approved",
            "request.searched",
            "request.grabbed"
        ]
    );
    for (_, metadata) in &audit {
        let text = metadata.to_string();
        assert!(!text.contains(KEY) && !text.contains("download"), "{text}");
    }
    assert_eq!(audit[3].1["title"], "Big Buck Bunny (2008) [Blu-ray ISO]");

    // The download finishes; the import brings it in, and the request is
    // fulfilled with what it became.
    std::fs::write(harness.downloads.join("disc.iso"), iso_image()).unwrap();
    {
        let mut fake = harness.fake.lock().unwrap();
        fake.torrents.insert(
            hash.clone(),
            torrent(&hash, "stalledUP", 1.0, "/downloads/tangible/disc.iso"),
        );
    }
    tokio::time::sleep(Duration::from_millis(1100)).await;
    harness.drain().await;
    let request = harness.request(&id).await;
    assert_eq!(request["state"], "fulfilled", "{request}");
    let (_, import, _) = harness
        .send(
            &harness.admin,
            Method::GET,
            &format!("/api/v1/imports/{import_id}"),
            None,
        )
        .await;
    assert_eq!(import["state"], "complete", "{import}");
    assert_eq!(request["artifact_id"], import["artifact_id"]);
    remove_prowlarrs(&harness.pool).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_magnet_only_release_is_grabbed_by_the_magnet_its_link_redirects_to() {
    let harness = harness().await;
    harness.prowlarr(KEY).await;
    let qbittorrent = harness.qbittorrent().await;
    let id = harness.approved_film().await;

    // Words a person gives are searched instead, in films.
    let (status, found, _) = harness
        .search(&id, json!({ "query": "Big Buck Bunny Soundtrack" }))
        .await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert_eq!(found["searched_by"], "words");
    assert_eq!(found["query"], "Big Buck Bunny Soundtrack");
    {
        let fake = harness.fake.lock().unwrap();
        let (query, _) = fake.searches.last().unwrap();
        assert_eq!(query["type"], "search");
        assert_eq!(query["query"], "Big Buck Bunny Soundtrack");
    }
    let result = found["results"][0].clone();
    assert_eq!(
        result["title"],
        "Big Buck Bunny Soundtrack (2008) [FLAC CD image]"
    );

    harness.fake.lock().unwrap().adding = Some(MAGNET_HASH.to_owned());
    let (status, grabbed, _) = harness
        .grab(
            &id,
            json!({ "result_id": result["id"], "integration_id": qbittorrent }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{grabbed}");
    assert_eq!(grabbed["request"]["state"], "downloading");
    let added = harness.fake.lock().unwrap().adds.last().unwrap().clone();
    assert!(
        added.contains(MAGNET_HASH),
        "the magnet link was handed over: {added}"
    );
    remove_prowlarrs(&harness.pool).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_search_needs_an_approved_request_a_prowlarr_and_its_key() {
    let harness = harness().await;
    let id = harness.approved_film().await;

    // No Prowlarr.
    let (status, refused, _) = harness.search(&id, json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["code"], "INTEGRATION_OFF");

    // A request waiting for approval is not searched for.
    let (_, waiting, _) = harness
        .send(
            &harness.viewer,
            Method::POST,
            "/api/v1/requests",
            Some(json!({ "kind": "game", "title": "x", "attests_ownership": true })),
        )
        .await;
    let waiting = waiting["id"].as_str().unwrap().to_owned();
    let (status, refused, _) = harness.search(&waiting, json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["code"], "CONFLICT");

    // Words that will not do.
    harness.prowlarr("a-wrong-key").await;
    let (status, refused, _) = harness
        .search(&id, json!({ "query": "x".repeat(201) }))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    let (status, _, _) = harness.search(&id, json!({ "query": "   " })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // A wrong key is Prowlarr refusing, said as such.
    let (status, refused, _) = harness.search(&id, json!({})).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{refused}");
    assert_eq!(refused["code"], "INTEGRATION_AUTH_FAILED");
    assert!(!refused.to_string().contains("a-wrong-key"));

    // A second Prowlarr is refused: requests are searched for in one.
    let (status, refused, _) = harness
        .send(
            &harness.admin,
            Method::POST,
            "/api/v1/integrations",
            Some(json!({
                "kind": "prowlarr",
                "name": "another-prowlarr",
                "settings": { "base_url": format!("http://{}/prowlarr", harness.address) },
                "secrets": { "api_key": KEY },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(refused["detail"].as_str().unwrap().contains("one service"));

    // A search that finds nothing is held, empty, and said to be so.
    remove_prowlarrs(&harness.pool).await;
    harness.prowlarr(KEY).await;
    let (status, found, _) = harness
        .search(&id, json!({ "query": "nothing at all" }))
        .await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert!(found["results"].as_array().unwrap().is_empty());
    remove_prowlarrs(&harness.pool).await;
}
