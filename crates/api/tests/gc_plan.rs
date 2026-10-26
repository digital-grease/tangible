// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The garbage collection planner (docs/plans/storage-integrity.md, phase
//! S2), against PostgreSQL and real files: what it marks live, what it
//! would collect, what it only counts, and that it removes nothing.
//!
//! The shared test database records other tests' objects too, whose files
//! are not in this test's library, so what is asserted is said of this
//! test's own objects, never as a total.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::io::Cursor;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use tangible_api::gc::{GcPlan, GcSettings, plan};
use tangible_api::{ApiState, ImportCheckpoint, ImportPipeline, ImportRequest, catalog, router};
use tangible_domain::{ArtifactId, ImportJobId, LogicalPath, Role, Sha256Digest};
use tangible_storage::{FilesystemStore, IngestLimits, ManifestStore, StagingManager};
use tempfile::TempDir;
use tower::ServiceExt as _;

mod support;

const LIMITS: IngestLimits = IngestLimits {
    max_bytes: None,
    fsync: false,
};

struct Harness {
    _dir: TempDir,
    pool: PgPool,
    database: tangible_db::Database,
    pipeline: ImportPipeline,
    objects: FilesystemStore,
    manifests: ManifestStore,
}

async fn harness() -> Harness {
    let database = support::database().await;
    let dir = TempDir::new().unwrap();
    let objects = FilesystemStore::open(dir.path().join("library"))
        .await
        .unwrap();
    let manifests = ManifestStore::open(objects.clone()).await.unwrap();
    let staging = StagingManager::open(dir.path().join("staging"))
        .await
        .unwrap();
    Harness {
        pool: database.pool().clone(),
        pipeline: ImportPipeline::new(staging, objects.clone(), manifests.clone()),
        database,
        objects,
        manifests,
        _dir: dir,
    }
}

/// Bytes no other test or run has.
fn fresh(size: usize) -> Vec<u8> {
    let seed = uuid::Uuid::now_v7();
    (0..size)
        .map(|i| seed.as_bytes()[i % 16].wrapping_add(u8::try_from(i % 251).unwrap()))
        .collect()
}

impl Harness {
    /// An artifact of one fresh file, its manifest published and, when
    /// asked, catalogued: its id and its object.
    async fn import(&self, catalogue: bool) -> (ArtifactId, Sha256Digest) {
        let import_id = ImportJobId::generate();
        let area = self.pipeline.open_area(import_id).await.unwrap();
        area.write(&LogicalPath::parse("disc.iso").unwrap(), &fresh(4096))
            .await
            .unwrap();
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
                    limits: LIMITS,
                },
                &mut ImportCheckpoint::default(),
            )
            .await
            .unwrap();
        let manifest = self.manifests.read(outcome.artifact_id).await.unwrap();
        if catalogue {
            catalog::register_manifest(&self.database, &manifest)
                .await
                .unwrap();
        }
        (outcome.artifact_id, manifest.components[0].content.sha256)
    }

    /// An object nothing catalogued, as an import that never finished
    /// leaves one.
    async fn orphan(&self) -> Sha256Digest {
        self.objects
            .put_stream(Cursor::new(fresh(1000)), LIMITS)
            .await
            .unwrap()
            .digest
    }

    /// An import in `state` whose checkpoint says it promoted `digest`.
    /// Ended or quarantined, so no runner of another test claims it.
    async fn import_job(&self, state: &str, digest: Sha256Digest) {
        let checkpoint = json!({
            "state": "inspecting",
            "promoted": { "disc.iso": { "digest": digest.to_hex(), "length_bytes": 1000 } },
            "artifact_id": null,
            "detected_format": null,
            "confidence": 0.0,
            "warnings": [],
            "evidence": []
        });
        // The checkpoint the runner would read, so the test cannot pass on
        // a shape the runner does not write.
        serde_json::from_value::<ImportCheckpoint>(checkpoint.clone()).unwrap();
        sqlx::query(
            "INSERT INTO import_jobs (id, source_kind, source_descriptor_json, state,
                 checkpoint_json, created_by)
             VALUES ($1, 'upload', '{}', $2, $3, 'gc-test')",
        )
        .bind(*ImportJobId::generate().as_uuid())
        .bind(state)
        .bind(checkpoint)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn plan(&self, grace_days: u32) -> GcPlan {
        plan(
            &self.database,
            &self.manifests,
            GcSettings::days(grace_days, usize::MAX),
        )
        .await
        .unwrap()
    }
}

