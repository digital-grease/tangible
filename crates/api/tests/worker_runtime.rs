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
use tangible_api::{
    ApiState, ImportCheckpoint, ImportContext, ImportPipeline, ImportRequest, ImportRunner,
    ImportRunnerSettings, catalog, router,
};
use tangible_burn::plan::{
    BlankReport, BlankRequest, BurnPlan, DriveCapabilities, DriveRef, MediumInfo, PreflightReport,
    VerifyReport, WriteMode, WriteReport,
};
use tangible_burn::runner::{WorkerRuntime, WorkerSettings};
use tangible_burn::{BurnEngine, EngineError, EventSink, FakeBehaviour, FakeEngine};
use tangible_db::{Database, DbConfig};
use tangible_domain::{ImportJobId, LogicalPath};
use tangible_storage::{FilesystemStore, IngestLimits, ManifestStore, StagingManager, WatchRoots};
use tempfile::TempDir;

mod support;

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
    /// The operator queueing burns; the worker under test has no session.
    session: support::Session,
}

async fn server() -> Server {
    server_with(false).await
}

/// A server, with decryption turned on by its administrator or not.
async fn server_with(decryption_allowed: bool) -> Server {
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
    let state = ApiState::with_manifests(database.clone(), manifests)
        .with_imports(ImportContext {
            pipeline: pipeline.clone(),
            roots: WatchRoots::new(std::collections::BTreeMap::new()),
            max_upload_bytes: 64 * 1024 * 1024,
        })
        .with_decryption_allowed(decryption_allowed);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router(state))
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await;
    });

    let session = support::administrator(&pool).await;
    Server {
        session,
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
                    physical_dump: None,
                    lineage: None,
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

    /// Import a CUE and its BIN, which is what a CD arrives as.
    ///
    /// Small on purpose: one data track and one audio track of four seconds
    /// each, the shortest a CD track may be, which is enough to have a layout
    /// and cheap to write.
    async fn import_cue_bin(&self) -> String {
        let sheet = b"FILE \"disc.bin\" BINARY\n\
  TRACK 01 MODE2/2352\n\
    INDEX 01 00:00:00\n\
  TRACK 02 AUDIO\n\
    PREGAP 00:00:02\n\
    INDEX 01 00:04:00\n";

        let import_id = ImportJobId::generate();
        let area = self.pipeline.open_area(import_id).await.expect("area");
        area.write(&LogicalPath::parse("disc.cue").expect("path"), sheet)
            .await
            .expect("stage the sheet");
        area.write(
            &LogicalPath::parse("disc.bin").expect("path"),
            &vec![7_u8; 2352 * 600],
        )
        .await
        .expect("stage the data");

        let mut checkpoint = ImportCheckpoint::default();
        let outcome = self
            .pipeline
            .run(
                &ImportRequest {
                    import_id,
                    source_kind: "upload".to_owned(),
                    source_filename: Some("disc.cue".to_owned()),
                    source_reference: None,
                    physical_dump: None,
                    lineage: None,
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
            .header("cookie", self.session.cookie())
            .header("x-csrf-token", &self.session.csrf_token)
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
            .header("cookie", self.session.cookie())
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
        sqlx::query(
            "UPDATE burn_jobs SET state = 'canceled', completed_at = now() WHERE state = 'queued'",
        )
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
        enrollment_token: Some(token.into()),
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

/// An engine that writes prepared images and not tables of contents.
///
/// Which is what the xorriso adapter is. Everything is delegated to the fake
/// engine except the one answer under test, so this stands in for a real
/// adapter's capabilities without standing in for its hardware.
#[derive(Clone)]
struct DataOnlyEngine(FakeEngine);

#[async_trait::async_trait]
impl BurnEngine for DataOnlyEngine {
    fn name(&self) -> &'static str {
        self.0.name()
    }

    fn version(&self) -> String {
        self.0.version()
    }

    fn uses_hardware(&self) -> bool {
        false
    }

    fn supports_mode(&self, mode: WriteMode) -> bool {
        matches!(mode, WriteMode::DataDiscAtOnce | WriteMode::DataTrackAtOnce)
    }

    async fn probe_drive(&self, drive: &DriveRef) -> Result<DriveCapabilities, EngineError> {
        self.0.probe_drive(drive).await
    }

    async fn inspect_medium(&self, drive: &DriveRef) -> Result<MediumInfo, EngineError> {
        self.0.inspect_medium(drive).await
    }

    async fn preflight(&self, plan: &BurnPlan) -> Result<PreflightReport, EngineError> {
        self.0.preflight(plan).await
    }

    async fn write(
        &self,
        plan: &BurnPlan,
        sink: &dyn EventSink,
    ) -> Result<WriteReport, EngineError> {
        self.0.write(plan, sink).await
    }

    async fn verify(
        &self,
        plan: &BurnPlan,
        sink: &dyn EventSink,
    ) -> Result<VerifyReport, EngineError> {
        self.0.verify(plan, sink).await
    }

    async fn blank(
        &self,
        request: &BlankRequest,
        sink: &dyn EventSink,
    ) -> Result<BlankReport, EngineError> {
        self.0.blank(request, sink).await
    }

    async fn eject(&self, drive: &DriveRef) -> Result<(), EngineError> {
        self.0.eject(drive).await
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

    // What preflight found in the tray is on the attempt, recorded once
    // although preflight may look more than once at the same disc.
    let inspected = events["items"]
        .as_array()
        .expect("events")
        .iter()
        .filter(|event| event["code"] == "MEDIUM_INSPECTED")
        .count();
    assert_eq!(inspected, 1, "{events}");
    let detail = server
        .get(&format!("/api/v1/burn-attempts/{attempt}"))
        .await;
    assert_eq!(detail["medium"]["profile"], "CD-R", "{detail}");
    assert_eq!(detail["medium"]["blank"], true, "{detail}");
    assert!(detail["medium"]["capacity_blocks"].as_u64().unwrap_or(0) > 0);

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
async fn a_worker_reports_decryption_only_when_its_image_has_it() {
    // The capability travels from the worker's start-up detection through
    // the real client to what the server stores; the published image's
    // worker reports none.
    let mut server = server().await;
    let css = tangible_burn::decryption::CssDecryption {
        library: "libdvdcss".to_owned(),
        version: "1.6.0".to_owned(),
        tool: "dvdbackup".to_owned(),
        tool_version: "0.4.2-4.1".to_owned(),
    };
    for decryption in [
        None,
        Some(tangible_burn::decryption::DecryptionCapability {
            css: Some(css.clone()),
        }),
    ] {
        let worker_dir = TempDir::new().expect("temp dir");
        let token = server.enrollment_token().await;
        let settings = WorkerSettings {
            decryption: decryption.clone(),
            ..settings(&server, worker_dir.path().to_path_buf(), token)
        };
        let engine = FakeEngine::new(settings.state_dir.join("media"));
        let mut runtime = WorkerRuntime::new(settings, engine).expect("runtime");
        let mut identity = runtime.identify().await.expect("enroll");
        runtime.announce(&mut identity).await.expect("announce");

        let rip: serde_json::Value = sqlx::query_scalar(
            "SELECT capabilities_json -> 'rip' FROM workers WHERE id = $1::uuid",
        )
        .bind(&identity.worker_id)
        .fetch_one(&server.pool)
        .await
        .expect("the worker");
        assert!(
            rip["engines"]
                .as_array()
                .is_some_and(|engines| engines.iter().any(|e| e["name"] == "block_reader")),
            "{rip}"
        );
        match decryption {
            None => assert!(rip["decryption"].is_null(), "{rip}"),
            Some(_) => assert_eq!(
                rip["decryption"],
                serde_json::json!({ "css": serde_json::to_value(&css).expect("json") }),
            ),
        }
    }
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
    // What the drive says it is, and how it is attached, travel from the
    // engine's probe through the real worker to the drives table. The fake
    // drive is a file, so its connection is honestly unknown.
    let (vendor, model, connection): (Option<String>, Option<String>, String) =
        sqlx::query_as("SELECT vendor, model, connection FROM drives WHERE id = $1::uuid")
            .bind(drive.as_uuid())
            .fetch_one(&server.pool)
            .await
            .expect("the drive");
    assert_eq!(vendor.as_deref(), Some("Tangible"));
    assert_eq!(model.as_deref(), Some("Fake drive"));
    assert_eq!(connection, "unknown");

    // Reported again on the next start, and it is the same drive.
    let again = runtime
        .announce(&mut identity)
        .await
        .expect("announce again");
    assert_eq!(drive, again);

    server.stop().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_cue_bin_burn_says_what_the_disc_will_not_be() {
    // The whole chain for a disc described as tracks: import a sheet and its
    // data, plan a burn from the topology the manifest carries, and tell the
    // operator what a track descriptor cannot reproduce before the media is
    // consumed rather than after.
    let _queue = exclusive_queue().await;
    let mut server = server().await;
    server.drain_queue().await;

    let artifact = server.import_cue_bin().await;
    let disc = server.seed_disc().await;
    let job = server.queue_burn(&disc, &artifact).await;

    let worker_dir = TempDir::new().expect("temp dir");
    let token = server.enrollment_token().await;
    let settings = settings(&server, worker_dir.path().to_path_buf(), token);
    let engine = FakeEngine::new(settings.state_dir.join("media"));

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
    let _ = worker.await.expect("the worker task");

    assert_eq!(settled["state"], "complete", "{settled}");

    let attempt = settled["attempts"][0]["id"].as_str().expect("an attempt");
    let events = server
        .get(&format!("/api/v1/burn-attempts/{attempt}/events?limit=500"))
        .await;
    let warnings: Vec<String> = events["items"]
        .as_array()
        .expect("events")
        .iter()
        .filter(|event| event["code"] == "LAYOUT_LIMITATION")
        .filter_map(|event| event["data"]["message"].as_str().map(ToOwned::to_owned))
        .collect();

    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("subchannel")),
        "a track descriptor carries no subchannel data, and the operator is \
         the one who needs to know: {warnings:?}"
    );
    assert!(
        warnings.iter().any(|warning| warning.contains("audio")),
        "audio is not verified by comparing bytes: {warnings:?}"
    );

    server.stop().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_engine_that_cannot_write_a_toc_refuses_before_asking_for_media() {
    // The failure this check exists to prevent is not an error message. It is
    // a mixed-mode CD flattened into one data track, which looks like a
    // successful burn and is a ruined disc.
    let _queue = exclusive_queue().await;
    let mut server = server().await;
    server.drain_queue().await;

    let artifact = server.import_cue_bin().await;
    let disc = server.seed_disc().await;
    let job = server.queue_burn(&disc, &artifact).await;

    let worker_dir = TempDir::new().expect("temp dir");
    let token = server.enrollment_token().await;
    let settings = settings(&server, worker_dir.path().to_path_buf(), token);
    let media_root = settings.state_dir.join("media");
    let engine = DataOnlyEngine(FakeEngine::new(&media_root));

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
    let _ = worker.await.expect("the worker task");

    assert_eq!(settled["state"], "failed", "{settled}");
    assert_eq!(
        settled["attempts"][0]["error_code"], "PREFLIGHT_WRITE_MODE_UNSUPPORTED",
        "{settled}"
    );
    assert!(
        settled["attempts"][0]["physical_copy_id"].is_null(),
        "nothing was written, so no disc exists: {settled}"
    );
    assert!(
        !media_root.exists(),
        "the refusal came before anything reached the medium"
    );

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
    assert!(
        !stages.iter().any(|stage| stage == "waiting_for_media"),
        "an operator was asked for a disc that could never have been written: {stages:?}"
    );

    server.stop().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_worker_erases_the_disc_it_is_asked_to_and_only_that() {
    // The whole erase path over HTTP: an administrator asks for the disc in
    // one drive to be erased, the drive's own worker takes it before any burn
    // work, looks at what is in the drive, erases it, and says so.
    let _queue = exclusive_queue().await;
    let mut server = server().await;
    server.drain_queue().await;

    let worker_dir = TempDir::new().expect("temp dir");
    let token = server.enrollment_token().await;
    let settings = settings(&server, worker_dir.path().to_path_buf(), token);
    let engine = || {
        FakeEngine::new(settings.state_dir.join("media")).with_behaviour(FakeBehaviour {
            medium_profile: Some("DVD-RW sequential recording".to_owned()),
            medium_sessions: 1,
            ..FakeBehaviour::default()
        })
    };

    let drive = {
        let mut runtime = WorkerRuntime::new(settings.clone(), engine()).expect("runtime");
        let mut identity = runtime.identify().await.expect("enroll");
        runtime.announce(&mut identity).await.expect("announce")
    };

    let (status, requested) = server
        .post(
            &format!("/api/v1/drives/{drive}/erasures"),
            serde_json::json!({ "mode": "quick", "confirm_data_loss": true }),
        )
        .await;
    assert_eq!(status, 201, "{requested}");
    let erasure = requested["id"].as_str().expect("an erasure").to_owned();

    let (stop, stopped) = tokio::sync::oneshot::channel();
    let engine = engine();
    let worker = tokio::spawn(async move {
        let mut runtime = WorkerRuntime::new(settings, engine).expect("runtime");
        runtime
            .run(async {
                let _ = stopped.await;
            })
            .await
    });

    let mut record = serde_json::Value::Null;
    for _ in 0..200 {
        record = server.get(&format!("/api/v1/erasures/{erasure}")).await;
        if record["is_open"] == false {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = stop.send(());
    let _ = worker.await.expect("the worker task");

    assert_eq!(record["state"], "erased", "{record}");
    assert_eq!(
        record["medium_before"]["profile"],
        "DVD-RW sequential recording"
    );
    assert_eq!(record["medium_before"]["sessions"], 1);

    server.stop().await;
}

/// A CD with something on it, as the fake engine presents one.
fn pressed_cd() -> FakeBehaviour {
    FakeBehaviour {
        medium_profile: Some("CD-ROM".to_owned()),
        medium_sessions: 1,
        ..FakeBehaviour::default()
    }
}

impl Server {
    /// Poll a rip until `done` says it has got far enough.
    async fn rip_until(
        &self,
        rip: &str,
        done: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let mut record = serde_json::Value::Null;
        for _ in 0..200 {
            record = self.get(&format!("/api/v1/rips/{rip}")).await;
            if done(&record) {
                return record;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the rip never got there: {record}");
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_worker_rips_the_disc_it_is_asked_to_into_the_library() {
    // The whole rip path over HTTP: the worker says it can rip, an operator
    // asks, the drive's worker reads the disc, sends both files, and the
    // import turns them into an artifact.
    let _queue = exclusive_queue().await;
    let mut server = server().await;
    server.drain_queue().await;

    let worker_dir = TempDir::new().expect("temp dir");
    let token = server.enrollment_token().await;
    let settings = settings(&server, worker_dir.path().to_path_buf(), token);
    let engine = || FakeEngine::new(settings.state_dir.join("media")).with_behaviour(pressed_cd());
    let drive = {
        let mut runtime = WorkerRuntime::new(settings.clone(), engine()).expect("runtime");
        let mut identity = runtime.identify().await.expect("enroll");
        runtime.announce(&mut identity).await.expect("announce")
    };

    let (status, requested) = server
        .post(
            &format!("/api/v1/drives/{drive}/rips"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(status, 201, "the worker said it can rip: {requested}");
    let rip = requested["id"].as_str().expect("a rip").to_owned();

    let (stop, stopped) = tokio::sync::oneshot::channel();
    let engine = engine();
    let worker = tokio::spawn(async move {
        let mut runtime = WorkerRuntime::new(settings, engine).expect("runtime");
        runtime
            .run(async {
                let _ = stopped.await;
            })
            .await
    });
    let sent = server
        .rip_until(&rip, |record| {
            record["state"] != "queued" && record["can_cancel"] == false
        })
        .await;
    let _ = stop.send(());
    let _ = worker.await.expect("the worker task");
    assert_eq!(sent["state"], "registering", "{sent}");
    assert_eq!(sent["engine"], "fake");
    assert_eq!(sent["medium"]["profile"], "CD-ROM");
    assert_eq!(sent["files"].as_array().unwrap().len(), 2);

    let runner = ImportRunner::new(
        server.database.clone(),
        server.pipeline.clone(),
        ImportRunnerSettings {
            owner: format!("test-{}", uuid::Uuid::now_v7()),
            ..ImportRunnerSettings::default()
        },
    );
    for _ in 0..10 {
        if !runner.step().await.expect("queue") {
            break;
        }
    }
    let done = server.get(&format!("/api/v1/rips/{rip}")).await;
    assert_eq!(done["state"], "complete", "{done}");
    let artifact = done["artifact_id"].as_str().expect("an artifact");
    let detail = server.get(&format!("/api/v1/artifacts/{artifact}")).await;
    assert_eq!(detail["format"], "toc_bin", "{detail}");
    assert_eq!(detail["disc"]["tracks"].as_array().unwrap().len(), 2);
    assert_eq!(detail["ripped"]["rip_id"], rip, "{detail}");
    assert_eq!(detail["ripped"]["encryption"], "none");
    assert_eq!(detail["ripped"]["decrypted_at_acquisition"], false);
    assert!(detail["ripped"]["decryption"].is_null(), "{detail}");
    let events = server.get(&format!("/api/v1/rips/{rip}/events")).await;
    assert!(
        events["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["code"] == "RIP_STARTED"),
        "{events}"
    );

    server.stop().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_withdrawn_rip_stops_the_worker_reading_and_nothing_is_sent() {
    let _queue = exclusive_queue().await;
    let mut server = server().await;
    server.drain_queue().await;

    let worker_dir = TempDir::new().expect("temp dir");
    let token = server.enrollment_token().await;
    let settings = settings(&server, worker_dir.path().to_path_buf(), token);
    let engine = || {
        FakeEngine::new(settings.state_dir.join("media")).with_behaviour(FakeBehaviour {
            // Long enough to be withdrawn partway: 100 steps of 50 ms.
            rip_steps: 100,
            ..pressed_cd()
        })
    };
    let drive = {
        let mut runtime = WorkerRuntime::new(settings.clone(), engine()).expect("runtime");
        let mut identity = runtime.identify().await.expect("enroll");
        runtime.announce(&mut identity).await.expect("announce")
    };
    let (_, requested) = server
        .post(
            &format!("/api/v1/drives/{drive}/rips"),
            serde_json::json!({}),
        )
        .await;
    let rip = requested["id"].as_str().expect("a rip").to_owned();

    let (stop, stopped) = tokio::sync::oneshot::channel();
    let engine = engine();
    let state_dir = settings.state_dir.clone();
    let worker = tokio::spawn(async move {
        let mut runtime = WorkerRuntime::new(settings, engine).expect("runtime");
        runtime
            .run(async {
                let _ = stopped.await;
            })
            .await
    });
    server
        .rip_until(&rip, |record| record["state"] == "reading")
        .await;
    let (status, canceled) = server
        .post(&format!("/api/v1/rips/{rip}/cancel"), serde_json::json!({}))
        .await;
    assert_eq!(status, 200, "{canceled}");

    // The worker hears of it at its next report, stops reading, and tidies
    // up: nothing of the rip is left on its disk.
    let rip_dir = state_dir.join("rips").join(&rip);
    for _ in 0..100 {
        if !rip_dir.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!rip_dir.exists(), "the worker removed what it had read");
    let _ = stop.send(());
    let _ = worker.await.expect("the worker task");

    let record = server.get(&format!("/api/v1/rips/{rip}")).await;
    assert_eq!(record["state"], "canceled");
    assert_eq!(record["bytes_received"], 0, "nothing was sent");

    server.stop().await;
}

/// Run a worker with `behaviour` until the rip it is asked for has ended or
/// been sent, and answer the rip as the server last saw it.
async fn rip_with(server: &Server, behaviour: FakeBehaviour) -> serde_json::Value {
    rip_asking(server, behaviour, serde_json::json!({}), None).await
}

/// As [`rip_with`], asking for the rip with `request`, on a worker that
/// decrypts with `copier` when there is one.
async fn rip_asking(
    server: &Server,
    behaviour: FakeBehaviour,
    request: serde_json::Value,
    copier: Option<tangible_burn::fake::FakeCssCopier>,
) -> serde_json::Value {
    let worker_dir = TempDir::new().expect("temp dir");
    let token = server.enrollment_token().await;
    let mut settings = settings(server, worker_dir.path().to_path_buf(), token);
    if copier.is_some() {
        settings.decryption = Some(tangible_burn::decryption::DecryptionCapability {
            css: Some(tangible_burn::decryption::CssDecryption {
                library: "libdvdcss".to_owned(),
                version: "1.6.0".to_owned(),
                tool: "dvdbackup".to_owned(),
                tool_version: "0.4.2-4.1".to_owned(),
            }),
        });
    }
    let runtime = |settings: WorkerSettings| {
        let engine =
            FakeEngine::new(settings.state_dir.join("media")).with_behaviour(behaviour.clone());
        let runtime = WorkerRuntime::new(settings, engine).expect("runtime");
        match &copier {
            Some(copier) => runtime.with_css_copier(std::sync::Arc::new(copier.clone())),
            None => runtime,
        }
    };
    let drive = {
        let mut runtime = runtime(settings.clone());
        let mut identity = runtime.identify().await.expect("enroll");
        runtime.announce(&mut identity).await.expect("announce")
    };
    let (status, requested) = server
        .post(&format!("/api/v1/drives/{drive}/rips"), request)
        .await;
    assert_eq!(status, 201, "{requested}");
    let rip = requested["id"].as_str().expect("a rip").to_owned();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let mut runtime = runtime(settings);
    let worker = tokio::spawn(async move {
        runtime
            .run(async {
                let _ = stopped.await;
            })
            .await
    });
    let record = server
        .rip_until(&rip, |record| {
            record["state"] != "queued" && record["can_cancel"] == false
        })
        .await;
    let _ = stop.send(());
    let _ = worker.await.expect("the worker task");
    record
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_worker_rips_a_dvd_into_one_image() {
    let _queue = exclusive_queue().await;
    let mut server = server().await;
    server.drain_queue().await;
    let sent = rip_with(
        &server,
        FakeBehaviour {
            medium_profile: Some("DVD-ROM".to_owned()),
            medium_sessions: 1,
            ..FakeBehaviour::default()
        },
    )
    .await;
    assert_eq!(sent["state"], "registering", "{sent}");
    assert_eq!(sent["files"][0]["name"], "disc.iso");

    let runner = ImportRunner::new(
        server.database.clone(),
        server.pipeline.clone(),
        ImportRunnerSettings {
            owner: format!("test-{}", uuid::Uuid::now_v7()),
            ..ImportRunnerSettings::default()
        },
    );
    for _ in 0..10 {
        if !runner.step().await.expect("queue") {
            break;
        }
    }
    let rip = sent["id"].as_str().unwrap();
    let done = server.get(&format!("/api/v1/rips/{rip}")).await;
    assert_eq!(done["state"], "complete", "{done}");
    let artifact = done["artifact_id"].as_str().expect("an artifact");
    let detail = server.get(&format!("/api/v1/artifacts/{artifact}")).await;
    assert_eq!(detail["format"], "iso", "{detail}");
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_encrypted_dvd_is_refused_and_nothing_is_sent() {
    let _queue = exclusive_queue().await;
    let mut server = server().await;
    server.drain_queue().await;
    let ended = rip_with(
        &server,
        FakeBehaviour {
            medium_profile: Some("DVD-ROM".to_owned()),
            medium_sessions: 1,
            rip_encrypted: true,
            ..FakeBehaviour::default()
        },
    )
    .await;
    assert_eq!(ended["state"], "refused", "{ended}");
    assert_eq!(ended["error_code"], "ENCRYPTED");
    assert_eq!(ended["bytes_received"], 0);
    assert_eq!(ended["medium"]["profile"], "DVD-ROM");
    server.stop().await;
}

/// A CSS-encrypted DVD, as the fake engine presents one.
fn encrypted_dvd() -> FakeBehaviour {
    FakeBehaviour {
        medium_profile: Some("DVD-ROM".to_owned()),
        medium_sessions: 1,
        rip_encrypted: true,
        ..FakeBehaviour::default()
    }
}

impl Server {
    /// Run the import queue until it has nothing left.
    async fn run_imports(&self) {
        let runner = ImportRunner::new(
            self.database.clone(),
            self.pipeline.clone(),
            ImportRunnerSettings {
                owner: format!("test-{}", uuid::Uuid::now_v7()),
                ..ImportRunnerSettings::default()
            },
        );
        for _ in 0..10 {
            if !runner.step().await.expect("queue") {
                break;
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_worker_decrypts_a_dvd_keeping_the_encrypted_original_and_a_copy() {
    let _queue = exclusive_queue().await;
    let mut server = server_with(true).await;
    server.drain_queue().await;
    let sent = rip_asking(
        &server,
        encrypted_dvd(),
        serde_json::json!({ "decrypt": true }),
        Some(tangible_burn::fake::FakeCssCopier::default()),
    )
    .await;
    assert_eq!(sent["state"], "registering", "{sent}");
    let names: Vec<&str> = sent["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"disc.iso"), "{names:?}");
    assert!(
        names.contains(&"decrypted/VIDEO_TS/VTS_01_1.VOB"),
        "sent under its full name: {names:?}"
    );
    assert_eq!(sent["encryption"], "css");
    assert_eq!(sent["decryption"]["library"], "libdvdcss");
    assert_eq!(sent["keep"], "encrypted_original");

    server.run_imports().await;
    let rip = sent["id"].as_str().unwrap();
    let done = server.get(&format!("/api/v1/rips/{rip}")).await;
    assert_eq!(done["state"], "complete", "{done}");
    let original = done["artifact_id"].as_str().expect("the original");
    let detail = server.get(&format!("/api/v1/artifacts/{original}")).await;
    assert_eq!(detail["format"], "iso", "{detail}");
    let copy = done["copy_artifact_id"].as_str().expect("the copy");
    let copied = server.get(&format!("/api/v1/artifacts/{copy}")).await;
    assert_eq!(copied["format"], "video_ts_directory", "{copied}");
    // The original is the disc as it is; only the copy says it was decrypted.
    assert_eq!(detail["ripped"]["encryption"], "css", "{detail}");
    assert_eq!(detail["ripped"]["decrypted_at_acquisition"], false);
    assert!(detail["ripped"]["decryption"].is_null(), "{detail}");
    assert_eq!(
        copied["ripped"]["decrypted_at_acquisition"], true,
        "{copied}"
    );
    assert_eq!(copied["ripped"]["decryption"]["library"], "libdvdcss");
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_worker_decrypts_a_dvd_into_a_decrypted_original() {
    let _queue = exclusive_queue().await;
    let mut server = server_with(true).await;
    server.drain_queue().await;
    let sent = rip_asking(
        &server,
        encrypted_dvd(),
        serde_json::json!({ "decrypt": true, "keep": "decrypted_original" }),
        Some(tangible_burn::fake::FakeCssCopier::default()),
    )
    .await;
    assert_eq!(sent["state"], "registering", "{sent}");
    assert_eq!(sent["engine"], "dvdbackup");
    assert!(
        sent["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|file| file["name"].as_str().unwrap().starts_with("VIDEO_TS/")),
        "{sent}"
    );
    server.run_imports().await;
    let rip = sent["id"].as_str().unwrap();
    let done = server.get(&format!("/api/v1/rips/{rip}")).await;
    assert_eq!(done["state"], "complete", "{done}");
    assert!(done["copy_artifact_id"].is_null(), "{done}");
    let original = done["artifact_id"].as_str().expect("the original");
    let detail = server.get(&format!("/api/v1/artifacts/{original}")).await;
    assert_eq!(detail["format"], "video_ts_directory", "{detail}");
    assert_eq!(detail["ripped"]["engine"], "dvdbackup", "{detail}");
    assert_eq!(detail["ripped"]["decrypted_at_acquisition"], true);
    assert_eq!(detail["ripped"]["decryption"]["system"], "css");
    server.stop().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_dvd_that_will_not_decrypt_fails_and_nothing_is_sent() {
    let _queue = exclusive_queue().await;
    let mut server = server_with(true).await;
    server.drain_queue().await;
    let ended = rip_asking(
        &server,
        encrypted_dvd(),
        serde_json::json!({ "decrypt": true }),
        Some(tangible_burn::fake::FakeCssCopier {
            authenticate_fails: true,
            ..tangible_burn::fake::FakeCssCopier::default()
        }),
    )
    .await;
    assert_eq!(ended["state"], "failed", "{ended}");
    assert_eq!(ended["error_code"], "DECRYPTION_FAILED");
    assert_eq!(ended["bytes_received"], 0);
    server.stop().await;
}
