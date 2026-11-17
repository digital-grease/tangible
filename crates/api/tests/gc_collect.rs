// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Collecting orphaned objects (ADR 0024, docs/plans/gc-sweep.md G2),
//! against PostgreSQL and real files: a plan kept, collected through the
//! routes and the deletion runner, removing what it listed and nothing
//! else, and keeping a candidate that changed since.
//!
//! The shared test database records other tests' objects too, whose files
//! are not in this test's library, so what is asserted is said of this
//! test's own plans and objects.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::io::Cursor;
use std::path::Path;
use std::time::{Duration, SystemTime};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use tangible_api::deletion::DeletionRunner;
use tangible_api::import::PromotedComponent;
use tangible_api::{ApiState, ImportCheckpoint, ImportPipeline, ImportRequest, catalog, router};
use tangible_domain::{ImportJobId, ImportState, LogicalPath, Role, Sha256Digest};
use tangible_storage::{FilesystemStore, IngestLimits, ManifestStore, StagingManager};
use tempfile::TempDir;
use tower::ServiceExt as _;

mod support;

/// One test at a time: the runner collects every plan queued, any test's.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const LIMITS: IngestLimits = IngestLimits {
    max_bytes: None,
    fsync: false,
};

const DAY: Duration = Duration::from_hours(24);