fn candidates(plan: &GcPlan) -> Vec<String> {
    plan.candidates.iter().map(|c| c.sha256.clone()).collect()
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_plan_tells_what_is_live_from_what_nothing_needs_and_removes_nothing() {
    let h = harness().await;
    let (_, catalogued) = h.import(true).await;
    let (_, published) = h.import(false).await;
    let orphan = h.orphan().await;
    let held = h.orphan().await;
    h.import_job("quarantined", held).await;
    let abandoned = h.orphan().await;
    h.import_job("canceled", abandoned).await;
    let (_, lost) = h.import(true).await;
    std::fs::remove_file(h.objects.object_path(&lost)).unwrap();
    std::fs::write(h.objects.root().join("objects/incoming/left.part"), b"half").unwrap();

    // Everything here was written just now, so a week's grace keeps it all.
    let week = h.plan(7).await;
    assert!(candidates(&week).is_empty(), "{:?}", week.candidates);
    assert_eq!(week.stored.count, 5);
    assert_eq!(week.recent.count, 2, "the orphan and the abandoned one");
    assert_eq!(week.live.count, 3, "catalogued, published and held");
    assert_eq!(week.partial_files.count, 1);
    assert_eq!(week.stale_partial_files, 0);
    assert_eq!(week.manifests_unreadable, 0);
    assert!(week.missing_digests.contains(&lost.to_hex()));

    // With none, the two nothing needs are what collection would remove.
    let now = h.plan(0).await;
    let mut expected = vec![orphan.to_hex(), abandoned.to_hex()];
    expected.sort();
    let mut found = candidates(&now);
    found.sort();
    assert_eq!(found, expected);
    assert!(now.candidates.iter().all(|c| !c.recorded));
    assert_eq!(now.collectable.count, 2);
    assert_eq!(now.collectable.bytes, 2000);
    assert_eq!(now.stale_partial_files, 1);
    for digest in [catalogued, published, held] {
        assert!(!found.contains(&digest.to_hex()));
    }

    // Nothing was removed.
    for digest in [catalogued, published, orphan, held, abandoned] {
        assert!(h.objects.object_path(&digest).exists());
    }
    assert!(h.objects.root().join("objects/incoming/left.part").exists());
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_manifest_that_cannot_be_read_makes_the_plan_uncertain() {
    let h = harness().await;
    std::fs::write(
        h.manifests.manifest_path(ArtifactId::generate()),
        b"{ not a manifest",
    )
    .unwrap();
    let plan = h.plan(7).await;
    assert_eq!(plan.manifests_unreadable, 1);
    assert!(!plan.certain);
}

async fn get(state: ApiState, path: &str, session: &support::Session) -> (StatusCode, Value) {
    let mut request = Request::get(path).body(Body::empty()).unwrap();
    session.apply(&mut request);
    let response = router(state).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 16 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn administrators_get_the_plan_and_nobody_else_does() {
    let h = harness().await;
    let orphan = h.orphan().await;
    let state = ApiState::with_manifests(h.database.clone(), h.manifests.clone());
    let admin = support::administrator(&h.pool).await;

    let (status, body) = get(
        state.clone(),
        "/api/v1/storage/gc-plan?grace_days=0&limit=5",
        &admin,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["grace_seconds"], 0);
    assert_eq!(body["candidates"][0]["sha256"], orphan.to_hex());
    assert_eq!(body["candidates"][0]["recorded"], false);
    assert_eq!(body["stored"]["count"], 1);
    assert!(body["missing_digests"].as_array().unwrap().len() <= 5);

    let (status, body) = get(state.clone(), "/api/v1/storage/gc-plan", &admin).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["grace_seconds"], 7 * 86_400);
    assert_eq!(body["recent"]["count"], 1);

    let (status, body) = get(
        state.clone(),
        "/api/v1/storage/gc-plan?grace_days=3651",
        &admin,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "INVALID_PARAMETER");

    for role in [Role::Viewer, Role::Operator] {
        let session = support::sign_in(&h.pool, role).await;
        let (status, body) = get(state.clone(), "/api/v1/storage/gc-plan", &session).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{role:?}: {body}");
    }

    let (status, body) = get(
        ApiState::new(h.database.clone()),
        "/api/v1/storage/gc-plan",
        &admin,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["code"], "STORAGE_UNAVAILABLE");
    assert!(h.objects.object_path(&orphan).exists());
}
