// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The fake-burn vertical slice, end to end.
//!
//! One ISO, generated here, taken the whole way: imported into the object
//! store, registered in the catalog, listed in the library, queued as a burn,
//! claimed by an enrolled worker over the real protocol, written and read back
//! by the hardware-free engine, and recorded as a physical copy.
//!
//! Everything except the drive is real. The HTTP requests go through the
//! actual router, the state changes go through the actual repositories, and
//! the engine writes to a file standing in for the disc, so a wrong byte is
//! caught by the same read-back a real drive would face.
//!
//! Two paths are covered, and the second matters more. A clean burn must end
//! as a verified physical copy. A drive that reports success while producing
//! a bad disc must end as a *recorded* disc that is not filed as good, because
//! an untracked ruined disc is the one that gets shelved and reused.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use sqlx::PgPool;
use tangible_api::{ApiState, ImportCheckpoint, ImportPipeline, ImportRequest, catalog, router};
use tangible_burn::{
    BurnEngine, BurnPlan, CollectingSink, DriveRef, FakeBehaviour, FakeEngine, PlannedInput,
    WriteMode,
};
use tangible_db::{Database, DbConfig};
use tangible_domain::manifest::ArtifactManifest;
use tangible_domain::{BurnAttemptId, DriveId, ImportJobId, LogicalPath, Sha256Digest, WorkerId};
use tangible_storage::{FilesystemStore, IngestLimits, ManifestStore, StagingManager};
use tempfile::TempDir;
use tower::ServiceExt as _;

/// Serialises the whole file: a claim takes any queued job, so two of these
/// running at once would take each other's work.
static QUEUE: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

async fn exclusive_queue() -> tokio::sync::MutexGuard<'static, ()> {
    QUEUE
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

const SECTOR: usize = 2048;
const SYSTEM_AREA: usize = 16 * SECTOR;

