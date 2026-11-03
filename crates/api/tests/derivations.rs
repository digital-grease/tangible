// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Derivatives, from request to lineage.
//!
//! Real imports, the real routes, the real runner and queue, and the fake
//! engine in place of chdman. What is asserted is what an operator relies
//! on: asking twice does the work once, the derivative is catalogued with
//! its lineage and linked to its parent's disc, the parent is untouched, and
//! a failure says why.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::sync::{Arc, OnceLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use tangible_api::{
    ApiState, DerivationPipeline, DerivationRunner, DerivationRunnerSettings, Derivations,
    ImportCheckpoint, ImportPipeline, ImportRequest, catalog, router,
};
use tangible_domain::{ImportJobId, LogicalPath, LossCharacter};
use tangible_image::derive::{DerivationEngine, FakeDeriver};
use tangible_storage::{FilesystemStore, IngestLimits, ManifestStore, StagingManager};
use tempfile::TempDir;
use tower::ServiceExt as _;

mod support;

const RAW: usize = 2352;

static QUEUE: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

/// One test at a time owns the derivation queue, so a runner only ever
/// claims what its own test asked for.
async fn exclusive_queue(pool: &PgPool) -> tokio::sync::MutexGuard<'static, ()> {
    let guard = QUEUE
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    // Anything a previous run left claimable is finished, not deleted: jobs
    // reference artifacts with RESTRICT.
    sqlx::query(
        "UPDATE derivation_jobs
         SET state = 'canceled', completed_at = now(), lease_owner = NULL, lease_expires_at = NULL
         WHERE state IN ('queued', 'failed_retryable')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE derivation_jobs
         SET state = 'failed_terminal', completed_at = now(),
             lease_owner = NULL, lease_expires_at = NULL
         WHERE state = 'running'",
    )
    .execute(pool)
    .await
    .unwrap();
    guard
}

struct Harness {
    _dir: TempDir,
    objects: FilesystemStore,
    router: axum::Router,
    database: tangible_db::Database,
    pipeline: ImportPipeline,
    runner: DerivationRunner,
}

async fn harness_with(engine: Option<Arc<dyn DerivationEngine>>) -> Harness {
    harness_using(Some(
        engine.unwrap_or_else(|| Arc::new(FakeDeriver::default())),
    ))
    .await
}

/// A harness whose derivations run with `engine`, or with none.
async fn harness_using(engine: Option<Arc<dyn DerivationEngine>>) -> Harness {
    let dir = TempDir::new().unwrap();
    let objects = FilesystemStore::open(dir.path().join("library"))
        .await
        .unwrap();
    let manifests = ManifestStore::open(objects.clone()).await.unwrap();
    let staging = StagingManager::open(dir.path().join("staging"))
        .await
        .unwrap();
    let pipeline = ImportPipeline::new(staging.clone(), objects.clone(), manifests.clone());
    let database = support::database().await;
    let session = support::administrator(database.pool()).await;

    let derivations = Derivations::new(match engine {
        Some(engine) => {
            DerivationPipeline::new(staging, objects.clone(), manifests.clone(), engine)
        }
        None => DerivationPipeline::without_engine(staging, objects.clone(), manifests.clone()),
    });
    let runner = DerivationRunner::new(
        database.clone(),
        derivations.clone(),
        DerivationRunnerSettings {
            retry_backoff: std::time::Duration::from_secs(1),
            ..DerivationRunnerSettings::default()
        },
    );
    let state = ApiState::with_manifests(database.clone(), manifests).with_derivations(derivations);
    Harness {
        _dir: dir,
        objects,
        router: support::signed_in(router(state), session),
        database,
        pipeline,
        runner,
    }
}

async fn harness() -> Harness {
    harness_with(None).await
}

impl Harness {
    fn pool(&self) -> &PgPool {
        self.database.pool()
    }

    async fn send(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        support_send(&self.router, method, uri, body).await
    }