struct Harness {
    _dir: TempDir,
    pool: PgPool,
    database: tangible_db::Database,
    pipeline: ImportPipeline,
    objects: FilesystemStore,
    manifests: ManifestStore,
    state: ApiState,
    admin: support::Session,
    runner: DeletionRunner,
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
    let pool = database.pool().clone();
    let admin = support::administrator(&pool).await;
    Harness {
        pipeline: ImportPipeline::new(staging, objects.clone(), manifests.clone()),
        state: ApiState::with_manifests(database.clone(), manifests.clone()),
        runner: DeletionRunner::new(database.clone(), manifests.clone()),
        pool,
        database,
        objects,
        manifests,
        admin,
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

/// Date a file's last write, which its owner may do on a read-only file, as
/// objects are (ADR 0011).
fn written_at(path: &Path, when: SystemTime) {
    std::fs::File::options()
        .read(true)
        .open(path)
        .unwrap()
        .set_modified(when)
        .unwrap();
}

fn upload(import_id: ImportJobId, name: &str) -> ImportRequest {
    ImportRequest {
        import_id,
        source_kind: "upload".to_owned(),
        source_filename: Some(name.to_owned()),
        source_reference: None,
        physical_dump: None,
        lineage: None,
        limits: LIMITS,
    }
}

impl Harness {
    async fn send(
        &self,
        session: &support::Session,
        method: &str,
        uri: &str,
    ) -> (StatusCode, Value) {
        let body = (method == "POST" && uri.ends_with("/gc-plans")).then(|| json!({}));
        self.send_with(session, method, uri, body).await
    }

    async fn send_with(
        &self,
        session: &support::Session,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .body(match &body {
                Some(body) => Body::from(body.to_string()),
                None => Body::empty(),
            })
            .unwrap();
        if body.is_some() {
            request
                .headers_mut()
                .insert("content-type", "application/json".parse().unwrap());
        }
        session.apply(&mut request);
        let response = router(self.state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 16 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn admin(&self, method: &str, uri: &str) -> (StatusCode, Value) {
        self.send(&self.admin, method, uri).await
    }

    /// Make a plan with a week's grace through the route; its body.
    async fn plan(&self) -> Value {
        let (status, body) = self
            .send_with(
                &self.admin,
                "POST",
                "/api/v1/storage/gc-plans",
                Some(json!({ "grace_days": 7 })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body
    }

    /// Ask for a plan to be collected, and have the runner collect it.
    async fn collect(&self, plan_id: &str) -> Value {
        let (status, body) = self
            .admin(
                "POST",
                &format!("/api/v1/storage/gc-plans/{plan_id}/collect"),
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert_eq!(body["state"], "queued");
        let pass = self.runner.collect_queued_plans().await.unwrap();
        assert!(pass.plans >= 1, "{pass:?}");
        let (status, body) = self
            .admin("GET", &format!("/api/v1/storage/gc-plans/{plan_id}"))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    /// An object nothing catalogued, as an import that never finished
    /// leaves one, last written a month ago.
    async fn old_orphan(&self) -> Sha256Digest {
        let digest = self
            .objects
            .put_stream(Cursor::new(fresh(1000)), LIMITS)
            .await
            .unwrap()
            .digest;
        written_at(
            &self.objects.object_path(&digest),
            SystemTime::now() - DAY * 30,
        );
        digest
    }

    /// An artifact of one fresh file, catalogued, its object a month old.
    async fn old_artifact(&self) -> Sha256Digest {
        let import_id = ImportJobId::generate();
        let area = self.pipeline.open_area(import_id).await.unwrap();
        area.write(&LogicalPath::parse("disc.iso").unwrap(), &fresh(4096))
            .await
            .unwrap();
        let outcome = self
            .pipeline
            .run(
                &upload(import_id, "disc.iso"),
                &mut ImportCheckpoint::default(),
            )
            .await
            .unwrap();
        let manifest = self.manifests.read(outcome.artifact_id).await.unwrap();
        catalog::register_manifest(&self.database, &manifest)
            .await
            .unwrap();
        let digest = manifest.components[0].content.sha256;
        written_at(
            &self.objects.object_path(&digest),
            SystemTime::now() - DAY * 30,
        );
        digest
    }

    /// An import in `state`, named `name`, whose checkpoint lists `digest`;
    /// its id.
    async fn import_job(&self, state: &str, name: &str, digest: Sha256Digest) -> uuid::Uuid {
        let checkpoint = ImportCheckpoint {
            promoted: [(
                "disc.iso".to_owned(),
                PromotedComponent {
                    digest,
                    length_bytes: 1000,
                },
            )]
            .into(),
            ..ImportCheckpoint::default()
        };
        let id = *ImportJobId::generate().as_uuid();
        sqlx::query(
            "INSERT INTO import_jobs (id, source_kind, source_descriptor_json, state,
                 checkpoint_json, error_code, created_by)
             VALUES ($1, 'upload', $2, $3, $4, $5, 'gc-test')",
        )
        .bind(id)
        .bind(json!({ "filename": name }))
        .bind(state)
        .bind(serde_json::to_value(&checkpoint).unwrap())
        .bind((state == "failed_terminal").then_some("IMPORT_DESCRIPTOR_INVALID"))
        .execute(&self.pool)
        .await
        .unwrap();
        id
    }

    fn present(&self, digest: &Sha256Digest) -> bool {
        self.objects.object_path(digest).exists()
    }

    /// What became of each of this plan's candidates, by digest.
    fn outcomes(body: &Value) -> std::collections::BTreeMap<String, String> {
        body["objects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|object| {
                (
                    object["sha256"].as_str().unwrap().to_owned(),
                    object["outcome"].as_str().unwrap_or("none").to_owned(),
                )
            })
            .collect()
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_kept_plan_is_collected_and_removes_what_it_listed_and_nothing_else() {
    let _serial = SERIAL.lock().await;
    let h = harness().await;
    let catalogued = h.old_artifact().await;
    let orphan = h.old_orphan().await;
    let import_id = h.import_job("failed_terminal", "Hackers.iso", orphan).await;
    let young = h
        .objects
        .put_stream(Cursor::new(fresh(500)), LIMITS)
        .await
        .unwrap()
        .digest;
    let incoming = h.objects.root().join("objects/incoming");
    std::fs::write(incoming.join("old.part"), b"half").unwrap();
    written_at(&incoming.join("old.part"), SystemTime::now() - DAY * 30);
    std::fs::write(incoming.join("new.part"), b"half").unwrap();
    std::fs::write(h.objects.root().join("objects/stray"), b"not an object").unwrap();

    let planned = h.plan().await;
    let plan_id = planned["plan"]["id"].as_str().unwrap().to_owned();
    assert_eq!(planned["plan"]["state"], "planned");
    assert_eq!(planned["plan"]["certain"], true);
    assert_eq!(
        planned["plan"]["planned_by"],
        h.admin.user.username.as_str()
    );
    assert_eq!(planned["plan"]["candidates"]["count"], 1, "{planned}");
    assert_eq!(planned["plan"]["candidates"]["bytes"], 1000);
    assert_eq!(planned["plan"]["stale_partial_files"], 1);
    let candidate = &planned["objects"][0];
    assert_eq!(candidate["sha256"], orphan.to_hex());
    assert_eq!(candidate["recorded"], false);
    assert_eq!(candidate["outcome"], Value::Null);
    assert_eq!(candidate["import"]["import_id"], import_id.to_string());
    assert_eq!(candidate["import"]["state"], "failed_terminal");
    assert_eq!(
        candidate["import"]["error_code"],
        "IMPORT_DESCRIPTOR_INVALID"
    );
    assert_eq!(candidate["import"]["name"], "Hackers.iso");
    // Planning removed nothing.
    assert!(h.present(&orphan));

    let collected = h.collect(&plan_id).await;
    assert_eq!(collected["plan"]["state"], "collected", "{collected}");
    let done = &collected["plan"]["collected"];
    assert_eq!(done["removed"], json!({ "count": 1, "bytes": 1000 }));
    assert_eq!(done["kept"], json!({ "count": 0, "bytes": 0 }));
    assert_eq!(done["partial_files_removed"], 1);
    assert_eq!(
        collected["plan"]["collect_requested_by"],
        h.admin.user.username.as_str()
    );
    assert_eq!(Harness::outcomes(&collected)[&orphan.to_hex()], "removed");

    assert!(!h.present(&orphan));
    for kept in [catalogued, young] {
        assert!(h.present(&kept), "{kept} is not an orphan old enough to go");
    }
    assert!(!incoming.join("old.part").exists());
    assert!(incoming.join("new.part").exists());
    assert!(h.objects.root().join("objects/stray").exists());

    let audit: (String, String, Value) = sqlx::query_as(
        "SELECT actor_type, actor_id, metadata FROM audit_events
         WHERE action = 'storage.orphans_removed' AND target_id = $1",
    )
    .bind(&plan_id)
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(audit.0, "user");
    assert_eq!(audit.1, h.admin.user.username);
    assert_eq!(audit.2["removed"], json!({ "count": 1, "bytes": 1000 }));

    // Once only.
    let (status, body) = h
        .admin(
            "POST",
            &format!("/api/v1/storage/gc-plans/{plan_id}/collect"),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "GC_PLAN_COLLECTED");

    let (status, body) = h.admin("GET", "/api/v1/storage/gc-plans?limit=100").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|plan| plan["id"] == plan_id.as_str() && plan["state"] == "collected")
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_candidate_that_changed_since_the_plan_is_kept() {
    let _serial = SERIAL.lock().await;
    let h = harness().await;
    let referenced = h.old_orphan().await;
    let rewritten = h.old_orphan().await;
    let vanished = h.old_orphan().await;
    let removable = h.old_orphan().await;
    // One the catalogue records with nothing counting it, and one a restore
    // brought back whose row says it was removed: both go, their rows marked.
    let recorded = h.old_orphan().await;
    let restored = h.old_orphan().await;
    for (digest, removed) in [(recorded, false), (restored, true)] {
        sqlx::query(
            "INSERT INTO cas_objects (sha256, size_bytes, removed_at)
             VALUES ($1, 1000, CASE WHEN $2 THEN now() END)",
        )
        .bind(digest.to_hex())
        .bind(removed)
        .execute(&h.pool)
        .await
        .unwrap();
    }

    let planned = h.plan().await;
    let plan_id = planned["plan"]["id"].as_str().unwrap().to_owned();
    assert_eq!(planned["plan"]["candidates"]["count"], 6, "{planned}");

    // After the plan: an import under way comes to list one, another's file
    // is written again, a third goes by other means.
    h.import_job("quarantined", "held.iso", referenced).await;
    written_at(
        &h.objects.object_path(&rewritten),
        SystemTime::now() - DAY * 20,
    );
    std::fs::remove_file(h.objects.object_path(&vanished)).unwrap();

    let collected = h.collect(&plan_id).await;
    let outcomes = Harness::outcomes(&collected);
    assert_eq!(outcomes[&referenced.to_hex()], "kept_live");
    assert_eq!(outcomes[&rewritten.to_hex()], "kept_changed");
    assert_eq!(outcomes[&vanished.to_hex()], "kept_gone");
    for gone in [removable, recorded, restored] {
        assert_eq!(outcomes[&gone.to_hex()], "removed");
        assert!(!h.present(&gone));
    }
    assert!(h.present(&referenced));
    assert!(h.present(&rewritten));
    let done = &collected["plan"]["collected"];
    assert_eq!(done["removed"]["count"], 3);
    assert_eq!(done["kept"], json!({ "count": 3, "bytes": 3000 }));
    let removed: Vec<bool> = sqlx::query_scalar(
        "SELECT removed_at IS NOT NULL FROM cas_objects WHERE sha256 = ANY($1) ORDER BY sha256",
    )
    .bind(vec![recorded.to_hex(), restored.to_hex()])
    .fetch_all(&h.pool)
    .await
    .unwrap();
    assert_eq!(removed, vec![true, true]);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_uncertain_expired_or_unknown_plan_is_refused_and_only_administrators_plan() {
    let _serial = SERIAL.lock().await;
    let h = harness().await;
    let orphan = h.old_orphan().await;

    // A manifest that cannot be read: something unread may name an orphan.
    let unreadable = h
        .manifests
        .manifest_path(tangible_domain::ArtifactId::generate());
    std::fs::write(&unreadable, b"{ not a manifest").unwrap();
    let uncertain = h.plan().await;
    assert_eq!(uncertain["plan"]["certain"], false);
    assert_eq!(uncertain["plan"]["state"], "uncertain");
    let id = uncertain["plan"]["id"].as_str().unwrap();
    let (status, body) = h
        .admin("POST", &format!("/api/v1/storage/gc-plans/{id}/collect"))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "GC_PLAN_UNCERTAIN");
    std::fs::remove_file(&unreadable).unwrap();

    // More than an hour old.
    let old = h.plan().await;
    let id = old["plan"]["id"].as_str().unwrap();
    sqlx::query("UPDATE gc_plans SET created_at = now() - interval '2 hours' WHERE id = $1")
        .bind(uuid::Uuid::parse_str(id).unwrap())
        .execute(&h.pool)
        .await
        .unwrap();
    let (_, body) = h
        .admin("GET", &format!("/api/v1/storage/gc-plans/{id}"))
        .await;
    assert_eq!(body["plan"]["state"], "expired");
    let (status, body) = h
        .admin("POST", &format!("/api/v1/storage/gc-plans/{id}/collect"))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "GC_PLAN_EXPIRED");

    // A grace period under a day, or past ten years.
    for grace_days in [0, 3651] {
        let (status, body) = h
            .send_with(
                &h.admin,
                "POST",
                "/api/v1/storage/gc-plans",
                Some(json!({ "grace_days": grace_days })),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{grace_days}: {body}");
        assert_eq!(body["code"], "INVALID_PARAMETER");
    }

    let unknown = uuid::Uuid::now_v7();
    for (method, uri) in [
        ("GET", format!("/api/v1/storage/gc-plans/{unknown}")),
        (
            "POST",
            format!("/api/v1/storage/gc-plans/{unknown}/collect"),
        ),
    ] {
        let (status, body) = h.admin(method, &uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}: {body}");
    }

    let plan_id = old["plan"]["id"].as_str().unwrap();
    for role in [Role::Viewer, Role::Operator] {
        let session = support::sign_in(&h.pool, role).await;
        for (method, uri) in [
            ("POST", "/api/v1/storage/gc-plans".to_owned()),
            ("GET", "/api/v1/storage/gc-plans".to_owned()),
            ("GET", format!("/api/v1/storage/gc-plans/{plan_id}")),
            (
                "POST",
                format!("/api/v1/storage/gc-plans/{plan_id}/collect"),
            ),
        ] {
            let (status, body) = h.send(&session, method, &uri).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{role:?} {method} {uri}: {body}"
            );
        }
    }
    // Nothing was collected.
    assert!(h.present(&orphan));
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn collection_waits_for_an_ingest_under_way() {
    let _serial = SERIAL.lock().await;
    let h = harness().await;
    let orphan = h.old_orphan().await;
    let planned = h.plan().await;
    let plan_id = planned["plan"]["id"].as_str().unwrap().to_owned();
    let (status, body) = h
        .admin(
            "POST",
            &format!("/api/v1/storage/gc-plans/{plan_id}/collect"),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");

    let ingesting = h.objects.ingesting().await;
    let pass = h.runner.collect_queued_plans().await.unwrap();
    assert!(pass.deferred, "{pass:?}");
    let (_, body) = h
        .admin("GET", &format!("/api/v1/storage/gc-plans/{plan_id}"))
        .await;
    assert_eq!(body["plan"]["state"], "queued");
    assert!(h.present(&orphan));

    drop(ingesting);
    let pass = h.runner.collect_queued_plans().await.unwrap();
    assert!(pass.plans >= 1 && !pass.deferred, "{pass:?}");
    assert!(!h.present(&orphan));
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_import_retried_after_its_orphan_was_collected_completes() {
    // An import that stored its bytes and then failed for good leaves them
    // an orphan, and its staged copy. Collecting the orphan does not strand
    // a retry: the retry stores them again from the staged copy (G1).
    let _serial = SERIAL.lock().await;
    let h = harness().await;
    let import_id = ImportJobId::generate();
    let path = LogicalPath::parse("disc.iso").unwrap();
    let area = h.pipeline.open_area(import_id).await.unwrap();
    area.write(&path, &fresh(1000)).await.unwrap();
    let stored = {
        let _ingesting = h.objects.ingesting().await;
        area.promote(&path, &h.objects, LIMITS).await.unwrap()
    };
    let mut checkpoint = ImportCheckpoint {
        state: ImportState::Hashing,
        ..ImportCheckpoint::default()
    };
    checkpoint.promoted.insert(
        "disc.iso".to_owned(),
        PromotedComponent {
            digest: stored.digest,
            length_bytes: stored.size_bytes,
        },
    );
    sqlx::query(
        "INSERT INTO import_jobs (id, source_kind, source_descriptor_json, state,
             checkpoint_json, error_code, created_by)
         VALUES ($1, 'upload', '{\"filename\": \"disc.iso\"}', 'failed_terminal', $2,
                 'IMPORT_DESCRIPTOR_INVALID', 'gc-test')",
    )
    .bind(*import_id.as_uuid())
    .bind(serde_json::to_value(&checkpoint).unwrap())
    .execute(&h.pool)
    .await
    .unwrap();
    written_at(
        &h.objects.object_path(&stored.digest),
        SystemTime::now() - DAY * 30,
    );

    let planned = h.plan().await;
    let plan_id = planned["plan"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        planned["objects"][0]["import"]["import_id"],
        import_id.to_string()
    );
    let collected = h.collect(&plan_id).await;
    assert_eq!(
        Harness::outcomes(&collected)[&stored.digest.to_hex()],
        "removed"
    );
    assert!(!h.present(&stored.digest));

    let retried = {
        let _ingesting = h.objects.ingesting().await;
        let outcome = h
            .pipeline
            .run(&upload(import_id, "disc.iso"), &mut checkpoint)
            .await
            .unwrap();
        let manifest = h.manifests.read(outcome.artifact_id).await.unwrap();
        catalog::register_manifest(&h.database, &manifest)
            .await
            .unwrap();
        manifest
    };
    assert_eq!(retried.components[0].content.sha256, stored.digest);
    h.objects
        .verify(&stored.digest)
        .await
        .expect("stored again");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_scheduled_plan_is_collected_as_the_schedule_and_an_uncertain_one_is_not() {
    let _serial = SERIAL.lock().await;
    let h = harness().await;
    let orphan = h.old_orphan().await;

    // With no interval the runner never plans.
    assert_eq!(h.runner.run_schedule().await.unwrap(), None);

    let (id, queued) = tangible_api::orphans::schedule_plan(&h.database, &h.manifests)
        .await
        .unwrap();
    assert!(queued);
    let pass = h.runner.collect_queued_plans().await.unwrap();
    assert!(pass.plans >= 1, "{pass:?}");
    let (status, body) = h
        .admin("GET", &format!("/api/v1/storage/gc-plans/{id}"))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["plan"]["state"], "collected", "{body}");
    assert_eq!(body["plan"]["planned_by"], Value::Null);
    assert_eq!(body["plan"]["collect_requested_by"], Value::Null);
    assert_eq!(body["plan"]["grace_seconds"], 7 * 86_400);
    assert_eq!(body["plan"]["collected"]["removed"]["count"], 1);
    assert!(!h.present(&orphan));
    let actor: (String, String) = sqlx::query_as(
        "SELECT actor_type, actor_id FROM audit_events
         WHERE action = 'storage.orphans_removed' AND target_id = $1",
    )
    .bind(id.to_string())
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(actor, ("system".to_owned(), "schedule".to_owned()));

    // A plan that could not read every mark is kept and not queued.
    let other = h.old_orphan().await;
    let unreadable = h
        .manifests
        .manifest_path(tangible_domain::ArtifactId::generate());
    std::fs::write(&unreadable, b"{ not a manifest").unwrap();
    let (id, queued) = tangible_api::orphans::schedule_plan(&h.database, &h.manifests)
        .await
        .unwrap();
    std::fs::remove_file(&unreadable).unwrap();
    assert!(!queued);
    let (_, body) = h
        .admin("GET", &format!("/api/v1/storage/gc-plans/{id}"))
        .await;
    assert_eq!(body["plan"]["state"], "uncertain");
    assert_eq!(body["plan"]["collect_requested_at"], Value::Null);
    h.runner.collect_queued_plans().await.unwrap();
    assert!(h.present(&other));
}
