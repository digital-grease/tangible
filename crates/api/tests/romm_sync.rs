// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Keeping RomM in step through its API (docs/plans/romm-api.md M2), against
//! a fake RomM answering as RomM 5.4.1 did (fixtures/tool-output/romm) and
//! PostgreSQL.
//!
//! Platforms the export changed are owed a scan; the task asks RomM for one
//! quick scan of them and follows it, and a platform stays owed until a scan
//! asked for after its change finishes. A refusal (a scan already running,
//! no scan worker) is asked again; a scan that fails, vanishes or outlasts
//! the bound is recorded as such. What is owed is shared by the whole
//! database, so the tests here take turns.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use tangible_api::secrets::SecretKey;
use tangible_api::{ApiState, RommSync, RommSyncHandle, RommSyncSettings, router};
use tangible_db::romm_api::{NewScan, ScanState, owe_scans, owed_scans, record_scan};
use tangible_integrations::outbound::{OutboundClient, OutboundPolicy};
use tangible_storage::{FilesystemStore, ManifestStore};
use tempfile::TempDir;
use tower::ServiceExt as _;

mod support;

use support::romm::{ScanReply, Shared, TASK, TOKEN, TaskReply};

static TURN: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

struct Harness {
    pool: PgPool,
    database: tangible_db::Database,
    state: ApiState,
    admin: support::Session,
    client: OutboundClient,
    key: Arc<SecretKey>,
    romm: SocketAddr,
    fake: Shared,
    integration: Option<String>,
    _dir: TempDir,
    _turn: tokio::sync::MutexGuard<'static, ()>,
}

/// A pace quick enough for a test: a scan is asked after every 20 ms.
fn quick() -> RommSyncSettings {
    RommSyncSettings {
        poll: Duration::from_millis(20),
        scan_bound: Duration::from_secs(10),
        retry: Duration::from_millis(10),
        ..RommSyncSettings::default()
    }
}

