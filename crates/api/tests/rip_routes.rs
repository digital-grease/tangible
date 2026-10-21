// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Ripping the disc in a drive, through the real router, PostgreSQL and the
//! import runner: from an operator's request to an artifact in the library.
//!
//! What matters spans all of it: only the drive's own worker takes and feeds
//! a rip, the server trusts no digest it did not compute, a withdrawn rip
//! stops its worker and leaves nothing behind, and what was read becomes an
//! ordinary import.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use sha2::Digest as _;
use sqlx::PgPool;
use tangible_api::{
    ApiState, ImportContext, ImportPipeline, ImportRunner, ImportRunnerSettings, router,
};
use tangible_domain::Role;
use tangible_storage::{FilesystemStore, ManifestStore, StagingManager, WatchRoots};
use tempfile::TempDir;
use tower::ServiceExt as _;

mod support;

/// The import queue is global, so imports here are run one test at a time.
static QUEUE: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

/// What cdrdao writes when it reads a two-track disc, named as a rip names
/// it: a data track and an audio track in one file.
const TOC: &[u8] = b"CD_ROM\n\
TRACK MODE1\n\
DATAFILE \"disc.bin\" 00:01:00 // length in bytes: 153600\n\
TRACK AUDIO\n\
SILENCE 00:00:02\n\
FILE \"disc.bin\" #153600 0 00:00:25\n\
START 00:00:02\n";

