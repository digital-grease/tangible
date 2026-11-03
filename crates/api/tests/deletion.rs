// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Deleting artifacts, as tombstones, against a real store and PostgreSQL
//! (ADR 0020, docs/plans/deletion.md).
//!
//! Real imports, the routes, and the deletion runner's passes. What is
//! asserted is what an operator would see: the artifact pending and then a
//! tombstone, its record kept, its bytes gone, and an object another
//! artifact still has kept.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use tangible_api::deletion::DeletionRunner;
use tangible_api::{ApiState, ImportCheckpoint, ImportPipeline, ImportRequest, catalog, router};
use tangible_domain::{ArtifactId, ImportJobId, LogicalPath, Role, Sha256Digest};
use tangible_storage::{FilesystemStore, IngestLimits, ManifestStore, StagingManager};
use tempfile::TempDir;
use tower::ServiceExt as _;

mod support;

/// One test at a time: a deletion pass makes every due artifact a
/// tombstone, any test's included.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Harness {
    _dir: TempDir,
    pool: PgPool,
    database: tangible_db::Database,
    objects: FilesystemStore,
    pipeline: ImportPipeline,
    state: ApiState,
    admin: support::Session,
    runner: DeletionRunner,
}

async fn harness() -> Harness {
    let dir = TempDir::new().unwrap();
    let objects = FilesystemStore::open(dir.path().join("library"))
        .await
        .unwrap();
    let manifests = ManifestStore::open(objects.clone()).await.unwrap();
    let staging = StagingManager::open(dir.path().join("staging"))
        .await
        .unwrap();
    let pipeline = ImportPipeline::new(staging, objects.clone(), manifests.clone());
    let database = support::database().await;
    let pool = database.pool().clone();
    let admin = support::administrator(&pool).await;
    // No grace period: the next pass deletes.
    let state =
        ApiState::with_manifests(database.clone(), manifests.clone()).with_deletion_grace_days(0);
    let runner = DeletionRunner::new(database.clone(), manifests);
    Harness {
        _dir: dir,
        pool,
        database,
        objects,
        pipeline,
        state,
        admin,
        runner,
    }
}

impl Harness {
    async fn send(
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
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 22)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    async fn admin(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.send(&self.admin, method, uri, body).await
    }

