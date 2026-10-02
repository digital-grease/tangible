// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Getting bytes into the library, through the real router and the real
//! background runner.
//!
//! Two paths in (an upload and a watched folder) and the same destination:
//! an artifact in the object store, a manifest, catalog rows, and a job that
//! says what happened. The runner is driven a step at a time rather than as a
//! loop, so each test asserts on a settled state instead of racing one.
//!
//! The refusals matter as much as the happy paths. A watched folder is the one
//! place an untrusted string reaches the filesystem, so the tests that try to
//! escape it are the point of the file.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use sqlx::PgPool;
use tangible_api::{
    ApiState, ImportContext, ImportPipeline, ImportRunner, ImportRunnerSettings, router,
};
use tangible_db::{Database, DbConfig};
use tangible_storage::{FilesystemStore, ManifestStore, StagingManager, WatchRoots};
use tempfile::TempDir;
use tower::ServiceExt as _;

mod support;

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

/// Serialises the file.
///
/// The import queue is global by design (any server process may take any
/// job), so two harnesses running at once would claim each other's imports
/// and fail them against the wrong staging directory.
static QUEUE: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

async fn exclusive_queue() -> tokio::sync::MutexGuard<'static, ()> {
    QUEUE
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

struct Harness {
    session: support::Session,
    router: axum::Router,
    runner: ImportRunner,
    pool: PgPool,
    watched: PathBuf,
    dir: TempDir,
    _queue: tokio::sync::MutexGuard<'static, ()>,
}

async fn harness() -> Harness {
    harness_with_limit(64 * 1024 * 1024).await
}

async fn harness_with_limit(max_upload_bytes: u64) -> Harness {
    let queue = exclusive_queue().await;
    let url = std::env::var("TANGIBLE_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TANGIBLE_DATABASE_URL"))
        .expect("set TANGIBLE_TEST_DATABASE_URL to run integration tests");
    let database = Database::connect(&DbConfig::new(url))
        .await
        .expect("connect");
    database.migrate().await.expect("migrate");
    let pool = database.pool().clone();

    let dir = TempDir::new().expect("temp dir");
    let watched = dir.path().join("incoming");
    std::fs::create_dir_all(&watched).expect("watched root");

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

    let mut roots = BTreeMap::new();
    roots.insert("incoming".to_owned(), watched.clone());
    let roots = WatchRoots::new(roots);

    let state = ApiState::with_manifests(database.clone(), manifests).with_imports(ImportContext {
        pipeline: pipeline.clone(),
        roots: roots.clone(),
        max_upload_bytes,
    });

    let runner = ImportRunner::new(
        database,
        pipeline,
        ImportRunnerSettings {
            // Unique per harness, so two tests running at once never claim
            // each other's imports through a shared lease name.
            owner: format!("test-{}", uuid::Uuid::now_v7()),
            roots,
            ..ImportRunnerSettings::default()
        },
    );

    // Anything left claimable by an earlier run belongs to a staging
    // directory that no longer exists. Cancelled rather than deleted: a
    // record of work attempted should not vanish because a test tidied up.
    sqlx::query(
        "UPDATE import_jobs SET state = 'canceled', lease_owner = NULL, lease_expires_at = NULL
         WHERE state IN ('requested', 'acquiring', 'staged', 'hashing', 'inspecting',
                         'registering', 'failed_retryable')",
    )
    .execute(&pool)
    .await
    .expect("drain the import queue");

    let session = support::administrator(&pool).await;
    Harness {
        session,
        router: router(state),
        runner,
        pool,
        watched,
        dir,
        _queue: queue,
    }
}

