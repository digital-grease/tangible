// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Integrity checks (docs/plans/storage-integrity.md), through the real
//! router, runner and files, damaged on purpose: a byte flipped, a file
//! removed, a file cut short, a manifest edited.
//!
//! A full scrub reads every object the shared test database records, other
//! tests' too, whose files are not in this test's library; so what a scrub
//! is asserted to find is said of this test's own artifacts, never as a
//! total.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::PathBuf;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::Value;
use sqlx::PgPool;
use tangible_api::integrity::{Integrity, IntegrityRunner, IntegrityRunnerSettings};
use tangible_api::{ApiState, ImportCheckpoint, ImportPipeline, ImportRequest, catalog, router};
use tangible_domain::{ImportJobId, LogicalPath, Role};
use tangible_storage::{FilesystemStore, IngestLimits, ManifestStore, StagingManager};
use tempfile::TempDir;
use tower::ServiceExt as _;

mod support;

static QUEUE: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

struct Harness {
    dir: TempDir,
    pool: PgPool,
    database: tangible_db::Database,
    state: ApiState,
    pipeline: ImportPipeline,
    objects: FilesystemStore,
    manifests: ManifestStore,
    runner: IntegrityRunner,
    admin: support::Session,
    operator: support::Session,
    _queue: tokio::sync::MutexGuard<'static, ()>,
}

