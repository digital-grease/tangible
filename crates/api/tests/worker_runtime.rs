// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The burn worker, run against a real server over a real socket.
//!
//! Everything the shipped binary does, minus the drive: the server listens on
//! a port, the worker enrolls over HTTP with a one-use token, reports what its
//! drive can do, claims work, downloads and verifies the artifact, writes it
//! with the hardware-free engine, reads it back, and reports the outcome.
//!
//! This is the test that would catch a protocol mismatch. The client's wire
//! types live in the burn crate and the server's live here, deliberately, so
//! something has to prove they agree, and a shape that only agrees in a
//! hand-written fixture agrees with nothing.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::PathBuf;
use std::time::Duration;

use sqlx::PgPool;
use tangible_api::{ApiState, ImportCheckpoint, ImportPipeline, ImportRequest, catalog, router};
use tangible_burn::runner::{WorkerRuntime, WorkerSettings};
use tangible_burn::{FakeBehaviour, FakeEngine};
use tangible_db::{Database, DbConfig};
use tangible_domain::{ImportJobId, LogicalPath};
use tangible_storage::{FilesystemStore, IngestLimits, ManifestStore, StagingManager};
use tempfile::TempDir;

/// Serialises the file: a claim takes any queued job.
static QUEUE: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

async fn exclusive_queue() -> tokio::sync::MutexGuard<'static, ()> {
    QUEUE
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

const SECTOR: usize = 2048;
const SYSTEM_AREA: usize = 16 * SECTOR;

fn iso_image(volume_id: &str, blocks: u32) -> Vec<u8> {
    let mut bytes = vec![0_u8; SYSTEM_AREA];
    let mut pvd = vec![0_u8; SECTOR];
    pvd[0] = 1;
    pvd[1..6].copy_from_slice(b"CD001");
    pvd[6] = 1;
    for slot in &mut pvd[40..72] {
        *slot = b' ';
    }
    pvd[40..40 + volume_id.len()].copy_from_slice(volume_id.as_bytes());
    pvd[80..84].copy_from_slice(&blocks.to_le_bytes());
    pvd[84..88].copy_from_slice(&blocks.to_be_bytes());
    pvd[128..130].copy_from_slice(&2048_u16.to_le_bytes());
    pvd[130..132].copy_from_slice(&2048_u16.to_be_bytes());
    bytes.extend_from_slice(&pvd);

    let mut terminator = vec![0_u8; SECTOR];
    terminator[0] = 255;
    terminator[1..6].copy_from_slice(b"CD001");
    terminator[6] = 1;
    bytes.extend_from_slice(&terminator);
    bytes.resize(blocks as usize * SECTOR, 0);
    bytes
}

/// A server listening on a real port, with a library behind it.
struct Server {
    base_url: String,
    pool: PgPool,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<()>>,
    _dir: TempDir,
    pipeline: ImportPipeline,
    database: Database,
}

async fn server() -> Server {
    let url = std::env::var("TANGIBLE_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TANGIBLE_DATABASE_URL"))
        .expect("set TANGIBLE_TEST_DATABASE_URL to run integration tests");
    let database = Database::connect(&DbConfig::new(url))
        .await
        .expect("connect");
    database.migrate().await.expect("migrate");
    let pool = database.pool().clone();

    let dir = TempDir::new().expect("temp dir");
    let objects = FilesystemStore::open(dir.path().join("library"))
        .await
        .expect("objects");
    let manifests = ManifestStore::open(objects.clone())
        .await
        .expect("manifests");
    let staging = StagingManager::open(dir.path().join("staging"))
        .await
        .expect("staging");
    let pipeline = ImportPipeline::new(staging, objects, manifests.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let address = listener.local_addr().expect("local address");

    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    let state = ApiState::with_manifests(database.clone(), manifests);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router(state))
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await;
    });

    Server {
        base_url: format!("http://{address}"),
        pool,
        shutdown: Some(shutdown),
        handle: Some(handle),
        _dir: dir,
        pipeline,
        database,
    }
}

impl Server {
    /// Import an ISO and register it, as an import route will.
    async fn import(&self, bytes: &[u8]) -> String {
        let import_id = ImportJobId::generate();
        let area = self.pipeline.open_area(import_id).await.expect("area");
        area.write(&LogicalPath::parse("disc.iso").expect("path"), bytes)
            .await
            .expect("stage");

        let mut checkpoint = ImportCheckpoint::default();
        let outcome = self
            .pipeline
            .run(
                &ImportRequest {
                    import_id,
                    source_kind: "upload".to_owned(),
                    source_filename: Some("disc.iso".to_owned()),
                    source_reference: None,
                    limits: IngestLimits {
                        max_bytes: None,
                        fsync: false,
                    },
                },
                &mut checkpoint,
            )
            .await
            .expect("import");

        let manifest = self
            .pipeline
            .manifests()
            .read(outcome.artifact_id)
            .await
            .expect("manifest");
        catalog::register_manifest(&self.database, &manifest)
            .await
            .expect("register");
        outcome.artifact_id.to_string()
    }