impl Harness {
    async fn send(&self, mut request: Request<Body>) -> (StatusCode, serde_json::Value) {
        self.session.apply(&mut request);
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("route the request");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 8 << 20)
            .await
            .expect("read the body");
        let body = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).expect("a JSON body")
        };
        (status, body)
    }

    async fn get(&self, path: &str) -> (StatusCode, serde_json::Value) {
        self.send(
            Request::builder()
                .method("GET")
                .uri(path)
                .body(Body::empty())
                .expect("build"),
        )
        .await
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        self.send(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("build"),
        )
        .await
    }

    async fn upload(&self, filename: &str, bytes: Vec<u8>) -> (StatusCode, serde_json::Value) {
        self.send(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/imports/upload?filename={filename}"))
                .header("content-type", "application/octet-stream")
                .body(Body::from(bytes))
                .expect("build"),
        )
        .await
    }

    /// Run the queue until this import settles, or give up.
    ///
    /// Stepping rather than looping in the background: a test that raced a
    /// timer would fail for reasons that have nothing to do with importing.
    async fn drain_until_settled(&self, import: &str) -> serde_json::Value {
        for _ in 0..20 {
            let (_, body) = self.get(&format!("/api/v1/imports/{import}")).await;
            if body["is_settled"].as_bool().unwrap_or(false) {
                return body;
            }
            if !self.runner.step().await.expect("step the queue") {
                // Nothing claimable. Either it settled between the two calls
                // or something else holds it; the next loop tells us which.
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
        let (_, body) = self.get(&format!("/api/v1/imports/{import}")).await;
        panic!("the import never settled: {body}");
    }
}

// --- uploads -----------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_uploaded_iso_becomes_an_artifact() {
    let harness = harness().await;
    let image = iso_image("UPLOADED", 32);

    let (status, queued) = harness.upload("disc.iso", image.clone()).await;
    // Accepted, not created: the bytes are safe and the job exists, but
    // nothing has been imported yet.
    assert_eq!(status, StatusCode::ACCEPTED, "{queued}");
    assert_eq!(queued["state"], "staged");
    assert_eq!(
        queued["bytes_received"],
        i64::try_from(image.len()).expect("a sane length")
    );
    assert_eq!(queued["source_filename"], "disc.iso");

    let import = queued["id"].as_str().expect("an id").to_owned();
    let settled = harness.drain_until_settled(&import).await;

    assert_eq!(settled["state"], "complete", "{settled}");
    let artifact = settled["artifact_id"].as_str().expect("an artifact");

    // Visible in the library, which reads manifests…
    let (status, detail) = harness.get(&format!("/api/v1/artifacts/{artifact}")).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["format"], "iso");
    assert_eq!(
        detail["total_bytes"],
        i64::try_from(image.len()).expect("a sane length")
    );

    // …and in the catalog, which is what a burn job can reference. An
    // artifact in one and not the other is browsable and unburnable.
    let catalogued: i64 = sqlx::query_scalar("SELECT count(*) FROM artifacts WHERE id = $1::uuid")
        .bind(artifact)
        .fetch_one(&harness.pool)
        .await
        .expect("count");
    assert_eq!(catalogued, 1);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_upload_larger_than_the_limit_is_refused_as_it_arrives() {
    // Enforced on the bytes rather than on a header: a limit checked
    // afterwards has already cost the disk space it was meant to protect.
    let harness = harness_with_limit(1024).await;
    let (status, problem) = harness.upload("big.iso", vec![0_u8; 4096]).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{problem}");

    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM import_jobs WHERE source_kind = 'upload' AND bytes_received = 4096",
    )
    .fetch_one(&harness.pool)
    .await
    .expect("count");
    assert_eq!(queued, 0, "a refused upload must not leave a job behind");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_upload_filename_cannot_name_a_directory() {
    // The filename comes from a browser, which passes on whatever the
    // operating system gave it.
    let harness = harness().await;
    for name in ["..%2Fescape.iso", "a%2Fb.iso"] {
        let (status, problem) = harness.upload(name, vec![0_u8; 16]).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{name}: {problem}"
        );
        assert_eq!(problem["code"], "VALIDATION_FAILED");
    }
}