    /// Import `bytes` as `name`, registered, the way the import runner does,
    /// holding the store's collection lock while it does.
    async fn import(&self, name: &str, bytes: &[u8]) -> ArtifactId {
        let _ingesting = self.objects.ingesting().await;
        let import_id = ImportJobId::generate();
        let area = self.pipeline.open_area(import_id).await.unwrap();
        area.write(&LogicalPath::parse(name).unwrap(), bytes)
            .await
            .unwrap();
        let mut checkpoint = ImportCheckpoint::default();
        let outcome = self
            .pipeline
            .run(
                &ImportRequest {
                    import_id,
                    source_kind: "upload".to_owned(),
                    source_filename: Some(name.to_owned()),
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
        outcome.artifact_id
    }

    async fn digest_of(&self, artifact: ArtifactId) -> Sha256Digest {
        let manifest = self.pipeline.manifests().read(artifact).await.unwrap();
        manifest.components[0].content.sha256
    }

    async fn reference_count(&self, digest: &Sha256Digest) -> (i32, bool) {
        sqlx::query_as(
            "SELECT reference_count, removed_at IS NOT NULL FROM cas_objects WHERE sha256 = $1",
        )
        .bind(digest.to_hex())
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn actions(&self, artifact: ArtifactId) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT action FROM audit_events WHERE target_id = $1 ORDER BY created_at, id",
        )
        .bind(artifact.to_string())
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }
}

/// Bytes no other test imports.
fn unique(tag: &str) -> Vec<u8> {
    let mut bytes = format!("{tag}-{}\n", uuid::Uuid::now_v7()).into_bytes();
    bytes.resize(64 * 1024, 0x5a);
    bytes
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_original_becomes_a_tombstone_its_record_kept_and_its_bytes_gone() {
    let _serial = SERIAL.lock().await;
    let harness = harness().await;
    let artifact = harness.import("disc.bin", &unique("original")).await;
    let digest = harness.digest_of(artifact).await;
    let path = harness.objects.object_path(&digest);
    let uri = format!("/api/v1/artifacts/{artifact}/deletion-requests");

    // An original needs saying so.
    let (status, body) = harness.admin("POST", &uri, Some(json!({}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("confirm_original")
    );

    let (status, pending) = harness
        .admin("POST", &uri, Some(json!({ "confirm_original": true })))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{pending}");
    assert_eq!(pending["state"], "pending");
    assert_eq!(
        pending["requested_by"],
        harness.admin.user.username.as_str()
    );
    assert!(pending["delete_after"].is_string());
    // Asking again changes nothing.
    let (status, again) = harness
        .admin("POST", &uri, Some(json!({ "confirm_original": true })))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(again["requested_at"], pending["requested_at"]);

    // Pending: shown, still readable, nothing new made of it.
    let (_, detail) = harness
        .admin("GET", &format!("/api/v1/artifacts/{artifact}"), None)
        .await;
    assert_eq!(detail["deletion"]["state"], "pending", "{detail}");
    let (status, refused) = harness
        .admin(
            "POST",
            &format!("/api/v1/artifacts/{artifact}/derivatives"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["code"], "ARTIFACT_DELETED");
    let component = detail["components"][0]["id"].as_str().unwrap().to_owned();
    let content = format!("/api/v1/artifacts/{artifact}/components/{component}/content");
    let (status, _) = harness.admin("GET", &content, None).await;
    assert_eq!(status, StatusCode::OK, "a copy can still be taken");

    // The pass makes the tombstone and removes the bytes.
    let pass = harness.runner.pass().await.unwrap();
    assert!(pass.deleted >= 1, "{pass:?}");
    assert!(!path.exists(), "the object is removed");
    assert_eq!(harness.reference_count(&digest).await, (0, true));

    let (status, tombstone) = harness
        .admin("GET", &format!("/api/v1/artifacts/{artifact}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "the record is kept: {tombstone}");
    assert_eq!(tombstone["deletion"]["state"], "deleted");
    assert!(tombstone["deletion"]["deleted_at"].is_string());
    assert!(
        tombstone["components"][0]["sha256"].is_string(),
        "hashes kept"
    );
    let (status, _) = harness
        .admin(
            "GET",
            &format!("/api/v1/artifacts/{artifact}/manifest"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "the manifest is kept");
    let (status, gone) = harness.admin("GET", &content, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{gone}");
    assert_eq!(gone["code"], "ARTIFACT_DELETED");
    let (status, _) = harness.admin("DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "a tombstone cannot be kept");

    // Left out of the library, and listed as a tombstone when asked.
    let (_, listed) = harness
        .admin("GET", "/api/v1/artifacts?limit=500", None)
        .await;
    assert!(
        listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["id"] != artifact.to_string()),
        "not in the library"
    );
    let (_, tombstones) = harness
        .admin("GET", "/api/v1/artifacts?deleted=true&limit=500", None)
        .await;
    assert!(
        tombstones["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == artifact.to_string()),
        "{tombstones}"
    );

    assert_eq!(
        harness.actions(artifact).await,
        ["artifact.deletion_requested", "artifact.deleted"]
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_object_another_artifact_has_is_kept() {
    let _serial = SERIAL.lock().await;
    let harness = harness().await;
    let bytes = unique("shared");
    let first = harness.import("first.bin", &bytes).await;
    let second = harness.import("second.bin", &bytes).await;
    let digest = harness.digest_of(first).await;
    assert_eq!(harness.digest_of(second).await, digest, "deduplicated");
    assert_eq!(harness.reference_count(&digest).await, (2, false));

    harness
        .admin(
            "POST",
            &format!("/api/v1/artifacts/{first}/deletion-requests"),
            Some(json!({ "confirm_original": true })),
        )
        .await;
    harness.runner.pass().await.unwrap();
    assert_eq!(harness.reference_count(&digest).await, (1, false));
    assert!(harness.objects.object_path(&digest).exists());

    // The second goes too: now nothing has it.
    harness
        .admin(
            "POST",
            &format!("/api/v1/artifacts/{second}/deletion-requests"),
            Some(json!({ "confirm_original": true })),
        )
        .await;
    harness.runner.pass().await.unwrap();
    assert_eq!(harness.reference_count(&digest).await, (0, true));
    assert!(!harness.objects.object_path(&digest).exists());

    // The same bytes imported again are stored again, the row reused.
    let third = harness.import("third.bin", &bytes).await;
    assert_eq!(harness.digest_of(third).await, digest);
    assert!(harness.objects.object_path(&digest).exists());
    assert_eq!(harness.reference_count(&digest).await, (1, false));
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_request_is_kept_and_held_artifacts_are_refused() {
    let _serial = SERIAL.lock().await;
    let harness = harness().await;
    let artifact = harness.import("kept.bin", &unique("kept")).await;
    let uri = format!("/api/v1/artifacts/{artifact}/deletion-requests");

    // A derivative needs no confirmation.
    sqlx::query("UPDATE artifacts SET origin = 'derived' WHERE id = $1")
        .bind(artifact.as_uuid())
        .execute(&harness.pool)
        .await
        .unwrap();
    let (status, body) = harness.admin("POST", &uri, Some(json!({}))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let (status, _) = harness.admin("DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, detail) = harness
        .admin("GET", &format!("/api/v1/artifacts/{artifact}"), None)
        .await;
    assert!(detail["deletion"].is_null(), "{detail}");
    let (status, _) = harness.admin("DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "not pending");
    harness.runner.pass().await.unwrap();
    assert!(
        harness
            .objects
            .object_path(&harness.digest_of(artifact).await)
            .exists(),
        "a kept artifact keeps its bytes"
    );

    // A retention lock refuses it.
    sqlx::query("UPDATE artifacts SET retention_lock = TRUE WHERE id = $1")
        .bind(artifact.as_uuid())
        .execute(&harness.pool)
        .await
        .unwrap();
    let (status, body) = harness.admin("POST", &uri, Some(json!({}))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "ARTIFACT_RETAINED");
    sqlx::query("UPDATE artifacts SET retention_lock = FALSE WHERE id = $1")
        .bind(artifact.as_uuid())
        .execute(&harness.pool)
        .await
        .unwrap();

    // A check under way refuses it, and one asked for while it is pending
    // defers the tombstone.
    let (status, _) = harness
        .admin(
            "POST",
            &format!("/api/v1/artifacts/{artifact}/validate"),
            None,
        )
        .await;
    assert!(status.is_success(), "{status}");
    let (status, body) = harness.admin("POST", &uri, Some(json!({}))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "ARTIFACT_IN_USE");
    assert!(body["detail"].as_str().unwrap().contains("check"), "{body}");
    sqlx::query(
        "UPDATE integrity_checks SET state = 'canceled', completed_at = now(),
                lease_owner = NULL, lease_expires_at = NULL
         WHERE artifact_id = $1 AND state IN ('queued', 'running')",
    )
    .bind(artifact.as_uuid())
    .execute(&harness.pool)
    .await
    .unwrap();
    let (status, _) = harness.admin("POST", &uri, Some(json!({}))).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    assert_eq!(
        harness.actions(artifact).await,
        [
            "artifact.deletion_requested",
            "artifact.deletion_canceled",
            "artifact.deletion_requested",
        ]
    );

    // Only an administrator asks.
    let operator = support::sign_in(&harness.pool, Role::Operator).await;
    let (status, _) = harness.send(&operator, "DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = harness.send(&operator, "POST", &uri, Some(json!({}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn collection_waits_for_an_ingest_under_way() {
    let _serial = SERIAL.lock().await;
    let harness = harness().await;
    let artifact = harness.import("waits.bin", &unique("waits")).await;
    let digest = harness.digest_of(artifact).await;
    harness
        .admin(
            "POST",
            &format!("/api/v1/artifacts/{artifact}/deletion-requests"),
            Some(json!({ "confirm_original": true })),
        )
        .await;

    // An import holding the lock: the tombstone is made, the bytes wait.
    let ingesting = harness.objects.ingesting().await;
    let pass = harness.runner.pass().await.unwrap();
    assert!(pass.collection_deferred, "{pass:?}");
    assert!(harness.objects.object_path(&digest).exists());
    drop(ingesting);

    harness.runner.pass().await.unwrap();
    assert!(!harness.objects.object_path(&digest).exists());
}
