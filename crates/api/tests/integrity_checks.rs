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
use tangible_api::deletion::DeletionRunner;
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
        let files: Vec<(&str, Vec<u8>)> = files
            .iter()
            .map(|(name, size)| {
                // Fresh bytes each time, so no two artifacts share an object.
                let seed = uuid::Uuid::now_v7();
                let bytes = (0..*size)
                    .map(|i| seed.as_bytes()[i % 16].wrapping_add(u8::try_from(i % 251).unwrap()))
                    .collect();
                (*name, bytes)
            })
            .collect();
        self.import_these(&files).await
    }

    /// An artifact of exactly these files, catalogued: its id and its
    /// objects' paths.
    async fn import_these(&self, files: &[(&str, Vec<u8>)]) -> (String, Vec<PathBuf>) {
        let import_id = ImportJobId::generate();
        let area = self.pipeline.open_area(import_id).await.unwrap();
        for (name, bytes) in files {
            area.write(&LogicalPath::parse(name).unwrap(), bytes)
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

    /// A full scrub that another process took, got as far as `after`, and
    /// left: the next step takes it over and reads only what sorts after.
    async fn full_scrub_left_after(&self, after: &str) -> String {
        let (status, check) = self
            .send(
                &self.admin,
                Method::POST,
                "/api/v1/integrity-checks",
                Some(serde_json::json!({ "scope": "all" })),
            )
            .await;
        assert!(status.is_success(), "{check}");
        let id = check["id"].as_str().unwrap().to_owned();
        let claimed = tangible_db::integrity::claim_next_check(&self.pool, "gone", 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id.to_string(), id);
        assert!(
            tangible_db::integrity::record_progress(
                &self.pool,
                &tangible_db::integrity::Progress {
                    id: claimed.id,
                    owner: "gone",
                    objects_checked: 0,
                    bytes_checked: 0,
                    resume_after: Some(after),
                    new_problems: &[],
                    new_problem_count: 0,
                    lease_seconds: 1,
                },
            )
            .await
            .unwrap()
        );
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        id
    }

    /// Delete an original with no grace period, and collect its objects.
    async fn delete_now(&self, artifact: &str) {
        let state = self.state.clone().with_deletion_grace_days(0);
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/v1/artifacts/{artifact}/deletion-requests"))
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({ "confirm_original": true }).to_string(),
            ))
            .unwrap();
        self.admin.apply(&mut request);
        let response = router(state).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let pass = DeletionRunner::new(self.database.clone(), self.manifests.clone())
            .pass()
            .await
            .unwrap();
        assert!(pass.deleted >= 1, "{pass:?}");
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

/// Bytes whose SHA-256 sorts after `after`, made by counting.
fn bytes_sorting_after(after: &str) -> (String, Vec<u8>) {
    use sha2::Digest as _;
    let seed = uuid::Uuid::now_v7();
    for counter in 0_u64.. {
        let mut bytes = seed.as_bytes().to_vec();
        bytes.extend_from_slice(&counter.to_le_bytes());
        bytes.resize(1_000, 0x5a);
        let digest = hex::encode(sha2::Sha256::digest(&bytes));
        if digest.as_str() > after {
            return (digest, bytes);
        }
    }
    unreachable!("some count sorts after any digest but the last")
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_object_collected_while_a_scrub_reads_is_not_reported() {
    // A full scrub lists objects a batch at a time. One collected after its
    // batch was listed, its artifact deleted (ADR 0020), is gone on purpose:
    // the scrub must not report it missing.
    //
    // The scrub is made to read only this test's two objects, so what it
    // finds is exact: both sort after every object the shared database
    // already holds, and the scrub resumes after the last of those. The
    // first is a named pipe, so reading it waits until the test has
    // collected the second.
    let harness = harness().await;
    let highest: String = sqlx::query_scalar("SELECT COALESCE(max(sha256), '') FROM cas_objects")
        .fetch_one(&harness.pool)
        .await
        .unwrap();
    let (one, one_bytes) = bytes_sorting_after(&highest);
    let (two, two_bytes) = bytes_sorting_after(&highest);
    let ((first, first_bytes), (second, second_bytes)) = if one < two {
        ((one, one_bytes), (two, two_bytes))
    } else {
        ((two, two_bytes), (one, one_bytes))
    };
    let (kept, kept_paths) = harness
        .import_these(&[("kept.bin", first_bytes.clone())])
        .await;
    let (deleted, deleted_paths) = harness.import_these(&[("deleted.bin", second_bytes)]).await;
    assert!(kept_paths[0].ends_with(&first) && deleted_paths[0].ends_with(&second));

    // Reading the kept object waits for the test.
    writable(&kept_paths[0]);
    std::fs::remove_file(&kept_paths[0]).unwrap();
    let made = std::process::Command::new("mkfifo")
        .arg(&kept_paths[0])
        .status()
        .unwrap();
    assert!(made.success());

    let id = harness.full_scrub_left_after(&highest).await;
    let runner = harness.runner.clone();
    let scrub = tokio::spawn(async move { runner.step().await });

    // Opening the pipe to write returns once the scrub has opened it to
    // read: both objects are listed, and the first is being read.
    let pipe = kept_paths[0].clone();
    let mut writer = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::task::spawn_blocking(move || std::fs::OpenOptions::new().write(true).open(pipe)),
    )
    .await
    .expect("the scrub reads the kept object")
    .unwrap()
    .unwrap();

    // Meanwhile the other artifact is deleted and its object collected.
    harness.delete_now(&deleted).await;
    assert!(!deleted_paths[0].exists(), "the object is collected");

    // The kept object reads back whole.
    std::io::Write::write_all(&mut writer, &first_bytes).unwrap();
    drop(writer);
    let ran = tokio::time::timeout(Duration::from_secs(30), scrub)
        .await
        .expect("the scrub ends")
        .unwrap()
        .unwrap();
    assert!(ran, "the scrub was claimed");

    let done = harness.check(&id).await;
    assert_eq!(done["state"], "complete", "{done}");
    assert_eq!(done["objects_checked"], 2, "{done}");
    assert_eq!(
        done["problem_count"], 0,
        "the collected object is not a problem: {done}"
    );
    assert_ne!(harness.artifact(&kept).await["integrity_state"], "damaged");
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

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_scheduled_scrub_is_queued_only_when_one_is_due() {
    use tangible_db::integrity::{integrity_summary, queue_scheduled_scrub};
    let harness = harness().await;

    // A full scrub ends now, withdrawn: one ended is one ended.
    let (_, asked) = harness
        .send(
            &harness.admin,
            Method::POST,
            "/api/v1/integrity-checks",
            Some(serde_json::json!({ "scope": "all" })),
        )
        .await;
    let cancel = format!(
        "/api/v1/integrity-checks/{}/cancel",
        asked["id"].as_str().unwrap()
    );
    let (status, _) = harness
        .send(&harness.admin, Method::POST, &cancel, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        queue_scheduled_scrub(&harness.pool, 3_600).await.unwrap(),
        None,
        "not due within the hour after one ended"
    );

    tokio::time::sleep(Duration::from_millis(1_200)).await;
    let queued = queue_scheduled_scrub(&harness.pool, 1)
        .await
        .unwrap()
        .expect("due a second after");
    assert_eq!(queued.created_by, "scheduler");
    assert_eq!(queued.scope.as_str(), "all");
    assert_eq!(queued.state.as_str(), "queued");
    assert_eq!(
        queue_scheduled_scrub(&harness.pool, 1).await.unwrap(),
        None,
        "one is queued already"
    );
    let summary = integrity_summary(&harness.pool).await.unwrap();
    assert_eq!(summary.active_scrub, Some((0, None)));
    tangible_db::integrity::cancel_check(&harness.pool, queued.id)
        .await
        .unwrap();

    // The runner queues one itself when one is due, and reads it.
    let runner = IntegrityRunner::new(
        harness.database.clone(),
        harness.objects.clone(),
        harness.manifests.clone(),
        Integrity::new(),
        IntegrityRunnerSettings {
            owner: format!("test-{}", uuid::Uuid::now_v7()),
            scrub_every: Some(Duration::from_secs(3_600)),
            ..IntegrityRunnerSettings::default()
        },
    );
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert!(
        !runner.step().await.unwrap(),
        "a scrub withdrawn a moment ago means none is due within the hour"
    );
    let runner = IntegrityRunner::new(
        harness.database.clone(),
        harness.objects.clone(),
        harness.manifests.clone(),
        Integrity::new(),
        IntegrityRunnerSettings {
            owner: format!("test-{}", uuid::Uuid::now_v7()),
            scrub_every: Some(Duration::from_secs(1)),
            ..IntegrityRunnerSettings::default()
        },
    );
    assert!(runner.step().await.unwrap());
    let latest = tangible_db::integrity::list_checks(&harness.pool, None, 1)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(latest.created_by, "scheduler");
    assert_eq!(latest.state.as_str(), "complete");
    let summary = integrity_summary(&harness.pool).await.unwrap();
    assert_eq!(summary.active_scrub, None);
    let last = summary.last_scrub.expect("one ended");
    assert_eq!(last.state.as_str(), "complete");
    assert_eq!(last.objects_checked, latest.objects_checked);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_check_given_up_is_taken_over_at_once() {
    use tangible_db::integrity::{Progress, claim_next_check, record_progress, release_checks};
    let harness = harness().await;
    let (artifact, _) = harness.import(&[("given-up.bin", 1_000)]).await;
    let check = harness.validate(&artifact).await;

    let held = claim_next_check(&harness.pool, "holder", 600)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(held.id.to_string(), check["id"].as_str().unwrap());
    assert!(
        claim_next_check(&harness.pool, "another", 600)
            .await
            .unwrap()
            .is_none(),
        "held for ten minutes"
    );
    assert_eq!(release_checks(&harness.pool, "holder").await.unwrap(), 1);
    assert!(
        !record_progress(
            &harness.pool,
            &Progress {
                id: held.id,
                owner: "holder",
                objects_checked: 0,
                bytes_checked: 0,
                resume_after: None,
                new_problems: &[],
                new_problem_count: 0,
                lease_seconds: 600,
            },
        )
        .await
        .unwrap(),
        "the holder finds it is no longer its own, and stops"
    );

    harness.run_all().await;
    let done = harness.check(check["id"].as_str().unwrap()).await;
    assert_eq!(done["state"], "complete", "{done}");
    assert_eq!(
        harness.artifact(&artifact).await["integrity_state"],
        "intact"
    );
}