// --- watched folders ----------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_file_in_a_watched_folder_imports() {
    let harness = harness().await;
    let image = iso_image("WATCHED", 24);
    std::fs::write(harness.watched.join("watched.iso"), &image).expect("write");

    let (status, queued) = harness
        .post(
            "/api/v1/imports",
            json!({ "path_id": "incoming", "relative_path": "watched.iso" }),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{queued}");
    assert_eq!(queued["state"], "requested");
    assert_eq!(
        queued["bytes_expected"],
        i64::try_from(image.len()).expect("a sane length")
    );

    let import = queued["id"].as_str().expect("an id").to_owned();
    let settled = harness.drain_until_settled(&import).await;
    assert_eq!(settled["state"], "complete", "{settled}");

    // The operator's file is theirs: importing copies, it never moves or
    // rewrites the original.
    assert_eq!(
        std::fs::read(harness.watched.join("watched.iso")).expect("read"),
        image,
        "the source file must be untouched"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_unconfigured_root_is_refused_without_naming_the_host() {
    // A caller guessing identifiers must not be able to map the filesystem.
    let harness = harness().await;
    let (status, problem) = harness
        .post(
            "/api/v1/imports",
            json!({ "path_id": "elsewhere", "relative_path": "x.iso" }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem["code"], "REFERENCE_NOT_FOUND");
    let detail = problem["detail"].as_str().expect("detail");
    assert!(
        !detail.contains(&harness.dir.path().display().to_string()),
        "the refusal leaked a host path: {detail}"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn traversal_out_of_a_watched_root_is_refused() {
    let harness = harness().await;
    std::fs::write(harness.dir.path().join("secret.iso"), b"x").expect("write");

    for relative in ["../secret.iso", "/etc/passwd", "sub/../../secret.iso"] {
        let (status, problem) = harness
            .post(
                "/api/v1/imports",
                json!({ "path_id": "incoming", "relative_path": relative }),
            )
            .await;
        assert!(
            status == StatusCode::UNPROCESSABLE_ENTITY,
            "{relative} was not refused: {status} {problem}"
        );
    }

    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM import_jobs WHERE source_kind = 'watch_folder' AND source_descriptor_json->>'relative_path' = '../secret.iso'")
        .fetch_one(&harness.pool)
        .await
        .expect("count");
    assert_eq!(jobs, 0, "a refused path must not leave a job behind");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
#[cfg(unix)]
async fn a_symlink_in_a_watched_folder_is_not_followed() {
    // The case the whole watched-root type exists for.
    let harness = harness().await;
    let outside = harness.dir.path().join("outside.iso");
    std::fs::write(&outside, iso_image("OUTSIDE", 20)).expect("write");
    std::os::unix::fs::symlink(&outside, harness.watched.join("link.iso")).expect("symlink");

    let (status, problem) = harness
        .post(
            "/api/v1/imports",
            json!({ "path_id": "incoming", "relative_path": "link.iso" }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_missing_file_is_reported_before_a_job_exists() {
    // An operator who mistyped a name should be told at once, not by a job
    // that fails a minute later.
    let harness = harness().await;
    let (status, problem) = harness
        .post(
            "/api/v1/imports",
            json!({ "path_id": "incoming", "relative_path": "absent.iso" }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem["code"], "REFERENCE_NOT_FOUND");
}

// --- the queue ------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn imports_are_listed_newest_first_and_page() {
    let harness = harness().await;
    let first = harness.upload("one.iso", iso_image("ONE", 20)).await.1["id"]
        .as_str()
        .expect("id")
        .to_owned();
    let second = harness.upload("two.iso", iso_image("TWO", 20)).await.1["id"]
        .as_str()
        .expect("id")
        .to_owned();

    let (status, page) = harness.get("/api/v1/imports?limit=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"][0]["id"], second);

    let cursor = page["next_cursor"].as_str().expect("a cursor");
    let (_, next) = harness
        .get(&format!("/api/v1/imports?limit=1&cursor={cursor}"))
        .await;
    assert_eq!(next["items"][0]["id"], first);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_unknown_import_state_filter_is_refused() {
    let harness = harness().await;
    let (status, problem) = harness.get("/api/v1/imports?state=melting").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["code"], "INVALID_PARAMETER");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_import_can_be_cancelled_before_it_runs() {
    let harness = harness().await;
    let (_, queued) = harness.upload("cancel.iso", iso_image("CANCEL", 20)).await;
    let import = queued["id"].as_str().expect("an id").to_owned();

    let (status, body) = harness
        .post(&format!("/api/v1/imports/{import}/cancel"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "canceled");

    // And the runner leaves it alone afterwards.
    harness.runner.step().await.expect("step");
    let (_, after) = harness.get(&format!("/api/v1/imports/{import}")).await;
    assert_eq!(after["state"], "canceled");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_completed_import_is_not_retried() {
    // Importing the same bytes again is a new import: the artifact this one
    // produced is already in the library.
    let harness = harness().await;
    let (_, queued) = harness.upload("done.iso", iso_image("DONE", 20)).await;
    let import = queued["id"].as_str().expect("an id").to_owned();
    harness.drain_until_settled(&import).await;

    let (status, problem) = harness
        .post(&format!("/api/v1/imports/{import}/retry"), json!({}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .expect("detail")
            .contains("new import")
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_cancelled_import_can_be_retried() {
    let harness = harness().await;
    let (_, queued) = harness.upload("again.iso", iso_image("AGAIN", 20)).await;
    let import = queued["id"].as_str().expect("an id").to_owned();

    harness
        .post(&format!("/api/v1/imports/{import}/cancel"), json!({}))
        .await;
    let (status, body) = harness
        .post(&format!("/api/v1/imports/{import}/retry"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "requested");

    // And it runs to completion, because the staged bytes are still there.
    let settled = harness.drain_until_settled(&import).await;
    assert_eq!(settled["state"], "complete", "{settled}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_import_whose_staged_bytes_are_gone_fails_terminally() {
    // Someone cleared the staging directory, or it was on a volume that did
    // not survive. A retryable failure would requeue forever and hide the
    // real problem behind an endlessly requeued job.
    let harness = harness().await;
    let (_, queued) = harness.upload("vanishing.iso", iso_image("GONE", 20)).await;
    let import = queued["id"].as_str().expect("an id").to_owned();

    let staged = harness
        .dir
        .path()
        .join("staging")
        .join("imports")
        .join(&import);
    // The area survives; its contents do not. That is what a cleaner, or a
    // half-restored volume, actually leaves behind.
    for entry in std::fs::read_dir(&staged).expect("read the staging area") {
        let entry = entry.expect("an entry");
        std::fs::remove_file(entry.path()).expect("remove a staged file");
    }

    let settled = harness.drain_until_settled(&import).await;
    assert_eq!(settled["state"], "failed_terminal", "{settled}");
    assert_eq!(settled["error_code"], "IMPORT_NOTHING_STAGED");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_empty_upload_imports_as_an_empty_artifact() {
    // Zero bytes is a real file, and the system's job is to describe what it
    // was given rather than to argue with it. The burn side is where an
    // empty artifact earns a warning.
    let harness = harness().await;
    let (_, queued) = harness.upload("empty.iso", Vec::new()).await;
    let import = queued["id"].as_str().expect("an id").to_owned();

    let settled = harness.drain_until_settled(&import).await;
    assert_eq!(settled["state"], "complete", "{settled}");

    let artifact = settled["artifact_id"].as_str().expect("an artifact");
    let (_, detail) = harness.get(&format!("/api/v1/artifacts/{artifact}")).await;
    assert_eq!(detail["total_bytes"], 0);
    assert_eq!(
        detail["format"], "unknown",
        "nothing was detected, and saying so is the honest answer"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_unknown_import_is_not_found() {
    let harness = harness().await;
    let missing = uuid::Uuid::now_v7();
    let (status, problem) = harness.get(&format!("/api/v1/imports/{missing}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(problem["code"], "NOT_FOUND");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_configured_sources_are_discoverable() {
    // The UI needs to offer the roots without ever seeing a host path.
    let harness = harness().await;
    let (status, sources) = harness.get("/api/v1/import-sources").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(sources["watch_roots"][0], "incoming");
    assert!(sources["max_upload_bytes"].as_i64().expect("a limit") > 0);
    assert!(
        !sources
            .to_string()
            .contains(&harness.watched.display().to_string()),
        "the host path must not be exposed: {sources}"
    );
}