async fn harness() -> Harness {
    let queue = QUEUE
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let database = support::database().await;
    let pool = database.pool().clone();
    let dir = TempDir::new().unwrap();
    let objects = FilesystemStore::open(dir.path().join("library"))
        .await
        .unwrap();
    let manifests = ManifestStore::open(objects.clone()).await.unwrap();
    let staging = StagingManager::open(dir.path().join("staging"))
        .await
        .unwrap();
    let pipeline = ImportPipeline::new(staging, objects.clone(), manifests.clone());
    let integrity = Integrity::new();
    let runner = IntegrityRunner::new(
        database.clone(),
        objects.clone(),
        manifests.clone(),
        integrity.clone(),
        IntegrityRunnerSettings {
            owner: format!("test-{}", uuid::Uuid::now_v7()),
            ..IntegrityRunnerSettings::default()
        },
    );
    // Checks an earlier run left queued name libraries that are gone.
    sqlx::query(
        "UPDATE integrity_checks SET state = 'canceled', lease_owner = NULL,
             lease_expires_at = NULL
         WHERE state IN ('queued', 'running')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let state =
        ApiState::with_manifests(database.clone(), manifests.clone()).with_integrity(integrity);
    Harness {
        admin: support::administrator(&pool).await,
        operator: support::sign_in(&pool, Role::Operator).await,
        dir,
        pool,
        database,
        state,
        pipeline,
        objects,
        manifests,
        runner,
        _queue: queue,
    }
}

impl Harness {
    /// An artifact of fresh random files, catalogued: its id and its
    /// objects' paths.
    async fn import(&self, files: &[(&str, usize)]) -> (String, Vec<PathBuf>) {
        let import_id = ImportJobId::generate();
        let area = self.pipeline.open_area(import_id).await.unwrap();
        for (name, size) in files {
            // Fresh bytes each time, so no two artifacts share an object.
            let seed = uuid::Uuid::now_v7();
            let bytes: Vec<u8> = (0..*size)
                .map(|i| seed.as_bytes()[i % 16].wrapping_add(u8::try_from(i % 251).unwrap()))
                .collect();
            area.write(&LogicalPath::parse(name).unwrap(), &bytes)
                .await
                .unwrap();
        }
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
                &mut ImportCheckpoint::default(),
            )
            .await
            .unwrap();
        let manifest = self.manifests.read(outcome.artifact_id).await.unwrap();
        catalog::register_manifest(&self.database, &manifest)
            .await
            .unwrap();
        let paths = manifest
            .components
            .iter()
            .map(|component| self.objects.object_path(&component.content.sha256))
            .collect();
        (outcome.artifact_id.to_string(), paths)
    }

    async fn send(
        &self,
        session: &support::Session,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
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
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn validate(&self, artifact: &str) -> Value {
        let (status, check) = self
            .send(
                &self.operator,
                Method::POST,
                &format!("/api/v1/artifacts/{artifact}/validate"),
                None,
            )
            .await;
        assert!(status.is_success(), "{check}");
        check
    }

    async fn run_all(&self) {
        for _ in 0..10 {
            if !self.runner.step().await.unwrap() {
                return;
            }
        }
    }

    async fn check(&self, id: &str) -> Value {
        self.send(
            &self.admin,
            Method::GET,
            &format!("/api/v1/integrity-checks/{id}"),
            None,
        )
        .await
        .1
    }

    async fn artifact(&self, id: &str) -> Value {
        self.send(
            &self.admin,
            Method::GET,
            &format!("/api/v1/artifacts/{id}"),
            None,
        )
        .await
        .1
    }
}

/// Make a read-only object writable, to damage it.
fn writable(path: &std::path::Path) {
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(path, permissions).unwrap();
}

fn flip_a_byte(path: &std::path::Path) {
    writable(path);
    let mut bytes = std::fs::read(path).unwrap();
    bytes[0] ^= 0xff;
    std::fs::write(path, bytes).unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_intact_artifact_reads_back_intact() {
    let harness = harness().await;
    let (artifact, _) = harness.import(&[("disc.iso", 70_000)]).await;
    assert_eq!(
        harness.artifact(&artifact).await["integrity_state"],
        "unchecked"
    );

    let check = harness.validate(&artifact).await;
    assert_eq!(check["state"], "queued");
    assert_eq!(check["scope"], "artifact");
    harness.run_all().await;
    let done = harness.check(check["id"].as_str().unwrap()).await;
    assert_eq!(done["state"], "complete", "{done}");
    assert_eq!(done["problem_count"], 0);
    assert_eq!(done["objects_total"], 1);
    assert_eq!(done["objects_checked"], 1);
    assert_eq!(done["bytes_checked"], 70_000);
    let view = harness.artifact(&artifact).await;
    assert_eq!(view["integrity_state"], "intact", "{view}");
    assert!(view["integrity_checked_at"].is_string());
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn damage_is_found_named_and_never_repaired() {
    let harness = harness().await;
    let (flipped, flipped_paths) = harness.import(&[("a.iso", 5_000)]).await;
    let (removed, removed_paths) = harness.import(&[("b.iso", 5_000)]).await;
    let (cut, cut_paths) = harness.import(&[("c.iso", 5_000)]).await;

    flip_a_byte(&flipped_paths[0]);
    writable(&removed_paths[0]);
    std::fs::remove_file(&removed_paths[0]).unwrap();
    writable(&cut_paths[0]);
    std::fs::write(&cut_paths[0], b"short").unwrap();
    let after_flip = std::fs::read(&flipped_paths[0]).unwrap();

    for (artifact, kind) in [
        (&flipped, "corrupt"),
        (&removed, "missing"),
        (&cut, "corrupt"),
    ] {
        let check = harness.validate(artifact).await;
        harness.run_all().await;
        let done = harness.check(check["id"].as_str().unwrap()).await;
        assert_eq!(done["state"], "complete", "{done}");
        assert_eq!(done["problem_count"], 1, "{done}");
        assert_eq!(done["problems"][0]["kind"], kind, "{done}");
        assert!(done["problems"][0]["sha256"].is_string());
        assert!(
            !done
                .to_string()
                .contains(&harness.dir.path().display().to_string()),
            "no path in what a check says: {done}"
        );
        assert_eq!(
            harness.artifact(artifact).await["integrity_state"],
            "damaged"
        );
    }
    assert_eq!(
        std::fs::read(&flipped_paths[0]).unwrap(),
        after_flip,
        "nothing is repaired"
    );

    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action = 'integrity.damage_found'
         AND metadata->>'artifact_id' = $1",
    )
    .bind(&flipped)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(audited, 1);

    // Restored from a backup: the next check finds it intact again.
    writable(&flipped_paths[0]);
    let mut restored = after_flip;
    restored[0] ^= 0xff;
    std::fs::write(&flipped_paths[0], restored).unwrap();
    harness.validate(&flipped).await;
    harness.run_all().await;
    assert_eq!(
        harness.artifact(&flipped).await["integrity_state"],
        "intact"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_manifest_that_disagrees_with_the_catalogue_is_found() {
    let harness = harness().await;
    let (edited, _) = harness.import(&[("x.iso", 2_000)]).await;
    let (gone, _) = harness.import(&[("y.iso", 2_000)]).await;
    let path = |id: &str| {
        harness
            .manifests
            .manifest_path(tangible_domain::ArtifactId::from_uuid(
                uuid::Uuid::parse_str(id).unwrap(),
            ))
    };
    let mut manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(path(&edited)).unwrap()).unwrap();
    manifest["components"][0]["logical_path"] = Value::from("renamed.iso");
    writable(&path(&edited));
    std::fs::write(path(&edited), manifest.to_string()).unwrap();
    writable(&path(&gone));
    std::fs::remove_file(path(&gone)).unwrap();

    for (artifact, kind) in [
        (&edited, "manifest_mismatch"),
        (&gone, "manifest_unreadable"),
    ] {
        let check = harness.validate(artifact).await;
        harness.run_all().await;
        let done = harness.check(check["id"].as_str().unwrap()).await;
        assert_eq!(done["problem_count"], 1, "{artifact}: {done}");
        assert_eq!(done["problems"][0]["kind"], kind, "{done}");
        assert!(done["problems"][0]["sha256"].is_null());
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_full_scrub_marks_what_it_covered() {
    let harness = harness().await;
    let (good, _) = harness.import(&[("good.iso", 3_000)]).await;
    let (bad, bad_paths) = harness
        .import(&[("bad-1.bin", 3_000), ("bad-2.bin", 3_000)])
        .await;
    flip_a_byte(&bad_paths[1]);

    let (status, check) = harness
        .send(
            &harness.admin,
            Method::POST,
            "/api/v1/integrity-checks",
            Some(serde_json::json!({ "scope": "all" })),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{check}");
    let (status, again) = harness
        .send(
            &harness.admin,
            Method::POST,
            "/api/v1/integrity-checks",
            Some(serde_json::json!({ "scope": "all" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "one full scrub at a time: {again}");
    assert_eq!(again["id"], check["id"]);

    harness.run_all().await;
    let done = harness.check(check["id"].as_str().unwrap()).await;
    assert_eq!(done["state"], "complete", "{done}");
    assert!(done["problem_count"].as_i64().unwrap() >= 1);
    assert_eq!(harness.artifact(&good).await["integrity_state"], "intact");
    assert_eq!(harness.artifact(&bad).await["integrity_state"], "damaged");

    let (status, page) = harness
        .send(
            &harness.admin,
            Method::GET,
            "/api/v1/integrity-checks?limit=1",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"][0]["id"], check["id"]);
    assert!(page["next_cursor"].is_string());
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_check_taken_over_resumes_after_the_last_object_it_finished() {
    let harness = harness().await;
    let (artifact, paths) = harness
        .import(&[("one.bin", 1_000), ("two.bin", 1_000), ("three.bin", 1_000)])
        .await;
    let mut digests: Vec<String> = paths
        .iter()
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    digests.sort();
    let check = harness.validate(&artifact).await;
    let id: uuid::Uuid = check["id"].as_str().unwrap().parse().unwrap();

    // Another process took it, finished the first object, and went away.
    let claimed = tangible_db::integrity::claim_next_check(&harness.pool, "gone", 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id.as_uuid(), &id);
    assert!(
        tangible_db::integrity::record_progress(
            &harness.pool,
            &tangible_db::integrity::Progress {
                id: claimed.id,
                owner: "gone",
                objects_checked: 1,
                bytes_checked: 1_000,
                resume_after: Some(&digests[0]),
                new_problems: &[],
                new_problem_count: 0,
                lease_seconds: 1,
            },
        )
        .await
        .unwrap()
    );
    // The object it finished is damaged since: a resumed check does not read
    // it again.
    let first = paths
        .iter()
        .find(|path| path.file_name().unwrap().to_string_lossy() == digests[0])
        .unwrap();
    flip_a_byte(first);
    tokio::time::sleep(Duration::from_millis(1_200)).await;

    harness.run_all().await;
    let done = harness.check(&id.to_string()).await;
    assert_eq!(done["state"], "complete", "{done}");
    assert_eq!(done["objects_checked"], 3);
    assert_eq!(done["problem_count"], 0, "resumed after the first: {done}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn who_may_ask_and_withdraw() {
    let harness = harness().await;
    let (artifact, _) = harness.import(&[("w.iso", 1_000)]).await;
    let viewer = support::sign_in(&harness.pool, Role::Viewer).await;
    let validate = format!("/api/v1/artifacts/{artifact}/validate");

    let (status, _) = harness.send(&viewer, Method::POST, &validate, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a viewer may not ask");
    let (status, first) = harness
        .send(&harness.operator, Method::POST, &validate, None)
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    let (status, again) = harness
        .send(&harness.operator, Method::POST, &validate, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["id"], first["id"]);

    // Anyone signed in may watch it; only an administrator may withdraw it.
    let id = first["id"].as_str().unwrap();
    let (status, seen) = harness
        .send(
            &viewer,
            Method::GET,
            &format!("/api/v1/integrity-checks/{id}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{seen}");
    let cancel = format!("/api/v1/integrity-checks/{id}/cancel");
    let (status, _) = harness
        .send(&harness.operator, Method::POST, &cancel, None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, canceled) = harness
        .send(&harness.admin, Method::POST, &cancel, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{canceled}");
    assert_eq!(canceled["state"], "canceled");
    assert!(!harness.runner.step().await.unwrap(), "nothing left to run");

    let (status, _) = harness
        .send(
            &harness.operator,
            Method::POST,
            "/api/v1/integrity-checks",
            Some(serde_json::json!({ "scope": "all" })),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a full scrub is an administrator's"
    );
    let (status, problem) = harness
        .send(
            &harness.admin,
            Method::POST,
            "/api/v1/integrity-checks",
            Some(serde_json::json!({ "scope": "some" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    let (status, _) = harness
        .send(
            &harness.operator,
            Method::POST,
            &format!("/api/v1/artifacts/{}/validate", uuid::Uuid::now_v7()),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