/// A minimal but structurally valid ISO of `blocks` 2048-byte blocks.
///
/// The same generator the import tests use. Real enough that detection
/// recognises it and the structural inspector reads a volume label out of it.
fn iso_image(volume_id: &str, blocks: u32) -> Vec<u8> {
    let mut bytes = vec![0_u8; SYSTEM_AREA];

    let mut pvd = vec![0_u8; SECTOR];
    pvd[0] = 1;
    pvd[1..6].copy_from_slice(b"CD001");
    pvd[6] = 1;
    for slot in &mut pvd[40..72] {
        *slot = b' ';
    }
    let id = volume_id.as_bytes();
    pvd[40..40 + id.len()].copy_from_slice(id);
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

struct World {
    router: axum::Router,
    pool: PgPool,
    pipeline: ImportPipeline,
    dir: TempDir,
}

async fn world() -> World {
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

    World {
        router: router(ApiState::with_manifests(database, manifests)),
        pool,
        pipeline,
        dir,
    }
}

impl World {
    async fn send(&self, request: Request<Body>) -> (StatusCode, serde_json::Value) {
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("route the request");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
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
                .expect("build the request"),
        )
        .await
    }

    async fn post(
        &self,
        path: &str,
        body: serde_json::Value,
        credential: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json");
        if let Some(credential) = credential {
            request = request.header("authorization", format!("Bearer {credential}"));
        }
        self.send(
            request
                .body(Body::from(body.to_string()))
                .expect("build the request"),
        )
        .await
    }

    /// Import one ISO and register it in the catalog.
    ///
    /// Exactly what an import route will do when there is one: run the
    /// pipeline, then write the catalog rows the manifest describes.
    async fn import_iso(&self, bytes: &[u8]) -> ArtifactManifest {
        let import_id = ImportJobId::generate();
        let area = self.pipeline.open_area(import_id).await.expect("area");
        area.write(
            &LogicalPath::parse("disc.iso").expect("a valid path"),
            bytes,
        )
        .await
        .expect("stage the image");

        let mut checkpoint = ImportCheckpoint::default();
        let outcome = self
            .pipeline
            .run(
                &ImportRequest {
                    import_id,
                    source_kind: "upload".to_owned(),
                    source_filename: Some("disc.iso".to_owned()),
                    source_reference: Some("upload:end-to-end".to_owned()),
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
            .expect("read the manifest");

        let url = std::env::var("TANGIBLE_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("TANGIBLE_DATABASE_URL"))
            .expect("a database url");
        let database = Database::connect(&DbConfig::new(url))
            .await
            .expect("connect");
        let registered = catalog::register_manifest(&database, &manifest)
            .await
            .expect("register");
        assert!(registered.created, "the artifact was new");

        manifest
    }

    /// A disc in the catalog for the burn to produce.
    ///
    /// Hand-seeded because catalog write routes do not exist yet; when they
    /// do, this is the only part of the flow that changes.
    async fn seed_disc(&self) -> String {
        let ids: Vec<uuid::Uuid> = (0..3).map(|_| uuid::Uuid::now_v7()).collect();
        let disc = uuid::Uuid::now_v7();
        sqlx::query("INSERT INTO titles (id, display_title, sort_title) VALUES ($1, $2, $2)")
            .bind(ids[0])
            .bind("Example Disc")
            .execute(&self.pool)
            .await
            .expect("seed a title");
        sqlx::query("INSERT INTO editions (id, title_id, display_name) VALUES ($1, $2, 'E')")
            .bind(ids[1])
            .bind(ids[0])
            .execute(&self.pool)
            .await
            .expect("seed an edition");
        sqlx::query("INSERT INTO disc_sets (id, edition_id, name) VALUES ($1, $2, 'S')")
            .bind(ids[2])
            .bind(ids[1])
            .execute(&self.pool)
            .await
            .expect("seed a disc set");
        sqlx::query("INSERT INTO discs (id, disc_set_id, sequence_number) VALUES ($1, $2, 1)")
            .bind(disc)
            .bind(ids[2])
            .execute(&self.pool)
            .await
            .expect("seed a disc");
        disc.to_string()
    }

    /// Enroll a worker through the protocol and attach a drive to it.
    async fn enroll_worker(&self) -> Worker {
        let secret = format!("tgw_enroll_{}", uuid::Uuid::now_v7().simple());
        let token_hash = {
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
        .bind(&token_hash)
        .execute(&self.pool)
        .await
        .expect("issue an enrollment token");

        let (status, body) = self
            .post(
                "/api/v1/worker-enrollments/consume",
                json!({
                    "enrollment_token": secret,
                    "name": format!("end-to-end-{}", uuid::Uuid::now_v7()),
                    "protocol_versions": ["1alpha1"],
                    "software_version": "0.1.0",
                }),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let worker_id = body["worker_id"].as_str().expect("worker_id").to_owned();
        let credential = body["credential"].as_str().expect("credential").to_owned();

        let drive = uuid::Uuid::now_v7();
        sqlx::query(
            "INSERT INTO drives (id, worker_id, configured_name, device_alias)
             VALUES ($1, $2::uuid, 'workshop', '/dev/disc-block')",
        )
        .bind(drive)
        .bind(&worker_id)
        .execute(&self.pool)
        .await
        .expect("attach a drive");

        Worker {
            id: worker_id,
            credential,
            drive: drive.to_string(),
        }
    }

    /// Fetch raw bytes, as a worker downloading a manifest or a component.
    async fn get_bytes(&self, path: &str) -> (StatusCode, Vec<u8>) {
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(path)
                    .body(Body::empty())
                    .expect("build the request"),
            )
            .await
            .expect("route the request");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 8 << 20)
            .await
            .expect("read the body")
            .to_vec();
        (status, bytes)
    }

    /// Stage an artifact the way the protocol says a worker must.
    ///
    /// Over HTTP, not by reaching into the object store: download the
    /// manifest, check it against the digest the lease named, then download
    /// each component and hash it as it lands. This is the check that makes
    /// "a burn starts only after hash verification" real: the bytes about to
    /// be written are the bytes the manifest describes, or nothing is written.
    async fn stage_from_server(&self, lease: &serde_json::Value) -> (PathBuf, Sha256Digest, u64) {
        use sha2::Digest as _;

        let manifest_url = lease["artifact"]["manifest_url"]
            .as_str()
            .expect("a manifest url");
        let (status, manifest_bytes) = self.get_bytes(manifest_url).await;
        assert_eq!(status, StatusCode::OK);

        let announced = lease["artifact"]["manifest_sha256"]
            .as_str()
            .expect("a manifest digest");
        let downloaded = {
            let mut hasher = sha2::Sha256::new();
            hasher.update(&manifest_bytes);
            hex::encode(hasher.finalize())
        };
        assert_eq!(
            downloaded, announced,
            "the manifest downloaded is the manifest the lease named"
        );

        let manifest: ArtifactManifest =
            serde_json::from_slice(&manifest_bytes).expect("a parseable manifest");
        let component = manifest.components.first().expect("one component");

        let (status, bytes) = self
            .get_bytes(&format!(
                "/api/v1/artifacts/{}/components/{}/content",
                manifest.artifact_id, component.id
            ))
            .await;
        assert_eq!(status, StatusCode::OK);

        let staged_digest = {
            let mut hasher = sha2::Sha256::new();
            hasher.update(&bytes);
            Sha256Digest::from_bytes(hasher.finalize().into())
        };
        assert_eq!(
            staged_digest, component.content.sha256,
            "the staged bytes must be the bytes the manifest describes"
        );

        let staged = self.dir.path().join("worker-staging").join("disc.iso");
        std::fs::create_dir_all(staged.parent().expect("a parent")).expect("staging dir");
        std::fs::write(&staged, &bytes).expect("stage for the burn");
        (staged, staged_digest, component.length_bytes)
    }
}

struct Worker {
    id: String,
    credential: String,
    drive: String,
}

/// Report one stage to the server, as a worker does while it works.
async fn report(world: &World, worker: &Worker, attempt: &str, sequence: i64, stage: &str) {
    let (status, body) = world
        .post(
            &format!("/api/v1/burn-attempts/{attempt}/events"),
            json!({
                "events": [{
                    "sequence": sequence,
                    "event_type": "stage_changed",
                    "stage": stage,
                    "code": "STAGE_CHANGED",
                    "progress": null,
                    "data": {},
                    "worker_time": "2026-01-01T00:00:00Z",
                }]
            }),
            Some(&worker.credential),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// The plan a worker builds from its lease and its staged bytes.
fn plan(
    attempt: &str,
    worker: &Worker,
    staged: PathBuf,
    digest: Sha256Digest,
    bytes: u64,
) -> BurnPlan {
    BurnPlan {
        attempt_id: attempt.parse::<BurnAttemptId>().expect("an attempt id"),
        drive: DriveRef {
            worker_id: worker.id.parse::<WorkerId>().expect("a worker id"),
            drive_id: worker.drive.parse::<DriveId>().expect("a drive id"),
            device_alias: "/dev/disc-block".to_owned(),
        },
        inputs: vec![PlannedInput {
            staged_path: staged,
            sha256: digest,
            length_bytes: bytes,
        }],
        tracks: Vec::new(),
        catalog: None,
        mode: WriteMode::DataDiscAtOnce,
        accepted_profiles: vec!["CD-R".to_owned()],
        speed: None,
        finalize: true,
        eject_on_success: true,
        total_bytes: bytes,
    }
}

// --- the whole slice ---------------------------------------------------------------

// Long on purpose: the value of this test is that one narrative runs from an
// imported file to a recorded disc without a seam. Splitting it into helpers
// would hide the order the steps happen in, which is the thing being asserted.
#[allow(clippy::too_many_lines)]
#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_imported_iso_is_burned_verified_and_recorded() {
    let _queue = exclusive_queue().await;
    let world = world().await;
    sqlx::query("UPDATE burn_jobs SET state = 'canceled' WHERE state = 'queued'")
        .execute(&world.pool)
        .await
        .expect("drain the queue");

    // --- import ------------------------------------------------------------
    let image = iso_image("EXAMPLE_DISC", 40);
    let manifest = world.import_iso(&image).await;
    assert_eq!(manifest.classification.total_bytes, image.len() as u64);

    // The library reads manifests, so the import is visible immediately.
    let (status, listed) = world
        .get(&format!("/api/v1/artifacts/{}", manifest.artifact_id))
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed["format"], "iso");

    // --- queue -------------------------------------------------------------
    let disc = world.seed_disc().await;
    let (status, job) = world
        .post(
            "/api/v1/burn-jobs",
            json!({
                "disc_id": disc,
                "artifact_id": manifest.artifact_id.to_string(),
                "requested_media_profile": "CD-R",
                "priority": 100,
            }),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{job}");
    let job_id = job["id"].as_str().expect("a job id").to_owned();
    assert_eq!(job["state"], "queued");

    // --- claim -------------------------------------------------------------
    let worker = world.enroll_worker().await;
    let (status, lease) = world
        .post(
            &format!("/api/v1/workers/{}/claims", worker.id),
            json!({ "drive_id": worker.drive, "engine": "fake", "engine_version": "0.1.0" }),
            Some(&worker.credential),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{lease}");
    assert_eq!(lease["burn_job_id"], job_id);
    assert_eq!(
        lease["artifact"]["artifact_id"],
        manifest.artifact_id.to_string()
    );
    assert_eq!(lease["eject_policy"], "eject_on_success");
    assert_eq!(lease["requested_media_profile"], "CD-R");
    let attempt = lease["attempt_id"].as_str().expect("an attempt").to_owned();
    let lease_token = lease["lease_token"].as_str().expect("a lease").to_owned();
    assert_eq!(lease["verification_policy"][0], "full_sector_readback");

    // --- stage -------------------------------------------------------------
    report(&world, &worker, &attempt, 1, "staging").await;
    let (staged, digest, bytes) = world.stage_from_server(&lease).await;
    let plan = plan(&attempt, &worker, staged, digest, bytes);
    let engine = FakeEngine::new(world.dir.path().join("media"));
    let sink = CollectingSink::new();

    // --- preflight ---------------------------------------------------------
    report(&world, &worker, &attempt, 2, "preflighting").await;
    let preflight = engine.preflight(&plan).await.expect("preflight");
    assert!(preflight.passed(), "failures: {:?}", preflight.failures);

    let (_, waiting) = world.get(&format!("/api/v1/burn-jobs/{job_id}")).await;
    assert_eq!(waiting["state"], "preflighting");
    assert_eq!(waiting["has_started_writing"], false);

    // --- write -------------------------------------------------------------
    report(&world, &worker, &attempt, 3, "writing").await;
    let write = engine.write(&plan, &sink).await.expect("write");
    assert!(write.engine_reported_success);
    assert_eq!(write.bytes_written, bytes);

    // Past the point of no return: cancelling is now refused, whatever the
    // operator asks for.
    let (status, refusal) = world
        .post(
            &format!("/api/v1/burn-jobs/{job_id}/cancel"),
            json!({}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refusal}");
    assert_eq!(refusal["code"], "WRITE_IN_PROGRESS");

    // --- verify ------------------------------------------------------------
    report(&world, &worker, &attempt, 4, "verifying").await;
    let verify = engine.verify(&plan, &sink).await.expect("verify");
    assert!(verify.matched, "the read-back must match");
    assert_eq!(verify.bytes_compared, bytes);

    // The medium really holds the imported image, byte for byte.
    let written = std::fs::read(engine.medium_path(&plan.drive)).expect("read the medium");
    assert_eq!(written, image, "the simulated disc holds the imported ISO");

    // --- complete ----------------------------------------------------------
    let (status, completion) = world
        .post(
            &format!("/api/v1/burn-attempts/{attempt}/complete"),
            json!({
                "lease_token": lease_token,
                "last_sequence": 4,
                "write_report": {
                    "state": "success",
                    "engine": "fake",
                    "engine_version": "0.1.0",
                    "started_at": "2026-01-01T00:00:00Z",
                    "completed_at": "2026-01-01T00:05:00Z",
                },
                "verification_report": {
                    "policy": "full_sector_readback",
                    "state": "match",
                    "bytes_read": verify.bytes_compared,
                    "expected_sha256": digest.to_hex(),
                    "observed_sha256": digest.to_hex(),
                },
                "physical_medium": {
                    "profile": "CD-R",
                    "manufacturer_id": "SIMULATED",
                    "serial": null,
                },
            }),
            Some(&worker.credential),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{completion}");
    assert_eq!(completion["acknowledged"], true);
    // A clean success is the only outcome that ejects.
    assert_eq!(completion["eject"], true);
    let copy_id = completion["physical_copy_id"]
        .as_str()
        .expect("a physical copy")
        .to_owned();

    // --- what the operator sees --------------------------------------------
    let (_, finished) = world.get(&format!("/api/v1/burn-jobs/{job_id}")).await;
    assert_eq!(finished["state"], "complete");
    assert_eq!(finished["attempt_count"], 1);
    assert_eq!(finished["attempts"][0]["state"], "verified");
    assert_eq!(finished["attempts"][0]["consumed_media"], true);
    assert_eq!(finished["attempts"][0]["physical_copy_id"], copy_id);

    let (_, timeline) = world
        .get(&format!("/api/v1/burn-attempts/{attempt}/events"))
        .await;
    let reported: Vec<&str> = timeline["items"]
        .as_array()
        .expect("events")
        .iter()
        .filter_map(|event| event["stage"].as_str())
        .collect();
    assert_eq!(
        reported,
        vec!["staging", "preflighting", "writing", "verifying"]
    );

    // --- the physical record -----------------------------------------------
    let (status, media_profile, verification_result): (String, String, String) = sqlx::query_as(
        "SELECT status, media_profile, verification_result FROM physical_copies WHERE id = $1::uuid",
    )
    .bind(&copy_id)
    .fetch_one(&world.pool)
    .await
    .expect("the physical copy");
    assert_eq!(status, "verified");
    assert_eq!(media_profile, "CD-R");
    assert_eq!(verification_result, "passed");
}

// Long for the same reason as the happy path above: the order is the point.
#[allow(clippy::too_many_lines)]
#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_disc_that_fails_verification_is_recorded_rather_than_forgotten() {
    // The engine reports a clean write and the medium is bad anyway, which is
    // exactly why write success and verification are separate claims. The disc
    // physically exists, so it must be trackable and destroyable, and it must
    // never be filed as a good copy.
    let _queue = exclusive_queue().await;
    let world = world().await;
    sqlx::query("UPDATE burn_jobs SET state = 'canceled' WHERE state = 'queued'")
        .execute(&world.pool)
        .await
        .expect("drain the queue");

    let image = iso_image("BAD_BURN", 24);
    let manifest = world.import_iso(&image).await;
    let disc = world.seed_disc().await;

    let (status, job) = world
        .post(
            "/api/v1/burn-jobs",
            json!({
                "disc_id": disc,
                "artifact_id": manifest.artifact_id.to_string(),
                "priority": 100,
            }),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{job}");
    let job_id = job["id"].as_str().expect("a job id").to_owned();

    let worker = world.enroll_worker().await;
    let (_, lease) = world
        .post(
            &format!("/api/v1/workers/{}/claims", worker.id),
            json!({ "drive_id": worker.drive, "engine": "fake", "engine_version": "0.1.0" }),
            Some(&worker.credential),
        )
        .await;
    let attempt = lease["attempt_id"].as_str().expect("an attempt").to_owned();
    let lease_token = lease["lease_token"].as_str().expect("a lease").to_owned();

    let (staged, digest, bytes) = world.stage_from_server(&lease).await;
    let plan = plan(&attempt, &worker, staged, digest, bytes);
    let engine = FakeEngine::new(world.dir.path().join("media")).with_behaviour(FakeBehaviour {
        corrupt_after_write: true,
        ..FakeBehaviour::default()
    });
    let sink = CollectingSink::new();

    report(&world, &worker, &attempt, 1, "writing").await;
    let write = engine.write(&plan, &sink).await.expect("write");
    assert!(
        write.engine_reported_success,
        "the engine believes it succeeded"
    );

    report(&world, &worker, &attempt, 2, "verifying").await;
    let verify = engine.verify(&plan, &sink).await.expect("verify");
    assert!(!verify.matched, "the read-back must catch the bad disc");

    let (status, completion) = world
        .post(
            &format!("/api/v1/burn-attempts/{attempt}/complete"),
            json!({
                "lease_token": lease_token,
                "last_sequence": 2,
                "write_report": {
                    "state": "success",
                    "engine": "fake",
                    "engine_version": "0.1.0",
                    "started_at": "2026-01-01T00:00:00Z",
                    "completed_at": "2026-01-01T00:05:00Z",
                },
                "verification_report": {
                    "policy": "full_sector_readback",
                    "state": "mismatch",
                    "bytes_read": verify.bytes_compared,
                    "expected_sha256": digest.to_hex(),
                    "observed_sha256": "0".repeat(64),
                },
                "physical_medium": { "profile": "CD-R" },
            }),
            Some(&worker.credential),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{completion}");
    // The disc stays in the drive: it is not something to hand back as though
    // it were good.
    assert_eq!(completion["eject"], false);
    let copy_id = completion["physical_copy_id"]
        .as_str()
        .expect("the bad disc is still recorded")
        .to_owned();

    let (copy_status, verification_result): (String, String) = sqlx::query_as(
        "SELECT status, verification_result FROM physical_copies WHERE id = $1::uuid",
    )
    .bind(&copy_id)
    .fetch_one(&world.pool)
    .await
    .expect("the physical copy");
    assert_eq!(copy_status, "verification_failed");
    assert_eq!(verification_result, "failed");

    let (_, finished) = world.get(&format!("/api/v1/burn-jobs/{job_id}")).await;
    assert_eq!(finished["state"], "failed");
    assert_eq!(finished["attempts"][0]["state"], "verification_failed");

    // And the operator can spend another disc on it, deliberately.
    let (status, retried) = world
        .post(
            &format!("/api/v1/burn-jobs/{job_id}/retry"),
            json!({}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    assert_eq!(retried["state"], "queued");
    assert_eq!(
        retried["attempts"].as_array().expect("attempts").len(),
        1,
        "the failed attempt is history and stays"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn registering_the_same_import_twice_catalogs_it_once() {
    // Import completion is idempotent by artifact identity: a pipeline that
    // stored its bytes and then failed to answer must be able to finish.
    let world = world().await;
    let manifest = world.import_iso(&iso_image("TWICE", 20)).await;

    let url = std::env::var("TANGIBLE_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TANGIBLE_DATABASE_URL"))
        .expect("a database url");
    let database = Database::connect(&DbConfig::new(url))
        .await
        .expect("connect");
    let count_before_replay = references_to(&world, &manifest).await;
    let again = catalog::register_manifest(&database, &manifest)
        .await
        .expect("register again");
    assert!(!again.created, "the second registration created nothing");

    let components: i64 =
        sqlx::query_scalar("SELECT count(*) FROM artifact_components WHERE artifact_id = $1")
            .bind(manifest.artifact_id.as_uuid())
            .fetch_one(&world.pool)
            .await
            .expect("count");
    assert_eq!(
        components,
        i64::try_from(manifest.components.len()).expect("a sane component count")
    );

    // Reference counts are what garbage collection reads. Inflating them on a
    // replay would pin objects the collector should be free to take.
    //
    // Compared before and after rather than against a fixed number: the object
    // store deduplicates, so an identical image imported by an earlier run
    // shares this row and its count is already whatever that run left. The
    // property is that a replay adds nothing, not that the count is one.
    let count_after_replay = references_to(&world, &manifest).await;
    assert_eq!(
        count_before_replay, count_after_replay,
        "a replayed registration inflated a reference count"
    );
}

/// How many artifacts the catalog believes reference an artifact's first
/// component.
async fn references_to(world: &World, manifest: &ArtifactManifest) -> i32 {
    sqlx::query_scalar("SELECT reference_count FROM cas_objects WHERE sha256 = $1")
        .bind(manifest.components[0].content.sha256.to_hex())
        .fetch_one(&world.pool)
        .await
        .expect("reference count")
}