    async fn import(&self, files: &[(&str, Vec<u8>)]) -> String {
        let import_id = ImportJobId::generate();
        let area = self.pipeline.open_area(import_id).await.unwrap();
        for (name, bytes) in files {
            area.write(&LogicalPath::parse(name).unwrap(), bytes)
                .await
                .unwrap();
        }
        let mut checkpoint = ImportCheckpoint::default();
        let outcome = self
            .pipeline
            .run(
                &ImportRequest {
                    import_id,
                    source_kind: "upload".to_owned(),
                    source_filename: Some(files[0].0.to_owned()),
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
            .unwrap();
        let manifest = self
            .pipeline
            .manifests()
            .read(outcome.artifact_id)
            .await
            .unwrap();
        catalog::register_manifest(&self.database, &manifest)
            .await
            .unwrap();
        outcome.artifact_id.to_string()
    }

    /// A disc in the catalog with `artifact` linked to it; returns the disc.
    async fn disc_for(&self, artifact: &str) -> String {
        let (_, t) = self
            .send(
                "POST",
                "/api/v1/titles",
                Some(json!({ "display_title": "Example Quest", "kind": "game" })),
            )
            .await;
        let (_, e) = self
            .send(
                "POST",
                &format!("/api/v1/titles/{}/editions", t["id"].as_str().unwrap()),
                Some(json!({ "display_name": "Retail" })),
            )
            .await;
        let (_, s) = self
            .send(
                "POST",
                &format!("/api/v1/editions/{}/disc-sets", e["id"].as_str().unwrap()),
                Some(json!({ "name": "Disc", "set_kind": "single_disc" })),
            )
            .await;
        let (_, d) = self
            .send(
                "POST",
                &format!("/api/v1/disc-sets/{}/discs", s["id"].as_str().unwrap()),
                Some(json!({ "sequence_number": 1, "media_family": "cd" })),
            )
            .await;
        let disc = d["id"].as_str().unwrap().to_owned();
        let (status, _) = self
            .send(
                "POST",
                &format!("/api/v1/discs/{disc}/artifact-links"),
                Some(json!({ "artifact_id": artifact })),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        disc
    }

    async fn derive(&self, artifact: &str, body: Value) -> (StatusCode, Value) {
        self.send(
            "POST",
            &format!("/api/v1/artifacts/{artifact}/derivatives"),
            Some(body),
        )
        .await
    }
}

async fn support_send(
    router: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(body) => {
            request = request.header("content-type", "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    let response = router
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    // A body the extractor refused is answered in plain text, not JSON.
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, value)
}

fn cue_disc(tag: &str) -> Vec<(&'static str, Vec<u8>)> {
    let sheet =
        b"FILE \"disc.bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n".to_vec();
    let mut data = vec![0_u8; RAW * 100];
    data[..tag.len()].copy_from_slice(tag.as_bytes());
    vec![("disc.cue", sheet), ("disc.bin", data)]
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn asking_twice_makes_one_derivative_with_lineage_linked_to_the_same_disc() {
    let h = harness().await;
    let _queue = exclusive_queue(h.pool()).await;
    let parent = h.import(&cue_disc("lineage")).await;
    let disc = h.disc_for(&parent).await;
    let (_, before) = h
        .send("GET", &format!("/api/v1/artifacts/{parent}/manifest"), None)
        .await;

    // Asked for, and asked again while it waits: one job.
    let (status, first) = h.derive(&parent, json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    assert_eq!(first["result"], "queued");
    assert_eq!(first["job"]["transformation"], "chd_create_cd");
    assert_eq!(first["job"]["tool_version"], FakeDeriver::VERSION);
    let job = first["job"]["id"].as_str().unwrap().to_owned();
    let (status, second) = h.derive(&parent, json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(second["result"], "pending");
    assert_eq!(second["job"]["id"], job.as_str());

    assert!(h.runner.step().await.unwrap(), "the runner found the job");
    let (_, done) = h
        .send("GET", &format!("/api/v1/derivation-jobs/{job}"), None)
        .await;
    assert_eq!(done["state"], "complete", "{done}");
    let child = done["child_artifact_id"].as_str().unwrap().to_owned();

    // The derivative is a catalogued artifact of its own, with lineage.
    let (status, artifact) = h
        .send("GET", &format!("/api/v1/artifacts/{child}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{artifact}");
    let (_, manifest) = h
        .send("GET", &format!("/api/v1/artifacts/{child}/manifest"), None)
        .await;
    assert_eq!(manifest["classification"]["format"], "chd");
    assert_eq!(manifest["origin"]["kind"], "derived");
    assert_eq!(manifest["lineage"]["parent_artifact_id"], parent.as_str());
    assert_eq!(
        manifest["lineage"]["command_fingerprint"],
        first["job"]["fingerprint"]
    );
    assert_eq!(manifest["components"][0]["logical_path"], "disc.chd");

    let (_, parent_lineage) = h
        .send("GET", &format!("/api/v1/artifacts/{parent}/lineage"), None)
        .await;
    assert_eq!(
        parent_lineage["derivatives"][0]["child_artifact_id"],
        child.as_str()
    );
    assert_eq!(
        parent_lineage["derivatives"][0]["loss_character"],
        "unknown"
    );
    assert_eq!(parent_lineage["suggested_transformation"], "chd_create_cd");
    let (_, child_lineage) = h
        .send("GET", &format!("/api/v1/artifacts/{child}/lineage"), None)
        .await;
    assert_eq!(
        child_lineage["derived_from"]["parent_artifact_id"],
        parent.as_str()
    );

    // Linked to the parent's disc, as another representation of it.
    let link: (String, String) = sqlx::query_as(
        "SELECT disc_id::text, relationship FROM artifact_disc_links WHERE artifact_id = $1::uuid",
    )
    .bind(&child)
    .fetch_one(h.pool())
    .await
    .unwrap();
    assert_eq!(link, (disc, "representation_of".to_owned()));

    // Asked for once more, it already exists: no new job, no new work.
    let (status, third) = h.derive(&parent, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{third}");
    assert_eq!(third["result"], "exists");
    assert_eq!(third["derivation"]["child_artifact_id"], child.as_str());
    assert!(!h.runner.step().await.unwrap(), "nothing left to run");

    // The parent is exactly as it was, and its objects are still read-only.
    let (_, after) = h
        .send("GET", &format!("/api/v1/artifacts/{parent}/manifest"), None)
        .await;
    assert_eq!(before, after);
    for component in after["components"].as_array().unwrap() {
        let digest: tangible_domain::Sha256Digest = component["content"]["sha256"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let meta = std::fs::metadata(h.objects.object_path(&digest)).unwrap();
        assert!(meta.permissions().readonly());
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn different_options_are_a_different_derivative() {
    let h = harness().await;
    let _queue = exclusive_queue(h.pool()).await;
    let parent = h.import(&cue_disc("options")).await;
    let (_, defaults) = h.derive(&parent, json!({})).await;
    let (status, custom) = h
        .derive(
            &parent,
            json!({ "compression": ["cdzl"], "hunk_bytes": 4896 }),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{custom}");
    assert_eq!(custom["result"], "queued");
    assert_ne!(custom["job"]["fingerprint"], defaults["job"]["fingerprint"]);
    assert_eq!(custom["job"]["options"]["compression"], json!(["cdzl"]));
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn requests_that_cannot_apply_are_refused_before_anything_is_queued() {
    let h = harness().await;
    let _queue = exclusive_queue(h.pool()).await;
    let parent = h.import(&cue_disc("refused")).await;

    let (status, body) = h
        .derive(&parent, json!({ "transformation": "chd_create_dvd" }))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, _) = h.derive(&parent, json!({ "compression": ["lzma"] })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = h.derive(&parent, json!({ "compression": ["gzip"] })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = h.derive(&parent, json!({ "unexpected": true })).await;
    assert!(status.is_client_error());
    let (status, _) = h
        .derive("01a1088f-0165-7652-819b-cac12fcbe043", json!({}))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (_, lineage) = h
        .send("GET", &format!("/api/v1/artifacts/{parent}/lineage"), None)
        .await;
    assert_eq!(lineage["jobs"], json!([]), "nothing was queued");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_failure_says_why_and_only_a_retryable_one_is_tried_again() {
    let h = harness_with(Some(Arc::new(
        FakeDeriver::default().failing("FAKE_REFUSED", false),
    )))
    .await;
    let _queue = exclusive_queue(h.pool()).await;
    let parent = h.import(&cue_disc("fails")).await;
    let (_, queued) = h.derive(&parent, json!({})).await;
    let job = queued["job"]["id"].as_str().unwrap().to_owned();
    assert!(h.runner.step().await.unwrap());
    let (_, failed) = h
        .send("GET", &format!("/api/v1/derivation-jobs/{job}"), None)
        .await;
    assert_eq!(failed["state"], "failed_terminal");
    assert_eq!(failed["error_code"], "FAKE_REFUSED");
    assert!(failed["child_artifact_id"].is_null());
    assert!(
        !h.runner.step().await.unwrap(),
        "a terminal failure is not retried"
    );

    // A terminal failure does not block asking again.
    let (status, again) = h.derive(&parent, json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(again["result"], "queued");
    assert_ne!(again["job"]["id"], job.as_str());

    let retrying = harness_with(Some(Arc::new(
        FakeDeriver::default().failing("FAKE_BUSY", true),
    )))
    .await;
    let parent = retrying.import(&cue_disc("retries")).await;
    let (_, queued) = retrying.derive(&parent, json!({})).await;
    let job = queued["job"]["id"].as_str().unwrap().to_owned();
    // Cancel the job left from the first half, so this runner sees only its own.
    sqlx::query("UPDATE derivation_jobs SET state = 'canceled', completed_at = now() WHERE state = 'queued' AND id <> $1::uuid")
        .bind(&job)
        .execute(retrying.pool())
        .await
        .unwrap();
    assert!(retrying.runner.step().await.unwrap());
    let (_, held) = retrying
        .send("GET", &format!("/api/v1/derivation-jobs/{job}"), None)
        .await;
    assert_eq!(held["state"], "failed_retryable");
    assert!(
        !retrying.runner.step().await.unwrap(),
        "held off for the backoff"
    );
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert!(
        retrying.runner.step().await.unwrap(),
        "offered again after it"
    );
    let (_, again) = retrying
        .send("GET", &format!("/api/v1/derivation-jobs/{job}"), None)
        .await;
    assert_eq!(again["attempts"], 2);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_queued_job_can_be_withdrawn_and_a_finished_one_cannot() {
    let h = harness().await;
    let _queue = exclusive_queue(h.pool()).await;
    let parent = h.import(&cue_disc("cancel")).await;
    let (_, queued) = h.derive(&parent, json!({})).await;
    let job = queued["job"]["id"].as_str().unwrap().to_owned();
    let (status, cancelled) = h
        .send(
            "POST",
            &format!("/api/v1/derivation-jobs/{job}/cancel"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["state"], "canceled");
    let (status, _) = h
        .send(
            "POST",
            &format!("/api/v1/derivation-jobs/{job}/cancel"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        !h.runner.step().await.unwrap(),
        "a withdrawn job is never run"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_reported_loss_character_is_recorded() {
    let h = harness_with(Some(Arc::new(
        FakeDeriver::default().reporting(LossCharacter::StructurallyEquivalent),
    )))
    .await;
    let _queue = exclusive_queue(h.pool()).await;
    let parent = h.import(&cue_disc("loss")).await;
    h.derive(&parent, json!({})).await;
    assert!(h.runner.step().await.unwrap());
    let (_, lineage) = h
        .send("GET", &format!("/api/v1/artifacts/{parent}/lineage"), None)
        .await;
    assert_eq!(
        lineage["derivatives"][0]["loss_character"],
        "structurally_equivalent"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_transformation_the_engine_cannot_run_is_refused_with_its_reason() {
    let h = harness_with(Some(Arc::new(
        FakeDeriver::default().refusing(tangible_domain::Transformation::ChdCreateCd),
    )))
    .await;
    let _queue = exclusive_queue(h.pool()).await;
    let parent = h.import(&cue_disc("unsupported")).await;
    let (status, body) = h.derive(&parent, json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["detail"].as_str().unwrap().contains("told not to run"),
        "{body}"
    );
    let (_, lineage) = h
        .send("GET", &format!("/api/v1/artifacts/{parent}/lineage"), None)
        .await;
    assert_eq!(lineage["jobs"], json!([]));
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_server_without_an_engine_refuses_to_derive() {
    let dir = TempDir::new().unwrap();
    let objects = FilesystemStore::open(dir.path().join("library"))
        .await
        .unwrap();
    let manifests = ManifestStore::open(objects).await.unwrap();
    let database = support::database().await;
    let session = support::administrator(database.pool()).await;
    let state = ApiState::with_manifests(database, manifests);
    let router = support::signed_in(router(state), session);
    let (status, body) = support_send(
        &router,
        "POST",
        "/api/v1/artifacts/01a1088f-0165-7652-819b-cac12fcbe043/derivatives",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

/// A CloneCD set of the mixed-mode fixture: its descriptor, an IMG of 750
/// sectors (the first a mode 1 data sector, tagged so the image is this
/// test's own) and a SUB.
fn clonecd_disc(tag: &str) -> Vec<(&'static str, Vec<u8>)> {
    let descriptor = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/images/ccd/mixed-mode.ccd"
    ))
    .unwrap();
    let mut image = vec![0_u8; RAW * 750];
    image[..12].copy_from_slice(&[
        0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0,
    ]);
    image[15] = 1;
    image[16..16 + tag.len()].copy_from_slice(tag.as_bytes());
    vec![
        ("Game.ccd", descriptor),
        ("Game.img", image),
        ("Game.sub", vec![0_u8; 96 * 750]),
    ]
}

/// The stripped-down shape of a manifest's tracks, without component ids.
fn track_shapes(manifest: &Value) -> Vec<Value> {
    manifest["topology"]["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            json!([
                t["number"],
                t["mode"],
                t["file_offset_bytes"],
                t["start_lba"],
                t["sector_count"],
                t["pregap_sectors"],
                t["indexes"],
                t["isrc"],
                t["flags"]
            ])
        })
        .collect()
}

fn component<'a>(manifest: &'a Value, name: &str) -> &'a Value {
    manifest["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["logical_path"] == name)
        .unwrap_or_else(|| panic!("{name} in {manifest}"))
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
#[allow(clippy::too_many_lines)]
async fn a_clonecd_image_becomes_a_cue_bin_that_drops_its_subchannel_and_says_so() {
    // No engine: Tangible makes this one itself.
    let h = harness_using(None).await;
    let _queue = exclusive_queue(h.pool()).await;
    let parent = h
        .import(&clonecd_disc(&uuid::Uuid::now_v7().simple().to_string()))
        .await;
    let (_, before) = h
        .send("GET", &format!("/api/v1/artifacts/{parent}/manifest"), None)
        .await;
    let (_, lineage) = h
        .send("GET", &format!("/api/v1/artifacts/{parent}/lineage"), None)
        .await;
    assert_eq!(
        lineage["suggested_transformation"], "cue_bin_from_ccd",
        "{lineage}"
    );

    let (status, asked) = h.derive(&parent, json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{asked}");
    assert_eq!(asked["job"]["transformation"], "cue_bin_from_ccd");
    assert_eq!(asked["job"]["tool_name"], "tangible");
    assert_eq!(asked["job"]["options"], json!({}));
    let job = asked["job"]["id"].as_str().unwrap().to_owned();
    assert!(h.runner.step().await.unwrap(), "the runner found the job");
    let (_, done) = h
        .send("GET", &format!("/api/v1/derivation-jobs/{job}"), None)
        .await;
    assert_eq!(done["state"], "complete", "{done}");
    let child = done["child_artifact_id"].as_str().unwrap().to_owned();

    let (_, manifest) = h
        .send("GET", &format!("/api/v1/artifacts/{child}/manifest"), None)
        .await;
    assert_eq!(manifest["classification"]["format"], "cue_bin");
    assert_eq!(manifest["classification"]["media_family"], "cd");
    assert_eq!(manifest["validation"]["state"], "valid");
    assert_eq!(manifest["lineage"]["transformation"], "cue_bin_from_ccd");
    assert_eq!(manifest["lineage"]["loss_character"], "lossy");
    assert_eq!(manifest["lineage"]["parent_artifact_id"], parent.as_str());
    let evidence = manifest["classification"]["detectors"][0]["evidence"].to_string();
    assert!(
        evidence.contains("not carried: the subchannel"),
        "{evidence}"
    );
    assert_eq!(manifest["topology"]["subchannel"]["present"], false);
    assert_eq!(manifest["topology"]["catalog"], "0123456789012");
    assert_eq!(
        track_shapes(&manifest),
        track_shapes(&before),
        "the same tracks"
    );

    // The BIN is the IMG, the same stored object.
    let bin = component(&manifest, "Game.bin");
    assert_eq!(bin["role"], "track_data");
    assert_eq!(
        bin["content"]["sha256"],
        component(&before, "Game.img")["content"]["sha256"]
    );
    let cue = component(&manifest, "Game.cue");
    assert_eq!(cue["role"], "descriptor");
    let digest: tangible_domain::Sha256Digest =
        cue["content"]["sha256"].as_str().unwrap().parse().unwrap();
    let sheet = std::fs::read_to_string(h.objects.object_path(&digest)).unwrap();
    assert_eq!(
        sheet,
        "CATALOG 0123456789012\nFILE \"Game.bin\" BINARY\n  TRACK 01 MODE1/2352\n    \
         INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    ISRC USRC17607839\n    INDEX 00 00:04:00\n    \
         INDEX 01 00:06:00\n  TRACK 03 AUDIO\n    FLAGS DCP\n    INDEX 01 00:08:00\n"
    );

    // Catalogued, with no subchannel left to stop a burn.
    let (status, artifact) = h
        .send("GET", &format!("/api/v1/artifacts/{child}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{artifact}");
    let facts = tangible_db::repositories::artifact_burn_facts(
        h.pool(),
        uuid::Uuid::parse_str(&child).unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!facts.has_subchannel);

    // Asked again: the derivative already made.
    let (status, again) = h.derive(&parent, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["result"], "exists");

    // It takes no options, and with no engine a CHD is not offered.
    let (status, refused) = h
        .derive(
            &parent,
            json!({ "transformation": "cue_bin_from_ccd", "hunk_bytes": 19584 }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert!(
        refused["detail"]
            .as_str()
            .unwrap()
            .contains("takes no options"),
        "{refused}"
    );
    let (_, lineage) = h
        .send("GET", &format!("/api/v1/artifacts/{child}/lineage"), None)
        .await;
    assert_eq!(
        lineage["suggested_transformation"],
        Value::Null,
        "{lineage}"
    );
    let (status, refused) = h.derive(&child, json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert!(
        refused["detail"]
            .as_str()
            .unwrap()
            .contains("TANGIBLE_DERIVATION_ENGINE"),
        "{refused}"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_cue_bin_of_a_clonecd_image_can_be_made_into_a_chd() {
    let h = harness().await;
    let _queue = exclusive_queue(h.pool()).await;
    let parent = h
        .import(&clonecd_disc(&uuid::Uuid::now_v7().simple().to_string()))
        .await;
    // chdman does not read CloneCD: the CUE/BIN is what suits it.
    let (_, lineage) = h
        .send("GET", &format!("/api/v1/artifacts/{parent}/lineage"), None)
        .await;
    assert_eq!(lineage["suggested_transformation"], "cue_bin_from_ccd");
    let (status, refused) = h
        .derive(&parent, json!({ "transformation": "chd_create_cd" }))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");

    let (_, asked) = h.derive(&parent, json!({})).await;
    assert!(h.runner.step().await.unwrap());
    let job = asked["job"]["id"].as_str().unwrap().to_owned();
    let (_, done) = h
        .send("GET", &format!("/api/v1/derivation-jobs/{job}"), None)
        .await;
    let child = done["child_artifact_id"].as_str().unwrap().to_owned();
    let (_, lineage) = h
        .send("GET", &format!("/api/v1/artifacts/{child}/lineage"), None)
        .await;
    assert_eq!(
        lineage["suggested_transformation"], "chd_create_cd",
        "{lineage}"
    );
    let (status, chd) = h.derive(&child, json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{chd}");
    assert!(h.runner.step().await.unwrap());
    let chd_job = chd["job"]["id"].as_str().unwrap().to_owned();
    let (_, chd_done) = h
        .send("GET", &format!("/api/v1/derivation-jobs/{chd_job}"), None)
        .await;
    assert_eq!(chd_done["state"], "complete", "{chd_done}");
}

fn mds_fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/../../fixtures/images/mds/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// `sectors` raw sectors, the first a mode 1 data sector tagged so the image
/// is this test's own, each followed by `subchannel` bytes of 0xAA.
fn raw_sectors(tag: &str, sectors: usize, subchannel: usize) -> (Vec<u8>, Vec<u8>) {
    let mut main = vec![0_u8; RAW * sectors];
    main[..12].copy_from_slice(&[
        0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0,
    ]);
    main[15] = 1;
    main[16..16 + tag.len()].copy_from_slice(tag.as_bytes());
    for (sector, bytes) in main.chunks_exact_mut(RAW).enumerate().skip(1) {
        bytes[..8].copy_from_slice(&(sector as u64).to_le_bytes());
    }
    let mut interleaved = Vec::with_capacity((RAW + subchannel) * sectors);
    for bytes in main.chunks_exact(RAW) {
        interleaved.extend_from_slice(bytes);
        interleaved.resize(interleaved.len() + subchannel, 0xAA);
    }
    (main, interleaved)
}

/// A minimal ISO of 64 2048-byte blocks, tagged in its last block.
fn iso_volume(tag: &str) -> Vec<u8> {
    let mut bytes = vec![0_u8; 16 * 2048];
    let mut pvd = vec![0_u8; 2048];
    pvd[0] = 1;
    pvd[1..6].copy_from_slice(b"CD001");
    pvd[6] = 1;
    pvd[40..72].fill(b' ');
    pvd[40..49].copy_from_slice(b"FILM_DISC");
    pvd[80..84].copy_from_slice(&64_u32.to_le_bytes());
    pvd[84..88].copy_from_slice(&64_u32.to_be_bytes());
    pvd[128..130].copy_from_slice(&2048_u16.to_le_bytes());
    pvd[130..132].copy_from_slice(&2048_u16.to_be_bytes());
    bytes.extend_from_slice(&pvd);
    let mut terminator = vec![0_u8; 2048];
    terminator[0] = 255;
    terminator[1..6].copy_from_slice(b"CD001");
    terminator[6] = 1;
    bytes.extend_from_slice(&terminator);
    bytes.resize(64 * 2048, 0);
    bytes[63 * 2048..63 * 2048 + tag.len()].copy_from_slice(tag.as_bytes());
    bytes
}

/// Ask for the derivative that suits `parent`, run it, and answer the
/// child's id and manifest.
async fn derive_and_run(h: &Harness, parent: &str, transformation: &str) -> (String, Value) {
    let (_, lineage) = h
        .send("GET", &format!("/api/v1/artifacts/{parent}/lineage"), None)
        .await;
    assert_eq!(
        lineage["suggested_transformation"], transformation,
        "{lineage}"
    );
    let (status, asked) = h.derive(parent, json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{asked}");
    assert_eq!(asked["job"]["transformation"], transformation);
    assert_eq!(asked["job"]["tool_name"], "tangible");
    let job = asked["job"]["id"].as_str().unwrap().to_owned();
    assert!(h.runner.step().await.unwrap(), "the runner found the job");
    let (_, done) = h
        .send("GET", &format!("/api/v1/derivation-jobs/{job}"), None)
        .await;
    assert_eq!(done["state"], "complete", "{done}");
    let child = done["child_artifact_id"].as_str().unwrap().to_owned();
    let (_, manifest) = h
        .send("GET", &format!("/api/v1/artifacts/{child}/manifest"), None)
        .await;
    assert_eq!(manifest["lineage"]["transformation"], transformation);
    assert_eq!(manifest["lineage"]["loss_character"], "lossy");
    assert_eq!(manifest["lineage"]["parent_artifact_id"], parent);
    (child, manifest)
}

fn stored_text(h: &Harness, component: &Value) -> Vec<u8> {
    let digest: tangible_domain::Sha256Digest = component["content"]["sha256"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    std::fs::read(h.objects.object_path(&digest)).unwrap()
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_mds_cd_becomes_a_cue_bin_whose_bin_is_its_mdf() {
    let h = harness_using(None).await;
    let _queue = exclusive_queue(h.pool()).await;
    let (image, _) = raw_sectors(&uuid::Uuid::now_v7().simple().to_string(), 450, 0);
    let parent = h
        .import(&[
            ("Game.mds", mds_fixture("mixed-mode.mds")),
            ("Game.mdf", image),
        ])
        .await;
    let (_, before) = h
        .send("GET", &format!("/api/v1/artifacts/{parent}/manifest"), None)
        .await;
    let (_, manifest) = derive_and_run(&h, &parent, "cue_bin_from_mds").await;

    assert_eq!(manifest["classification"]["format"], "cue_bin");
    assert_eq!(manifest["classification"]["media_family"], "cd");
    assert_eq!(manifest["validation"]["state"], "valid");
    let evidence = manifest["classification"]["detectors"][0]["evidence"].to_string();
    assert!(
        evidence.contains("not carried: the descriptor's raw table of contents"),
        "{evidence}"
    );
    assert_eq!(track_shapes(&manifest), track_shapes(&before));
    let bin = component(&manifest, "Game.bin");
    assert_eq!(
        bin["content"]["sha256"],
        component(&before, "Game.mdf")["content"]["sha256"],
        "the BIN is the MDF, the same stored object"
    );
    let sheet = String::from_utf8(stored_text(&h, component(&manifest, "Game.cue"))).unwrap();
    assert!(sheet.starts_with("FILE \"Game.bin\" BINARY\n"), "{sheet}");
    assert!(
        sheet.contains("PREGAP 00:02:00"),
        "track 2's pregap: {sheet}"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_mds_cd_with_subchannel_becomes_a_cue_bin_without_it() {
    let h = harness_using(None).await;
    let _queue = exclusive_queue(h.pool()).await;
    let (main, interleaved) = raw_sectors(&uuid::Uuid::now_v7().simple().to_string(), 100, 96);
    let parent = h
        .import(&[
            ("Sub.mds", mds_fixture("subchannel.mds")),
            ("Sub.mdf", interleaved),
        ])
        .await;
    let (_, before) = h
        .send("GET", &format!("/api/v1/artifacts/{parent}/manifest"), None)
        .await;
    assert_eq!(before["topology"]["kind"], "unknown", "imported whole");

    let (_, manifest) = derive_and_run(&h, &parent, "cue_bin_from_mds").await;
    let evidence = manifest["classification"]["detectors"][0]["evidence"].to_string();
    assert!(
        evidence.contains("not carried: the subchannel and the descriptor's raw table of contents"),
        "{evidence}"
    );
    assert_eq!(
        track_shapes(&manifest),
        [json!([1, "MODE1/2352", 0, 0, 100, 0, [{ "number": 1, "relative_lba": 0 }], null, []])]
    );
    let bin = component(&manifest, "Sub.bin");
    assert_eq!(bin["length_bytes"], RAW * 100);
    assert!(
        stored_text(&h, bin) == main,
        "each sector's first 2352 bytes, nothing else"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_mds_dvd_becomes_the_iso_its_mdf_is() {
    let h = harness_using(None).await;
    let _queue = exclusive_queue(h.pool()).await;
    let parent = h
        .import(&[
            ("Film.mds", mds_fixture("dvd.mds")),
            (
                "Film.mdf",
                iso_volume(&uuid::Uuid::now_v7().simple().to_string()),
            ),
        ])
        .await;
    let (_, before) = h
        .send("GET", &format!("/api/v1/artifacts/{parent}/manifest"), None)
        .await;
    let (child, manifest) = derive_and_run(&h, &parent, "iso_from_mds").await;

    assert_eq!(manifest["classification"]["format"], "iso");
    assert_eq!(manifest["classification"]["media_family"], "dvd");
    assert_eq!(manifest["validation"]["state"], "valid");
    assert_eq!(manifest["topology"]["kind"], "single_track_block_image");
    assert_eq!(manifest["topology"]["block_count"], 64);
    assert_eq!(manifest["topology"]["volume_labels"], json!(["FILM_DISC"]));
    let iso = component(&manifest, "Film.iso");
    assert_eq!(iso["role"], "primary_image");
    assert_eq!(
        iso["content"]["sha256"],
        component(&before, "Film.mdf")["content"]["sha256"],
        "the ISO is the MDF, the same stored object"
    );

    // An ISO burns; nothing of the MDS is left to refuse it.
    let facts = tangible_db::repositories::artifact_burn_facts(
        h.pool(),
        uuid::Uuid::parse_str(&child).unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(facts.format, "iso");
}

/// A CHD fixture with this test's own bytes appended past its last
/// structure, so its content is this run's own.
fn tagged_chd(name: &str) -> Vec<u8> {
    let mut bytes = std::fs::read(format!(
        "{}/../../fixtures/images/chd/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    bytes.extend_from_slice(uuid::Uuid::now_v7().simple().to_string().as_bytes());
    bytes
}

/// Ask for what suits `parent`, run it, and answer the job as it ended.
async fn ask_and_run(h: &Harness, parent: &str, transformation: &str) -> Value {
    let (_, lineage) = h
        .send("GET", &format!("/api/v1/artifacts/{parent}/lineage"), None)
        .await;
    assert_eq!(
        lineage["suggested_transformation"], transformation,
        "{lineage}"
    );
    let (status, asked) = h.derive(parent, json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{asked}");
    assert_eq!(asked["job"]["transformation"], transformation);
    assert_eq!(asked["job"]["options"], json!({}));
    let job = asked["job"]["id"].as_str().unwrap().to_owned();
    assert!(h.runner.step().await.unwrap(), "the runner found the job");
    let (_, done) = h
        .send("GET", &format!("/api/v1/derivation-jobs/{job}"), None)
        .await;
    done
}

fn checked_engine() -> Arc<dyn DerivationEngine> {
    Arc::new(FakeDeriver::default().reporting(LossCharacter::StructurallyEquivalent))
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_cd_chd_becomes_the_cue_bin_chdman_extracts() {
    let h = harness_with(Some(checked_engine())).await;
    let _queue = exclusive_queue(h.pool()).await;
    let parent = h
        .import(&[("Game.chd", tagged_chd("mixed-mode.chd"))])
        .await;
    let done = ask_and_run(&h, &parent, "cue_bin_from_chd").await;
    assert_eq!(done["state"], "complete", "{done}");
    let child = done["child_artifact_id"].as_str().unwrap().to_owned();

    let (_, manifest) = h
        .send("GET", &format!("/api/v1/artifacts/{child}/manifest"), None)
        .await;
    assert_eq!(manifest["classification"]["format"], "cue_bin");
    assert_eq!(manifest["classification"]["media_family"], "cd");
    assert_eq!(manifest["lineage"]["transformation"], "cue_bin_from_chd");
    assert_eq!(
        manifest["lineage"]["loss_character"],
        "structurally_equivalent"
    );
    assert_eq!(manifest["lineage"]["tool"]["name"], "fake");
    assert_eq!(
        manifest["validation"]["validators"][0]["code"],
        "CUE_LAID_OUT"
    );
    assert_eq!(component(&manifest, "Game.cue")["role"], "descriptor");
    assert_eq!(component(&manifest, "Game.bin")["role"], "track_data");
    // The sheet the engine wrote, laid out as an import lays one out.
    assert_eq!(manifest["topology"]["kind"], "cd_tracks");
    assert_eq!(manifest["topology"]["tracks"][0]["mode"], "MODE1/2352");

    // A CUE/BIN burns; the CHD did not.
    let facts = tangible_db::repositories::artifact_burn_facts(
        h.pool(),
        uuid::Uuid::parse_str(&child).unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(facts.format, "cue_bin");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_dvd_chd_becomes_the_iso_chdman_extracts() {
    let h = harness_with(Some(checked_engine())).await;
    let _queue = exclusive_queue(h.pool()).await;
    let parent = h.import(&[("Film.chd", tagged_chd("dvd.chd"))]).await;
    let done = ask_and_run(&h, &parent, "iso_from_chd").await;
    assert_eq!(done["state"], "complete", "{done}");
    let child = done["child_artifact_id"].as_str().unwrap().to_owned();
    let (_, manifest) = h
        .send("GET", &format!("/api/v1/artifacts/{child}/manifest"), None)
        .await;
    assert_eq!(manifest["classification"]["format"], "iso");
    assert_eq!(manifest["classification"]["media_family"], "dvd");
    assert_eq!(manifest["topology"]["kind"], "single_track_block_image");
    assert_eq!(manifest["topology"]["volume_labels"], json!(["FAKE"]));
    assert_eq!(component(&manifest, "Film.iso")["role"], "primary_image");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_delta_chd_is_not_extracted_and_says_why() {
    let h = harness_with(Some(checked_engine())).await;
    let _queue = exclusive_queue(h.pool()).await;
    let parent = h.import(&[("delta.chd", tagged_chd("delta.chd"))]).await;
    let done = ask_and_run(&h, &parent, "cue_bin_from_chd").await;
    assert_eq!(done["state"], "failed_terminal", "{done}");
    assert_eq!(done["error_code"], "DERIVATION_NOT_EXPRESSIBLE");
    assert!(
        done["error_detail"]
            .as_str()
            .unwrap()
            .contains("without that parent"),
        "{done}"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_chd_made_here_is_not_offered_extraction() {
    let h = harness().await;
    let _queue = exclusive_queue(h.pool()).await;
    let parent = h
        .import(&cue_disc(&uuid::Uuid::now_v7().simple().to_string()))
        .await;
    let (_, asked) = h.derive(&parent, json!({})).await;
    assert_eq!(asked["job"]["transformation"], "chd_create_cd");
    assert!(h.runner.step().await.unwrap());
    let job = asked["job"]["id"].as_str().unwrap().to_owned();
    let (_, done) = h
        .send("GET", &format!("/api/v1/derivation-jobs/{job}"), None)
        .await;
    let chd = done["child_artifact_id"].as_str().unwrap().to_owned();
    // What it was made from is the CUE/BIN; extracting it is not offered,
    // and an empty request finds nothing to do.
    let (_, lineage) = h
        .send("GET", &format!("/api/v1/artifacts/{chd}/lineage"), None)
        .await;
    assert_eq!(
        lineage["suggested_transformation"],
        Value::Null,
        "{lineage}"
    );
    let (status, refused) = h.derive(&chd, json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
}

/// chdman itself, when `TANGIBLE_CHDMAN_TESTS=1` (and `TANGIBLE_CHDMAN_BIN`
/// if it is not on `PATH`), as crates/image/tests/chdman.rs runs it.
async fn real_chdman() -> Option<Arc<dyn DerivationEngine>> {
    if std::env::var("TANGIBLE_CHDMAN_TESTS").unwrap_or_default() != "1" {
        eprintln!("skipping: set TANGIBLE_CHDMAN_TESTS=1 to run it with chdman");
        return None;
    }
    let program = std::env::var("TANGIBLE_CHDMAN_BIN").unwrap_or_else(|_| "chdman".to_owned());
    Some(Arc::new(
        tangible_image::chdman::Chdman::probe(program)
            .await
            .expect("chdman reports a version"),
    ))
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn chdman_extracts_a_cd_chd_and_a_dvd_chd_end_to_end() {
    let Some(engine) = real_chdman().await else {
        return;
    };
    let h = harness_with(Some(engine)).await;
    let _queue = exclusive_queue(h.pool()).await;

    let parent = h
        .import(&[("Game.chd", tagged_chd("mixed-mode.chd"))])
        .await;
    let done = ask_and_run(&h, &parent, "cue_bin_from_chd").await;
    assert_eq!(done["state"], "complete", "{done}");
    let child = done["child_artifact_id"].as_str().unwrap().to_owned();
    let (_, manifest) = h
        .send("GET", &format!("/api/v1/artifacts/{child}/manifest"), None)
        .await;
    assert_eq!(
        manifest["lineage"]["loss_character"],
        "structurally_equivalent"
    );
    assert_eq!(manifest["lineage"]["tool"]["name"], "chdman");
    let shapes = track_shapes(&manifest);
    assert_eq!(shapes.len(), 3, "{manifest}");
    // Track 2's pregap was in the CHD's data, and is in the BIN.
    assert_eq!(manifest["topology"]["tracks"][1]["pregap_sectors"], 150);
    assert_eq!(component(&manifest, "Game.bin")["length_bytes"], 750 * RAW);

    let parent = h.import(&[("Film.chd", tagged_chd("dvd.chd"))]).await;
    let done = ask_and_run(&h, &parent, "iso_from_chd").await;
    assert_eq!(done["state"], "complete", "{done}");
    let child = done["child_artifact_id"].as_str().unwrap().to_owned();
    let (_, manifest) = h
        .send("GET", &format!("/api/v1/artifacts/{child}/manifest"), None)
        .await;
    assert_eq!(
        manifest["lineage"]["loss_character"],
        "structurally_equivalent"
    );
    assert_eq!(
        manifest["topology"]["volume_labels"],
        json!(["TANGIBLE_DVD"])
    );
    assert_eq!(manifest["topology"]["block_count"], 64);
}