async fn harness() -> Harness {
    let turn = TURN
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let database = support::database().await;
    let pool = database.pool().clone();
    remove_romm_api(&pool).await;
    // Owed scans are the whole database's; the RomM export's own tests
    // leave some behind.
    sqlx::query("DELETE FROM romm_owed_scans")
        .execute(&pool)
        .await
        .unwrap();
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
    let key = Arc::new(SecretKey::from_base64(&SecretKey::generate().unwrap()).unwrap());
    let state = ApiState::with_manifests(database.clone(), manifests)
        .with_secret_key(Arc::clone(&key))
        .with_integration_client(client.clone());
    let (romm, fake) = support::romm::start().await;
    Harness {
        admin: support::administrator(&pool).await,
        pool,
        database,
        state,
        client,
        key,
        romm,
        fake,
        integration: None,
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

    /// Configure the fake as RomM's API, with `settings` beside its address.
    async fn connect(&mut self, settings: Value) -> String {
        let url = format!("http://{}/", self.romm);
        self.connect_to(&url, TOKEN, settings).await
    }

    /// Configure the RomM at `url` as RomM's API.
    async fn connect_to(&mut self, url: &str, token: &str, settings: Value) -> String {
        let mut all = json!({ "base_url": url });
        all.as_object_mut()
            .unwrap()
            .extend(settings.as_object().unwrap().clone());
        let (status, made) = self
            .send(
                Method::POST,
                "/api/v1/integrations",
                Some(json!({
                    "kind": "romm_api",
                    "name": "RomM",
                    "enabled": true,
                    "settings": all,
                    "secrets": { "token": token },
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
        let id = made["id"].as_str().unwrap().to_owned();
        self.integration = Some(id.clone());
        id
    }

    fn sync(&self, settings: RommSyncSettings) -> RommSync {
        RommSync::new(
            self.database.clone(),
            Some(self.client.clone()),
            Some(Arc::clone(&self.key)),
            settings,
            RommSyncHandle::new(),
        )
    }

    async fn owed(&self) -> Vec<String> {
        owed_scans(&self.pool)
            .await
            .unwrap()
            .into_iter()
            .map(|owed| owed.platform)
            .collect()
    }

    async fn api(&self) -> Value {
        let (status, settings) = self.send(Method::GET, "/api/v1/romm", None).await;
        assert_eq!(status, StatusCode::OK, "{settings}");
        settings["api"].clone()
    }

    fn scans_asked(&self) -> Vec<Value> {
        self.fake.lock().unwrap().scans.clone()
    }

    async fn done(self) {
        if let Some(id) = &self.integration {
            let (status, body) = self
                .send(Method::DELETE, &format!("/api/v1/integrations/{id}"), None)
                .await;
            assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
        }
        sqlx::query("DELETE FROM romm_owed_scans")
            .execute(&self.pool)
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn what_the_export_changed_is_scanned_once_and_followed_to_its_end() {
    let mut harness = harness().await;
    assert_eq!(harness.api().await, Value::Null, "no RomM API yet");
    owe_scans(&harness.pool, &["psx", "ps2", "psx"])
        .await
        .unwrap();
    assert_eq!(harness.owed().await, ["ps2", "psx"]);

    // Without RomM's API nothing is asked, and the platforms stay owed.
    let sync = harness.sync(quick());
    assert_eq!(sync.pass().await, tangible_api::romm_sync::Pass::default());
    assert_eq!(harness.owed().await, ["ps2", "psx"]);

    let id = harness.connect(json!({})).await;
    let api = harness.api().await;
    assert_eq!(api["integration_id"], id.as_str());
    assert_eq!(api["scan_after_export"], true);
    assert_eq!(api["scans_owed"], json!(["ps2", "psx"]));
    assert_eq!(api["last_scan"], Value::Null);

    harness
        .fake
        .lock()
        .unwrap()
        .task_replies
        .extend([TaskReply::Says("queued"), TaskReply::Says("started")]);
    let pass = sync.pass().await;
    assert_eq!(pass.scan, Some(ScanState::Finished));
    assert!(!pass.retry_soon);
    assert_eq!(
        harness.scans_asked(),
        [json!({ "type": "quick", "platform_fs_slugs": ["ps2", "psx"] })]
    );
    assert!(harness.owed().await.is_empty());

    let api = harness.api().await;
    assert_eq!(api["status"], "ok", "{api}");
    assert_eq!(api["scans_owed"], json!([]));
    let scan = &api["last_scan"];
    assert_eq!(scan["state"], "finished", "{scan}");
    assert_eq!(scan["platforms"], json!(["ps2", "psx"]));
    assert_eq!(scan["total_roms"], 1);
    assert_eq!(scan["scanned_roms"], 1);
    assert_eq!(scan["new_roms"], 0);
    assert!(scan["ended_at"].is_string(), "{scan}");

    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events
         WHERE action = 'romm.scan_requested' AND target_id = $1 AND outcome = 'success'",
    )
    .bind(&id)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(audited, 1);

    // Nothing owed, nothing asked.
    assert_eq!(sync.pass().await.scan, None);
    assert_eq!(harness.scans_asked().len(), 1);
    harness.done().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_scan_romm_will_not_queue_now_is_asked_for_again() {
    let mut harness = harness().await;
    harness.connect(json!({})).await;
    owe_scans(&harness.pool, &["psx"]).await.unwrap();
    harness
        .fake
        .lock()
        .unwrap()
        .scan_replies
        .extend([ScanReply::Busy, ScanReply::NoWorker]);
    let sync = harness.sync(quick());

    let busy = sync.pass().await;
    assert_eq!(busy.scan, Some(ScanState::Refused));
    assert!(busy.retry_soon);
    assert_eq!(harness.owed().await, ["psx"]);
    let scan = harness.api().await["last_scan"].clone();
    assert_eq!(scan["state"], "refused", "{scan}");
    assert_eq!(
        scan["detail"],
        "RomM is busy: Quick Scan is already running"
    );
    assert!(scan["ended_at"].is_string());

    let no_worker = sync.pass().await;
    assert_eq!(no_worker.scan, Some(ScanState::Refused));
    let scan = harness.api().await["last_scan"].clone();
    assert_eq!(
        scan["detail"],
        "RomM has no scan worker: No scan worker is listening, so the scan cannot be queued; \
         restarting RomM brings it back"
    );
    assert_eq!(harness.owed().await, ["psx"]);

    assert_eq!(sync.pass().await.scan, Some(ScanState::Finished));
    assert!(harness.owed().await.is_empty());
    assert_eq!(harness.scans_asked().len(), 3);
    harness.done().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_change_made_while_a_scan_runs_is_owed_again() {
    let mut harness = harness().await;
    harness.connect(json!({})).await;
    owe_scans(&harness.pool, &["psx", "saturn"]).await.unwrap();
    harness.fake.lock().unwrap().hold = true;
    let sync = harness.sync(quick());
    let running = tokio::spawn(async move { sync.pass().await });

    // Once RomM has been asked after the scan, the export changes psx.
    for _ in 0..500 {
        if harness.fake.lock().unwrap().task_asks > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(harness.fake.lock().unwrap().task_asks > 0);
    let scan = harness.api().await["last_scan"].clone();
    assert_eq!(scan["state"], "running", "{scan}");
    owe_scans(&harness.pool, &["psx"]).await.unwrap();
    harness.fake.lock().unwrap().hold = false;

    assert_eq!(running.await.unwrap().scan, Some(ScanState::Finished));
    assert_eq!(
        harness.owed().await,
        ["psx"],
        "saturn is settled; psx changed since"
    );
    harness.done().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_scan_that_fails_vanishes_or_outlasts_the_bound_is_recorded_and_still_owed() {
    let mut harness = harness().await;
    harness.connect(json!({})).await;
    owe_scans(&harness.pool, &["psx"]).await.unwrap();
    let sync = harness.sync(quick());

    for (reply, state, detail) in [
        (
            TaskReply::Says("failed"),
            "failed",
            "RomM's scan failed; RomM's log says why",
        ),
        (
            TaskReply::Says("canceled"),
            "canceled",
            "the scan was stopped in RomM",
        ),
        (
            TaskReply::Gone,
            "lost",
            "RomM no longer has the task, so how it ended is not known",
        ),
    ] {
        harness.fake.lock().unwrap().task_replies.push_back(reply);
        let pass = sync.pass().await;
        assert!(pass.retry_soon, "{state}");
        let scan = harness.api().await["last_scan"].clone();
        assert_eq!(scan["state"], state, "{scan}");
        assert_eq!(scan["detail"], detail, "{scan}");
        assert_eq!(harness.owed().await, ["psx"], "{state}");
    }

    harness.fake.lock().unwrap().hold = true;
    let patient = harness.sync(RommSyncSettings {
        scan_bound: Duration::from_millis(300),
        ..quick()
    });
    assert_eq!(patient.pass().await.scan, Some(ScanState::TimedOut));
    let scan = harness.api().await["last_scan"].clone();
    assert_eq!(scan["state"], "timed_out", "{scan}");
    assert!(
        scan["detail"]
            .as_str()
            .unwrap()
            .contains("Tangible stopped waiting"),
        "{scan}"
    );
    assert_eq!(harness.owed().await, ["psx"]);
    harness.done().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_scan_left_open_by_a_restart_is_followed_on_the_next_pass() {
    let mut harness = harness().await;
    let id = harness.connect(json!({})).await;
    owe_scans(&harness.pool, &["psx"]).await.unwrap();
    // As a pass that stopped with the server would have left it.
    record_scan(
        &harness.pool,
        NewScan {
            integration_id: id.parse().unwrap(),
            platforms: &["psx".to_owned()],
            task_id: Some(TASK),
            state: ScanState::Queued,
            detail: None,
            counts: None,
        },
    )
    .await
    .unwrap();

    let pass = harness.sync(quick()).pass().await;
    // The open scan covered psx, so no other was asked for.
    assert_eq!(pass.scan, None);
    assert!(harness.scans_asked().is_empty());
    assert!(harness.owed().await.is_empty());
    assert_eq!(harness.api().await["last_scan"]["state"], "finished");
    harness.done().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn nothing_is_asked_of_a_romm_that_takes_no_scans_or_when_scans_are_off() {
    let mut harness = harness().await;
    let id = harness.connect(json!({ "scan_after_export": false })).await;
    owe_scans(&harness.pool, &["psx"]).await.unwrap();
    let sync = harness.sync(quick());
    let pass = sync.pass().await;
    assert_eq!((pass.scan, pass.retry_soon), (None, false));
    assert_eq!(harness.api().await["scan_after_export"], false);

    // RomM 5.0 to 5.3: its document has no scan a token may ask for.
    let (status, changed) = harness
        .send(
            Method::PATCH,
            &format!("/api/v1/integrations/{id}"),
            Some(json!({ "settings": {
                "base_url": format!("http://{}/", harness.romm),
                "scan_after_export": true,
            } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    harness.fake.lock().unwrap().document["paths"]
        .as_object_mut()
        .unwrap()
        .remove("/api/tasks/scan");
    let pass = sync.pass().await;
    assert_eq!((pass.scan, pass.retry_soon), (None, false));
    assert!(harness.scans_asked().is_empty());
    assert_eq!(
        harness.owed().await,
        ["psx"],
        "still owed, for RomM's own scan"
    );
    harness.done().await;
}

/// Against the RomM 5.4.1 that `TANGIBLE_TEST_ROMM_URL` and
/// `TANGIBLE_TEST_ROMM_TOKEN` name, as `tangible_integrations`' live test
/// does; skipped otherwise. A RomM whose host slept may have lost its scan
/// worker and refuse; restarting RomM brings it back.
#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_real_romm_scans_what_is_owed() {
    let (Ok(url), Ok(token)) = (
        std::env::var("TANGIBLE_TEST_ROMM_URL"),
        std::env::var("TANGIBLE_TEST_ROMM_TOKEN"),
    ) else {
        eprintln!("skipping: set TANGIBLE_TEST_ROMM_URL and TANGIBLE_TEST_ROMM_TOKEN");
        return;
    };
    let mut harness = harness().await;
    harness.connect_to(&url, &token, json!({})).await;
    owe_scans(&harness.pool, &["psx"]).await.unwrap();
    let pass = harness
        .sync(RommSyncSettings {
            poll: Duration::from_millis(500),
            scan_bound: Duration::from_secs(120),
            ..quick()
        })
        .pass()
        .await;
    let api = harness.api().await;
    assert_eq!(pass.scan, Some(ScanState::Finished), "{api}");
    assert!(harness.owed().await.is_empty());
    let scan = &api["last_scan"];
    assert_eq!(scan["platforms"], json!(["psx"]));
    assert!(scan["scanned_roms"].as_i64().unwrap() >= 1, "{scan}");
    assert_eq!(
        api["version"],
        Value::Null,
        "the version is the test's to record"
    );
    assert!(!api.to_string().contains(&token));
    harness.done().await;
}
