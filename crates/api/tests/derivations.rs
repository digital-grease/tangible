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

    let derivations = Derivations::new(DerivationPipeline::new(
        staging,
        objects.clone(),
        manifests.clone(),
        engine.unwrap_or_else(|| Arc::new(FakeDeriver::default())),
    ));
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