fn bin() -> Vec<u8> {
    vec![0_u8; 75 * 2048 + 25 * 2352]
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

struct Harness {
    router: axum::Router,
    runner: ImportRunner,
    pool: PgPool,
    operator: support::Session,
    staging: std::path::PathBuf,
    _dir: TempDir,
    _queue: tokio::sync::MutexGuard<'static, ()>,
}

enum As<'a> {
    Person(&'a support::Session),
    Worker(&'a str),
    Nobody,
}

struct Worker {
    id: String,
    credential: String,
    drive: String,
}

async fn harness() -> Harness {
    harness_with(false).await
}

/// A harness whose server does or does not allow decryption.
async fn harness_with(decryption_allowed: bool) -> Harness {
    let queue = QUEUE
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let database = support::database().await;
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
    let state = ApiState::with_manifests(database.clone(), manifests)
        .with_imports(ImportContext {
            pipeline: pipeline.clone(),
            roots: WatchRoots::new(BTreeMap::new()),
            max_upload_bytes: 64 * 1024 * 1024,
        })
        .with_decryption_allowed(decryption_allowed);
    let runner = ImportRunner::new(
        database,
        pipeline,
        ImportRunnerSettings {
            owner: format!("test-{}", uuid::Uuid::now_v7()),
            ..ImportRunnerSettings::default()
        },
    );
    // Claimable imports left by earlier runs point at staging directories
    // that no longer exist.
    sqlx::query(
        "UPDATE import_jobs SET state = 'canceled', lease_owner = NULL, lease_expires_at = NULL
         WHERE state IN ('requested', 'acquiring', 'staged', 'hashing', 'inspecting',
                         'registering', 'failed_retryable')",
    )
    .execute(&pool)
    .await
    .expect("drain the import queue");
    let operator = support::sign_in(&pool, Role::Operator).await;
    Harness {
        router: router(state),
        runner,
        pool,
        operator,
        staging: dir.path().join("staging"),
        _dir: dir,
        _queue: queue,
    }
}

impl Harness {
    async fn send(
        &self,
        method: &str,
        uri: &str,
        who: &As<'_>,
        body: Body,
        headers: &[(&str, String)],
    ) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(uri);
        match who {
            As::Person(session) => {
                request = request
                    .header("cookie", session.cookie())
                    .header("x-csrf-token", &session.csrf_token);
            }
            As::Worker(credential) => {
                request = request.header("authorization", format!("Bearer {credential}"));
            }
            As::Nobody => {}
        }
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        let response = self
            .router
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 16 << 20)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (status, value)
    }

    async fn json(
        &self,
        method: &str,
        uri: &str,
        who: &As<'_>,
        body: Value,
    ) -> (StatusCode, Value) {
        self.send(
            method,
            uri,
            who,
            Body::from(body.to_string()),
            &[("content-type", "application/json".to_owned())],
        )
        .await
    }

    async fn get(&self, uri: &str) -> (StatusCode, Value) {
        self.send("GET", uri, &As::Person(&self.operator), Body::empty(), &[])
            .await
    }

    /// An enrolled worker with one drive, which says it can rip unless told
    /// otherwise.
    async fn worker(&self, can_rip: bool) -> Worker {
        let secret = format!("tgw_enroll_{}", uuid::Uuid::now_v7().simple());
        sqlx::query(
            "INSERT INTO worker_enrollments (id, token_hash, expires_at, created_by)
             VALUES ($1, $2, now() + interval '1 hour', 'test')",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(sha256(secret.as_bytes()))
        .execute(&self.pool)
        .await
        .unwrap();
        let (status, body) = self
            .json(
                "POST",
                "/api/v1/worker-enrollments/consume",
                &As::Nobody,
                json!({
                    "enrollment_token": secret,
                    "name": format!("rip-{}", uuid::Uuid::now_v7()),
                    "protocol_versions": ["1alpha1"],
                    "software_version": "0.1.0",
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let id = body["worker_id"].as_str().unwrap().to_owned();
        let drive = uuid::Uuid::now_v7();
        sqlx::query(
            "INSERT INTO drives (id, worker_id, configured_name, device_alias)
             VALUES ($1, $2::uuid, 'sr0', '/dev/sr0')",
        )
        .bind(drive)
        .bind(&id)
        .execute(&self.pool)
        .await
        .unwrap();
        if can_rip {
            sqlx::query(
                "UPDATE workers SET capabilities_json =
                     '{\"rip\": {\"engines\": [{\"name\": \"cdrdao\", \"version\": \"1.2.4\"}]}}'
                 WHERE id = $1::uuid",
            )
            .bind(&id)
            .execute(&self.pool)
            .await
            .unwrap();
        }
        Worker {
            id,
            credential: body["credential"].as_str().unwrap().to_owned(),
            drive: drive.to_string(),
        }
    }

    async fn request(&self, drive: &str) -> (StatusCode, Value) {
        self.json(
            "POST",
            &format!("/api/v1/drives/{drive}/rips"),
            &As::Person(&self.operator),
            json!({}),
        )
        .await
    }

    async fn claim(&self, worker: &Worker) -> (StatusCode, Value) {
        self.json(
            "POST",
            &format!("/api/v1/workers/{}/rip-claims", worker.id),
            &As::Worker(&worker.credential),
            json!({ "drive_id": worker.drive }),
        )
        .await
    }

    async fn upload(
        &self,
        worker: &Worker,
        rip: &str,
        name: &str,
        bytes: &[u8],
        claimed: &str,
    ) -> (StatusCode, Value) {
        self.send(
            "PUT",
            &format!("/api/v1/rips/{rip}/files/{name}"),
            &As::Worker(&worker.credential),
            Body::from(bytes.to_vec()),
            &[
                ("content-length", bytes.len().to_string()),
                ("x-content-sha256", claimed.to_owned()),
            ],
        )
        .await
    }

    async fn complete(&self, worker: &Worker, rip: &str, body: Value) -> (StatusCode, Value) {
        self.json(
            "POST",
            &format!("/api/v1/rips/{rip}/complete"),
            &As::Worker(&worker.credential),
            body,
        )
        .await
    }

    async fn event(&self, worker: &Worker, rip: &str, sequence: i64) -> (StatusCode, Value) {
        self.json(
            "POST",
            &format!("/api/v1/rips/{rip}/events"),
            &As::Worker(&worker.credential),
            json!({ "events": [{
                "sequence": sequence,
                "event_type": "progress",
                "code": "READING",
                "progress": 0.5,
                "data": { "message": "read 50%" },
                "worker_time": "2026-10-19T12:00:00Z",
            }]}),
        )
        .await
    }

    /// A rip requested and taken by `worker`.
    async fn taken(&self, worker: &Worker) -> String {
        let (status, body) = self.request(&worker.drive).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (status, claim) = self.claim(worker).await;
        assert_eq!(status, StatusCode::OK, "{claim}");
        claim["rip_id"].as_str().unwrap().to_owned()
    }

    async fn drain(&self) {
        for _ in 0..20 {
            if !self.runner.step().await.expect("queue") {
                return;
            }
        }
    }

    fn staged_files(&self) -> usize {
        walk(&self.staging)
    }
}

fn walk(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir).map_or(0, |entries| {
        entries
            .flatten()
            .map(|entry| {
                let path = entry.path();
                if path.is_dir() { walk(&path) } else { 1 }
            })
            .sum()
    })
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_rip_becomes_an_artifact_in_the_library() {
    let harness = harness().await;
    let worker = harness.worker(true).await;

    let (status, queued) = harness.request(&worker.drive).await;
    assert_eq!(status, StatusCode::CREATED, "{queued}");
    assert_eq!(queued["state"], "queued");
    assert_eq!(queued["can_cancel"], true);

    let (status, claim) = harness.claim(&worker).await;
    assert_eq!(status, StatusCode::OK, "{claim}");
    assert_eq!(
        claim["outputs"],
        json!(["disc.toc", "disc.bin", "disc.iso"])
    );
    let rip = claim["rip_id"].as_str().unwrap().to_owned();
    assert_eq!(rip, queued["id"]);

    let (status, ack) = harness.event(&worker, &rip, 0).await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!(ack["state"], "reading");

    let disc = bin();
    for (name, bytes) in [("disc.toc", TOC.to_vec()), ("disc.bin", disc.clone())] {
        let (status, body) = harness
            .upload(&worker, &rip, name, &bytes, &sha256(&bytes))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    let (status, done) = harness
        .complete(
            &worker,
            &rip,
            json!({
                "outcome": "read",
                "engine": "cdrdao",
                "engine_version": "1.2.4",
                "read_errors": { "unreadable_sectors": 0, "retried_sectors": 2 },
                "outputs": ["disc.toc", "disc.bin"],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["state"], "registering");
    assert_eq!(done["files"].as_array().unwrap().len(), 2);
    assert_eq!(done["bytes_received"], TOC.len() + disc.len());

    harness.drain().await;
    let (_, finished) = harness.get(&format!("/api/v1/rips/{rip}")).await;
    assert_eq!(finished["state"], "complete", "{finished}");
    assert_eq!(finished["read_errors"]["retried_sectors"], 2);
    let artifact = finished["artifact_id"].as_str().expect("an artifact");
    let (status, detail) = harness.get(&format!("/api/v1/artifacts/{artifact}")).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["format"], "toc_bin");
    assert_eq!(detail["source_kind"], "physical_drive");
    assert_eq!(detail["disc"]["tracks"].as_array().unwrap().len(), 2);

    let (_, events) = harness.get(&format!("/api/v1/rips/{rip}/events")).await;
    assert_eq!(events["items"][0]["message"], "read 50%");
    let (_, listed) = harness.get("/api/v1/rips?limit=200").await;
    assert!(
        listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == rip.as_str())
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_server_trusts_no_digest_it_did_not_compute() {
    let harness = harness().await;
    let worker = harness.worker(true).await;
    let rip = harness.taken(&worker).await;
    let before = harness.staged_files();

    let (status, body) = harness
        .upload(&worker, &rip, "disc.toc", TOC, &sha256(b"something else"))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .starts_with("UPLOAD_MISMATCH")
    );
    assert_eq!(
        harness.staged_files(),
        before,
        "the mismatched file is gone"
    );

    for (name, digest) in [
        ("disc.img", sha256(TOC)),
        ("../disc.toc", sha256(TOC)),
        ("disc.toc", "not-hex".to_owned()),
    ] {
        let (status, _) = harness.upload(&worker, &rip, name, TOC, &digest).await;
        assert!(
            matches!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY | StatusCode::NOT_FOUND | StatusCode::BAD_REQUEST
            ),
            "{name}: {status}"
        );
    }

    // Completion must name exactly what arrived.
    let (status, body) = harness
        .complete(
            &worker,
            &rip,
            json!({ "outcome": "read", "engine": "cdrdao", "engine_version": "1.2.4",
                    "outputs": ["disc.toc", "disc.bin"] }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn only_the_drives_own_worker_feeds_its_rip() {
    let harness = harness().await;
    let worker = harness.worker(true).await;
    let stranger = harness.worker(true).await;
    let rip = harness.taken(&worker).await;

    let (status, _) = harness
        .upload(&stranger, &rip, "disc.toc", TOC, &sha256(TOC))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = harness.event(&stranger, &rip, 0).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = harness
        .json(
            "POST",
            &format!("/api/v1/workers/{}/rip-claims", stranger.id),
            &As::Worker(&stranger.credential),
            json!({ "drive_id": worker.drive }),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_withdrawn_rip_stops_its_worker_and_leaves_nothing_behind() {
    let harness = harness().await;
    let worker = harness.worker(true).await;
    let rip = harness.taken(&worker).await;
    let before = harness.staged_files();
    let (status, _) = harness
        .upload(&worker, &rip, "disc.toc", TOC, &sha256(TOC))
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, canceled) = harness
        .send(
            "POST",
            &format!("/api/v1/rips/{rip}/cancel"),
            &As::Person(&harness.operator),
            Body::empty(),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{canceled}");
    assert_eq!(canceled["state"], "canceled");
    assert_eq!(harness.staged_files(), before, "what was sent is discarded");

    let (status, _) = harness.event(&worker, &rip, 1).await;
    assert_eq!(status, StatusCode::CONFLICT, "the worker learns to stop");
    let (status, _) = harness
        .upload(&worker, &rip, "disc.bin", &bin(), &sha256(&bin()))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_refused_rip_frees_the_drive() {
    let harness = harness().await;
    let worker = harness.worker(true).await;
    let rip = harness.taken(&worker).await;
    let (status, refused) = harness
        .complete(
            &worker,
            &rip,
            json!({ "outcome": "refused", "error_code": "NO_MEDIUM",
                    "error_detail": "the drive is empty" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{refused}");
    assert_eq!(refused["state"], "refused");
    assert_eq!(refused["error_code"], "NO_MEDIUM");
    let (status, _) = harness.request(&worker.drive).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _) = harness
        .complete(&worker, &rip, json!({ "outcome": "refused" }))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a code is required"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn asking_needs_an_operator_a_drive_and_a_worker_that_can_rip() {
    let harness = harness().await;
    let old = harness.worker(false).await;
    let (status, body) = harness.request(&old.drive).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("has not said it can rip")
    );

    let (status, _) = harness.request(&uuid::Uuid::now_v7().to_string()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let worker = harness.worker(true).await;
    let viewer = support::sign_in(&harness.pool, Role::Viewer).await;
    let (status, _) = harness
        .json(
            "POST",
            &format!("/api/v1/drives/{}/rips", worker.drive),
            &As::Person(&viewer),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, body) = harness.request(&worker.drive).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, _) = harness.request(&worker.drive).await;
    assert_eq!(status, StatusCode::CONFLICT, "one at a time");
}

/// A structurally valid ISO 9660 image of `blocks` 2048-byte blocks.
fn iso_image(blocks: u32) -> Vec<u8> {
    let sector = 2048;
    let mut bytes = vec![0_u8; 16 * sector];
    let mut pvd = vec![0_u8; sector];
    pvd[0] = 1;
    pvd[1..6].copy_from_slice(b"CD001");
    pvd[6] = 1;
    for slot in &mut pvd[40..72] {
        *slot = b' ';
    }
    pvd[40..43].copy_from_slice(b"RIP");
    pvd[80..84].copy_from_slice(&blocks.to_le_bytes());
    pvd[84..88].copy_from_slice(&blocks.to_be_bytes());
    pvd[128..130].copy_from_slice(&2048_u16.to_le_bytes());
    pvd[130..132].copy_from_slice(&2048_u16.to_be_bytes());
    bytes.extend_from_slice(&pvd);
    let mut terminator = vec![0_u8; sector];
    terminator[0] = 255;
    terminator[1..6].copy_from_slice(b"CD001");
    terminator[6] = 1;
    bytes.extend_from_slice(&terminator);
    bytes.resize(blocks as usize * sector, 0);
    bytes
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_dvd_rip_is_one_image_and_becomes_an_iso() {
    let harness = harness().await;
    let worker = harness.worker(true).await;
    let rip = harness.taken(&worker).await;
    let image = iso_image(64);
    let (status, body) = harness
        .upload(&worker, &rip, "disc.iso", &image, &sha256(&image))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, done) = harness
        .complete(
            &worker,
            &rip,
            json!({ "outcome": "read", "engine": "block_reader", "engine_version": "0.1.0",
                    "outputs": ["disc.iso"] }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    harness.drain().await;
    let (_, finished) = harness.get(&format!("/api/v1/rips/{rip}")).await;
    assert_eq!(finished["state"], "complete", "{finished}");
    let artifact = finished["artifact_id"].as_str().expect("an artifact");
    let (_, detail) = harness.get(&format!("/api/v1/artifacts/{artifact}")).await;
    assert_eq!(detail["format"], "iso", "{detail}");
    assert_eq!(detail["source_kind"], "physical_drive");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_rip_is_never_a_mixture_or_a_part() {
    let harness = harness().await;
    let worker = harness.worker(true).await;
    let rip = harness.taken(&worker).await;
    for (name, bytes) in [("disc.toc", TOC.to_vec()), ("disc.iso", iso_image(32))] {
        let (status, body) = harness
            .upload(&worker, &rip, name, &bytes, &sha256(&bytes))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    for outputs in [json!(["disc.toc", "disc.iso"]), json!(["disc.toc"])] {
        let (status, body) = harness
            .complete(
                &worker,
                &rip,
                json!({ "outcome": "read", "engine": "x", "engine_version": "1",
                        "outputs": outputs }),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }
}

// --- decryption ------------------------------------------------------------------

impl Harness {
    /// Say a worker can decrypt DVDs, as one from the opt-in image does.
    async fn can_decrypt(&self, worker: &Worker) {
        sqlx::query(
            "UPDATE workers SET capabilities_json = '{\"rip\": {
                 \"engines\": [{\"name\": \"block_reader\", \"version\": \"0.1.0\"}],
                 \"decryption\": {\"css\": {\"library\": \"libdvdcss\", \"version\": \"1.6.0\",
                                              \"tool\": \"dvdbackup\", \"tool_version\": \"0.4.2-4.1\"}}}}'
             WHERE id = $1::uuid",
        )
        .bind(&worker.id)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn request_with(&self, drive: &str, body: Value) -> (StatusCode, Value) {
        self.json(
            "POST",
            &format!("/api/v1/drives/{drive}/rips"),
            &As::Person(&self.operator),
            body,
        )
        .await
    }

    async fn drive_view(&self, drive: &str) -> Value {
        let (status, body) = self.get("/api/v1/drives").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == drive)
            .cloned()
            .expect("the drive is listed")
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn decryption_is_refused_while_the_server_does_not_allow_it() {
    let harness = harness().await;
    let worker = harness.worker(true).await;
    harness.can_decrypt(&worker).await;

    let (status, body) = harness
        .request_with(&worker.drive, json!({ "decrypt": true }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "DECRYPTION_OFF");
    let (status, _) = harness.claim(&worker).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "nothing was queued");

    // The drive list says so, so the web UI does not offer it.
    let view = harness.drive_view(&worker.drive).await;
    assert_eq!(view["can_rip"], true);
    assert_eq!(view["can_decrypt_rips"], false);

    // Without decryption, the same drive rips as before; a choice of
    // original sent anyway is ignored.
    let (status, body) = harness
        .request_with(&worker.drive, json!({ "keep": "decrypted_original" }))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["decrypt"], false);
    assert!(body["keep"].is_null(), "{body}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn decryption_needs_a_worker_that_can() {
    let harness = harness_with(true).await;
    let worker = harness.worker(true).await;

    let (status, body) = harness
        .request_with(&worker.drive, json!({ "decrypt": true }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "ENCRYPTED_NO_DECRYPTION");
    let view = harness.drive_view(&worker.drive).await;
    assert_eq!(view["can_decrypt_rips"], false);

    // A worker that cannot rip at all is still told that first.
    let old = harness.worker(false).await;
    let (status, body) = harness
        .request_with(&old.drive, json!({ "decrypt": true }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "CONFLICT");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_decrypting_rip_carries_its_choice_to_the_worker() {
    let harness = harness_with(true).await;
    let worker = harness.worker(true).await;
    harness.can_decrypt(&worker).await;
    assert_eq!(
        harness.drive_view(&worker.drive).await["can_decrypt_rips"],
        true
    );

    let (status, body) = harness
        .request_with(
            &worker.drive,
            json!({ "decrypt": true, "keep": "sideways" }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "VALIDATION_FAILED");

    // The encrypted original is the default.
    let (status, body) = harness
        .request_with(&worker.drive, json!({ "decrypt": true }))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["decrypt"], true);
    assert_eq!(body["keep"], "encrypted_original");

    let (status, claim) = harness.claim(&worker).await;
    assert_eq!(status, StatusCode::OK, "{claim}");
    assert_eq!(claim["rip_id"], body["id"]);
    assert_eq!(claim["decrypt"], true);
    assert_eq!(claim["keep"], "encrypted_original");

    // Asking was audited with the choice.
    let metadata: Value = sqlx::query_scalar(
        "SELECT metadata FROM audit_events
         WHERE action = 'rip.requested' AND target_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(body["id"].as_str().unwrap())
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(metadata["decrypt"], true);
    assert_eq!(metadata["keep"], "encrypted_original");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_plain_rip_tells_the_worker_not_to_decrypt() {
    let harness = harness_with(true).await;
    let worker = harness.worker(true).await;
    harness.can_decrypt(&worker).await;
    let (status, body) = harness.request(&worker.drive).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, claim) = harness.claim(&worker).await;
    assert_eq!(status, StatusCode::OK, "{claim}");
    assert_eq!(claim["decrypt"], false);
    assert!(claim["keep"].is_null(), "{claim}");
}