    /// A disc in the catalog for a burn to produce.
    async fn seed_disc(&self) -> String {
        let ids: Vec<uuid::Uuid> = (0..3).map(|_| uuid::Uuid::now_v7()).collect();
        let disc = uuid::Uuid::now_v7();
        for statement in [
            (
                "INSERT INTO titles (id, display_title, sort_title) VALUES ($1, 'T', 'T')",
                vec![ids[0]],
            ),
            (
                "INSERT INTO editions (id, title_id, display_name) VALUES ($1, $2, 'E')",
                vec![ids[1], ids[0]],
            ),
            (
                "INSERT INTO disc_sets (id, edition_id, name) VALUES ($1, $2, 'S')",
                vec![ids[2], ids[1]],
            ),
            (
                "INSERT INTO discs (id, disc_set_id, sequence_number) VALUES ($1, $2, 1)",
                vec![disc, ids[2]],
            ),
        ] {
            let mut query = sqlx::query(statement.0);
            for id in statement.1 {
                query = query.bind(id);
            }
            query.execute(&self.pool).await.expect("seed the catalog");
        }
        disc.to_string()
    }

    /// Issue a one-use enrollment token, as an administrator would.
    async fn enrollment_token(&self) -> String {
        let secret = format!("tgw_enroll_{}", uuid::Uuid::now_v7().simple());
        let hash = {
            use sha2::Digest as _;
            let mut hasher = sha2::Sha256::new();
            hasher.update(secret.as_bytes());
            hex::encode(hasher.finalize())
        };
        sqlx::query(
            "INSERT INTO worker_enrollments (id, token_hash, expires_at, created_by)
             VALUES ($1, $2, now() + interval '1 hour', 'test')",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(hash)
        .execute(&self.pool)
        .await
        .expect("issue a token");
        secret
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        let response = reqwest::Client::new()
            .post(format!("{}{path}", self.base_url))
            .json(&body)
            .send()
            .await
            .expect("send");
        let status = response.status().as_u16();
        let value = response.json().await.unwrap_or(serde_json::Value::Null);
        (status, value)
    }

    async fn get(&self, path: &str) -> serde_json::Value {
        reqwest::Client::new()
            .get(format!("{}{path}", self.base_url))
            .send()
            .await
            .expect("send")
            .json()
            .await
            .unwrap_or(serde_json::Value::Null)
    }

    /// Queue a burn that this file's worker will be the one to claim.
    async fn queue_burn(&self, disc: &str, artifact: &str) -> String {
        let (status, body) = self
            .post(
                "/api/v1/burn-jobs",
                serde_json::json!({
                    "disc_id": disc,
                    "artifact_id": artifact,
                    "priority": 100,
                }),
            )
            .await;
        assert_eq!(status, 201, "{body}");
        body["id"].as_str().expect("a job id").to_owned()
    }

    async fn drain_queue(&self) {
        sqlx::query("UPDATE burn_jobs SET state = 'canceled' WHERE state = 'queued'")
            .execute(&self.pool)
            .await
            .expect("drain");
    }

    /// Wait for a job to settle, returning its final view.
    async fn await_settled(&self, job: &str) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let body = self.get(&format!("/api/v1/burn-jobs/{job}")).await;
            let state = body["state"].as_str().unwrap_or_default().to_owned();
            if matches!(
                state.as_str(),
                "complete" | "failed" | "canceled" | "needs_attention"
            ) {
                return body;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the burn never settled; last state {state}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn stop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}

/// Worker settings tuned for a test: everything that would wait, waits
/// briefly.
fn settings(server: &Server, state_dir: PathBuf, token: String) -> WorkerSettings {
    WorkerSettings {
        enrollment_token: Some(token),
        poll_interval: Duration::from_millis(100),
        media_wait: Duration::from_secs(3),
        media_poll_interval: Duration::from_millis(100),
        report_interval: Duration::from_millis(100),
        device_alias: "/dev/disc-block".to_owned(),
        configured_name: "test-drive".to_owned(),
        ..WorkerSettings::new(
            server.base_url.clone(),
            format!("runtime-worker-{}", uuid::Uuid::now_v7()),
            state_dir,
        )
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_worker_enrolls_claims_burns_and_reports_without_help() {
    let _queue = exclusive_queue().await;
    let mut server = server().await;
    server.drain_queue().await;

    let image = iso_image("RUNTIME", 32);
    let artifact = server.import(&image).await;
    let disc = server.seed_disc().await;
    let job = server.queue_burn(&disc, &artifact).await;

    let worker_dir = TempDir::new().expect("temp dir");
    let token = server.enrollment_token().await;
    let settings = settings(&server, worker_dir.path().to_path_buf(), token);
    let media_root = settings.state_dir.join("media");
    let engine = FakeEngine::new(&media_root);

    let (stop, stopped) = tokio::sync::oneshot::channel();
    let worker = tokio::spawn(async move {
        let mut runtime = WorkerRuntime::new(settings, engine).expect("runtime");
        runtime
            .run(async {
                let _ = stopped.await;
            })
            .await
    });

    let settled = server.await_settled(&job).await;
    let _ = stop.send(());
    let outcome = worker.await.expect("the worker task");
    assert!(outcome.is_ok(), "the worker exited badly: {outcome:?}");

    assert_eq!(settled["state"], "complete", "{settled}");
    assert_eq!(settled["attempt_count"], 1);
    assert_eq!(settled["attempts"][0]["state"], "verified");
    assert!(
        settled["attempts"][0]["physical_copy_id"].is_string(),
        "a verified burn records a disc: {settled}"
    );

    // The simulated disc holds the imported bytes. Nothing in the chain
    // (download, staging, plan, write) reordered or truncated them.
    let media: Vec<PathBuf> = std::fs::read_dir(&media_root)
        .expect("media directory")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    let written = media.first().expect("one simulated disc");
    assert_eq!(std::fs::read(written).expect("read the disc"), image);

    // The worker reported its way through the stages, so an operator watching
    // saw more than "queued" and then "done".
    let attempt = settled["attempts"][0]["id"].as_str().expect("an attempt");
    let events = server
        .get(&format!("/api/v1/burn-attempts/{attempt}/events?limit=500"))
        .await;
    let stages: Vec<String> = events["items"]
        .as_array()
        .expect("events")
        .iter()
        .filter_map(|event| event["stage"].as_str().map(ToOwned::to_owned))
        .collect();
    for expected in ["staging", "preflighting", "writing", "verifying"] {
        assert!(
            stages.iter().any(|stage| stage == expected),
            "{expected} was never reported: {stages:?}"
        );
    }

    server.stop().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_worker_that_produces_a_bad_disc_says_so() {
    // The engine reports a clean write and the medium is corrupt anyway. The
    // worker's own read-back is what catches it, and the disc it made is
    // recorded rather than quietly dropped.
    let _queue = exclusive_queue().await;
    let mut server = server().await;
    server.drain_queue().await;

    let artifact = server.import(&iso_image("BADRUN", 24)).await;
    let disc = server.seed_disc().await;
    let job = server.queue_burn(&disc, &artifact).await;

    let worker_dir = TempDir::new().expect("temp dir");
    let token = server.enrollment_token().await;
    let settings = settings(&server, worker_dir.path().to_path_buf(), token);
    let engine = FakeEngine::new(settings.state_dir.join("media")).with_behaviour(FakeBehaviour {
        corrupt_after_write: true,
        ..FakeBehaviour::default()
    });

    let (stop, stopped) = tokio::sync::oneshot::channel();
    let worker = tokio::spawn(async move {
        let mut runtime = WorkerRuntime::new(settings, engine).expect("runtime");
        runtime
            .run(async {
                let _ = stopped.await;
            })
            .await
    });

    let settled = server.await_settled(&job).await;
    let _ = stop.send(());
    let _ = worker.await;

    assert_eq!(settled["state"], "failed", "{settled}");
    assert_eq!(settled["attempts"][0]["state"], "verification_failed");
    assert_eq!(settled["attempts"][0]["consumed_media"], true);
    assert!(
        settled["attempts"][0]["physical_copy_id"].is_string(),
        "the bad disc is still recorded so it can be destroyed: {settled}"
    );

    server.stop().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_worker_with_no_disc_waits_and_then_reports_no_media() {
    // The drive is empty and stays empty. The worker must ask for a disc,
    // wait, and then finish without inventing one: no write, no physical
    // copy, and a reason an operator can read.
    let _queue = exclusive_queue().await;
    let mut server = server().await;
    server.drain_queue().await;

    let artifact = server.import(&iso_image("NOMEDIA", 20)).await;
    let disc = server.seed_disc().await;
    let job = server.queue_burn(&disc, &artifact).await;

    let worker_dir = TempDir::new().expect("temp dir");
    let token = server.enrollment_token().await;
    let mut settings = settings(&server, worker_dir.path().to_path_buf(), token);
    settings.media_wait = Duration::from_millis(300);
    let engine = FakeEngine::new(settings.state_dir.join("media")).with_behaviour(FakeBehaviour {
        no_medium: true,
        ..FakeBehaviour::default()
    });

    let (stop, stopped) = tokio::sync::oneshot::channel();
    let worker = tokio::spawn(async move {
        let mut runtime = WorkerRuntime::new(settings, engine).expect("runtime");
        runtime
            .run(async {
                let _ = stopped.await;
            })
            .await
    });

    let settled = server.await_settled(&job).await;
    let _ = stop.send(());
    let _ = worker.await;

    assert_eq!(settled["state"], "failed", "{settled}");
    assert_eq!(settled["attempts"][0]["state"], "failed_before_write");
    assert_eq!(settled["attempts"][0]["consumed_media"], false);
    assert!(
        settled["attempts"][0]["physical_copy_id"].is_null(),
        "nothing was written, so there is no disc: {settled}"
    );
    assert_eq!(
        settled["attempts"][0]["error_code"], "PREFLIGHT_NO_MEDIUM",
        "the operator is told which check stopped it"
    );

    let attempt = settled["attempts"][0]["id"].as_str().expect("an attempt");
    let events = server
        .get(&format!("/api/v1/burn-attempts/{attempt}/events?limit=500"))
        .await;
    let codes: Vec<String> = events["items"]
        .as_array()
        .expect("events")
        .iter()
        .filter_map(|event| event["code"].as_str().map(ToOwned::to_owned))
        .collect();
    assert!(
        codes.iter().any(|code| code == "MEDIA_REQUIRED"),
        "the worker must ask for a disc: {codes:?}"
    );

    server.stop().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_restarted_worker_reuses_its_credential() {
    // The credential is returned exactly once. A worker that enrolled again
    // on every restart would need a fresh token each time and would litter
    // the workers page.
    let mut server = server().await;
    let worker_dir = TempDir::new().expect("temp dir");
    let token = server.enrollment_token().await;
    let settings = settings(&server, worker_dir.path().to_path_buf(), token);
    let engine = FakeEngine::new(settings.state_dir.join("media"));

    let mut first = WorkerRuntime::new(settings.clone(), engine.clone()).expect("runtime");
    let identity = first.identify().await.expect("enroll");

    // The token is spent, so a worker that tried to enroll again would fail.
    let mut second = WorkerRuntime::new(settings, engine).expect("runtime");
    let again = second
        .identify()
        .await
        .expect("reuse the stored credential");
    assert_eq!(identity.worker_id, again.worker_id);

    let workers: i64 = sqlx::query_scalar("SELECT count(*) FROM workers WHERE id = $1::uuid")
        .bind(&identity.worker_id)
        .fetch_one(&server.pool)
        .await
        .expect("count");
    assert_eq!(workers, 1);

    server.stop().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_worker_reports_its_drive_before_asking_for_work() {
    // A claim names a drive, and the drive has to exist first. The identifier
    // comes back from the server rather than being chosen locally.
    let mut server = server().await;
    let worker_dir = TempDir::new().expect("temp dir");
    let token = server.enrollment_token().await;
    let settings = settings(&server, worker_dir.path().to_path_buf(), token);
    let engine = FakeEngine::new(settings.state_dir.join("media"));

    let mut runtime = WorkerRuntime::new(settings, engine).expect("runtime");
    let mut identity = runtime.identify().await.expect("enroll");
    let drive = runtime.announce(&mut identity).await.expect("announce");

    let (alias, capabilities): (String, serde_json::Value) =
        sqlx::query_as("SELECT device_alias, capabilities_json FROM drives WHERE id = $1::uuid")
            .bind(drive.as_uuid())
            .fetch_one(&server.pool)
            .await
            .expect("the drive");
    assert_eq!(alias, "/dev/disc-block");
    assert!(
        capabilities["write_profiles"].is_array(),
        "the probe's findings are recorded: {capabilities}"
    );

    // Reported again on the next start, and it is the same drive.
    let again = runtime
        .announce(&mut identity)
        .await
        .expect("announce again");
    assert_eq!(drive, again);

    server.stop().await;
}
