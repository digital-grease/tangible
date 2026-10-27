// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Importing an object from an S3-compatible bucket
//! (docs/plans/acquisition.md A6), through the real router and runner,
//! against a real S3-compatible server.
//!
//! Gated like PostgreSQL, and on a server too: each test skips unless
//! `TANGIBLE_TEST_S3_ENDPOINT`, `TANGIBLE_TEST_S3_ACCESS_KEY` and
//! `TANGIBLE_TEST_S3_SECRET_KEY` name one. CI runs SeaweedFS's S3 gateway
//! (MinIO no longer publishes images), as the fixtures in
//! fixtures/tool-output/s3 were recorded from:
//!
//! ```sh
//! docker run -d -p 127.0.0.1:18333:8333 -v "$PWD/s3.json:/etc/seaweedfs/s3.json:ro" \
//!   chrislusf/seaweedfs:4.48 server -s3 -s3.config=/etc/seaweedfs/s3.json -volume.max=64
//! ```
//!
//! Every test makes a bucket of its own, so runs never meet. Each bucket is
//! a SeaweedFS collection with volumes of its own, and by default a server
//! has room for only a few: `-volume.max` lets many runs share one server.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use rusty_s3::{Bucket, Credentials, S3Action as _, UrlStyle};
use serde_json::{Value, json};
use sqlx::PgPool;
use tangible_api::integrity::{Integrity, IntegrityRunner, IntegrityRunnerSettings};
use tangible_api::secrets::SecretKey;
use tangible_api::{
    ApiState, ImportContext, ImportPipeline, ImportRunner, ImportRunnerSettings,
    SourceRemovalHandle, SourceRemovalSettings, SourceRemover, router,
};
use tangible_domain::Role;
use tangible_integrations::outbound::{OutboundClient, OutboundPolicy};
use tangible_storage::{FilesystemStore, ManifestStore, StagingManager, WatchRoots};
use tempfile::TempDir;
use tower::ServiceExt as _;

mod support;

/// The S3 server under test, when one is named.
struct Server {
    endpoint: String,
    access_key: String,
    secret_key: String,
}

fn server() -> Option<Server> {
    let vars = [
        "TANGIBLE_TEST_S3_ENDPOINT",
        "TANGIBLE_TEST_S3_ACCESS_KEY",
        "TANGIBLE_TEST_S3_SECRET_KEY",
    ]
    .map(|name| std::env::var(name).ok());
    let [Some(endpoint), Some(access_key), Some(secret_key)] = vars else {
        eprintln!("skipping: set TANGIBLE_TEST_S3_ENDPOINT, _ACCESS_KEY and _SECRET_KEY");
        return None;
    };
    Some(Server {
        endpoint,
        access_key,
        secret_key,
    })
}

impl Server {
    fn bucket(&self, name: &str) -> Bucket {
        Bucket::new(
            self.endpoint.parse().unwrap(),
            UrlStyle::Path,
            name.to_owned(),
            "us-east-1",
        )
        .unwrap()
    }

    fn credentials(&self) -> Credentials {
        Credentials::new(&self.access_key, &self.secret_key)
    }

    /// A new bucket, versioned.
    async fn make_bucket(&self) -> String {
        let name = self.make_plain_bucket().await;
        let bucket = self.bucket(&name);
        // PUT ?versioning on the bucket: the same request shape as making it.
        let credentials = self.credentials();
        let mut versioning = bucket.create_bucket(&credentials);
        versioning.query_mut().insert("versioning", "");
        let versioned = reqwest::Client::new()
            .put(versioning.sign(Duration::from_secs(60)).as_str())
            .body(
                "<VersioningConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                 <Status>Enabled</Status></VersioningConfiguration>",
            )
            .send()
            .await
            .unwrap();
        assert!(versioned.status().is_success(), "{}", versioned.status());
        name
    }

    /// A new bucket that keeps no versions.
    async fn make_plain_bucket(&self) -> String {
        let name = format!(
            "tangible-{}",
            &uuid::Uuid::now_v7().simple().to_string()[16..]
        );
        let bucket = self.bucket(&name);
        let http = reqwest::Client::new();
        let made = http
            .put(
                bucket
                    .create_bucket(&self.credentials())
                    .sign(Duration::from_secs(60))
                    .as_str(),
            )
            .send()
            .await
            .unwrap();
        assert!(made.status().is_success(), "{}", made.status());
        name
    }

    /// The status and bytes of a GET of `key`, at `version` when given.
    async fn get(&self, bucket: &str, key: &str, version: Option<&str>) -> (u16, Vec<u8>) {
        let credentials = self.credentials();
        let bucket = self.bucket(bucket);
        let mut action = bucket.get_object(Some(&credentials), key);
        if let Some(version) = version {
            action.query_mut().insert("versionId", version.to_owned());
        }
        let answer = reqwest::Client::new()
            .get(action.sign(Duration::from_secs(60)).as_str())
            .send()
            .await
            .unwrap();
        let status = answer.status().as_u16();
        (status, answer.bytes().await.unwrap().to_vec())
    }

    async fn put(&self, bucket: &str, key: &str, bytes: Vec<u8>) {
        let url = self
            .bucket(bucket)
            .put_object(Some(&self.credentials()), key)
            .sign(Duration::from_secs(60));
        let put = reqwest::Client::new()
            .put(url.as_str())
            .body(bytes)
            .send()
            .await
            .unwrap();
        assert!(put.status().is_success(), "{}", put.status());
    }
}

struct Harness {
    pool: PgPool,
    state: ApiState,
    runner: ImportRunner,
    remover: SourceRemover,
    integrity: IntegrityRunner,
    admin: support::Session,
    operator: support::Session,
    _dir: TempDir,
    _queue: tokio::sync::MutexGuard<'static, ()>,
}

static QUEUE: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

async fn harness(max_upload_bytes: u64) -> Harness {
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
    // The server under test listens on loopback, which only an allowed block
    // lets in.
    let client = OutboundClient::new(
        OutboundPolicy::for_integrations(
            vec!["127.0.0.0/8".parse().unwrap(), "::1/128".parse().unwrap()],
            None,
        ),
        "tangible-test",
    )
    .unwrap();
    let key = Arc::new(SecretKey::from_base64(&SecretKey::generate().unwrap()).unwrap());
    let state = ApiState::with_manifests(database.clone(), manifests.clone())
        .with_imports(ImportContext {
            pipeline: pipeline.clone(),
            roots: WatchRoots::new(BTreeMap::new()),
            max_upload_bytes,
            outbound: None,
            upload_part_bytes: 4096,
        })
        .with_secret_key(Arc::clone(&key))
        .with_integration_client(client.clone());
    let integrity = Integrity::new();
    let remover = SourceRemover::new(
        database.clone(),
        integrity.clone(),
        Some(client.clone()),
        Some(Arc::clone(&key)),
        SourceRemovalHandle::new(),
        SourceRemovalSettings {
            owner: format!("test-{}", uuid::Uuid::now_v7()),
            check_poll: Duration::ZERO,
            first_retry: Duration::ZERO,
            ..SourceRemovalSettings::default()
        },
    );
    let integrity = IntegrityRunner::new(
        database.clone(),
        objects,
        manifests,
        integrity,
        IntegrityRunnerSettings {
            owner: format!("test-{}", uuid::Uuid::now_v7()),
            ..IntegrityRunnerSettings::default()
        },
    );
    let runner = ImportRunner::new(
        database,
        pipeline,
        ImportRunnerSettings {
            owner: format!("test-{}", uuid::Uuid::now_v7()),
            integrations: Some(client),
            secret_key: Some(key),
            retry_backoff: Duration::ZERO,
            ..ImportRunnerSettings::default()
        },
    );
    sqlx::query(
        "UPDATE import_jobs SET state = 'canceled', lease_owner = NULL, lease_expires_at = NULL
         WHERE state IN ('requested', 'acquiring', 'staged', 'hashing', 'inspecting',
                         'registering', 'failed_retryable')",
    )
    .execute(&pool)
    .await
    .unwrap();
    Harness {
        admin: support::administrator(&pool).await,
        operator: support::sign_in(&pool, Role::Operator).await,
        pool,
        state,
        runner,
        remover,
        integrity,
        _dir: dir,
        _queue: queue,
    }
}

impl Harness {
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
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// An S3 integration for `server` allowing `buckets`, with `secret_key`.
    async fn integration(&self, server: &Server, buckets: &[&str], secret_key: &str) -> String {
        self.integration_after(server, buckets, secret_key, "keep")
            .await
    }

    /// The same, doing `after_import` with an object once it is imported.
    async fn integration_after(
        &self,
        server: &Server,
        buckets: &[&str],
        secret_key: &str,
        after_import: &str,
    ) -> String {
        let (status, made) = self
            .send(
                &self.admin,
                Method::POST,
                "/api/v1/integrations",
                Some(json!({
                    "kind": "s3",
                    "name": format!("s3-{}", uuid::Uuid::now_v7().simple()),
                    "settings": {
                        "endpoint": server.endpoint,
                        "buckets": buckets,
                        "after_import": after_import,
                    },
                    "secrets": {
                        "access_key_id": server.access_key,
                        "secret_access_key": secret_key,
                    },
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
        made["id"].as_str().unwrap().to_owned()
    }

    async fn import(&self, body: Value) -> (StatusCode, Value) {
        self.send(
            &self.operator,
            Method::POST,
            "/api/v1/imports/s3",
            Some(body),
        )
        .await
    }

    async fn settle(&self, id: &str) -> Value {
        for _ in 0..20 {
            if !self.runner.step().await.unwrap() {
                break;
            }
        }
        self.send(
            &self.admin,
            Method::GET,
            &format!("/api/v1/imports/{id}"),
            None,
        )
        .await
        .1
    }
}

impl Harness {
    /// Run the remover and the integrity checks until neither has anything
    /// due, and answer the import's source removal.
    async fn removal(&self, id: &str) -> Value {
        for _ in 0..20 {
            let removed = self.remover.step().await.unwrap();
            let checked = self.integrity.step().await.unwrap();
            if !removed && !checked {
                break;
            }
        }
        self.send(
            &self.admin,
            Method::GET,
            &format!("/api/v1/imports/{id}"),
            None,
        )
        .await
        .1["source_removal"]
            .clone()
    }

    /// The version an import pinned.
    async fn pinned_version(&self, id: &str) -> String {
        sqlx::query_scalar(
            "SELECT source_descriptor_json->>'version_id' FROM import_jobs WHERE id = $1",
        )
        .bind(uuid::Uuid::parse_str(id).unwrap())
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }
}

fn iso_image(label: [u8; 4]) -> Vec<u8> {
    const SECTOR: usize = 2048;
    let mut bytes = vec![0_u8; 16 * SECTOR];
    let mut pvd = vec![0_u8; SECTOR];
    pvd[0] = 1;
    pvd[1..6].copy_from_slice(b"CD001");
    pvd[6] = 1;
    for slot in &mut pvd[40..72] {
        *slot = b' ';
    }
    pvd[40..44].copy_from_slice(&label);
    pvd[80..84].copy_from_slice(&32_u32.to_le_bytes());
    pvd[84..88].copy_from_slice(&32_u32.to_be_bytes());
    pvd[128..130].copy_from_slice(&2048_u16.to_le_bytes());
    pvd[130..132].copy_from_slice(&2048_u16.to_be_bytes());
    bytes.extend_from_slice(&pvd);
    let mut terminator = vec![0_u8; SECTOR];
    terminator[0] = 255;
    terminator[1..6].copy_from_slice(b"CD001");
    terminator[6] = 1;
    bytes.extend_from_slice(&terminator);
    bytes.resize(32 * SECTOR, 0);
    bytes
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    hex::encode(Sha256::digest(bytes))
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_object_is_imported_pinned_to_the_version_asked_for() {
    let Some(server) = server() else { return };
    let harness = harness(64 * 1024 * 1024).await;
    let bucket = server.make_bucket().await;
    let first = iso_image(*b"ONE ");
    server
        .put(&bucket, "discs/Probe Disc.iso", first.clone())
        .await;
    let integration = harness
        .integration(&server, &[&bucket], &server.secret_key)
        .await;

    // Operators see the integration and its buckets.
    let (_, sources) = harness
        .send(
            &harness.operator,
            Method::GET,
            "/api/v1/import-sources",
            None,
        )
        .await;
    assert!(
        sources["s3_sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|source| source["id"] == integration.as_str()
                && source["buckets"] == json!([bucket])),
        "{sources}"
    );

    let (status, queued) = harness
        .import(json!({
            "integration_id": integration,
            "bucket": bucket,
            "key": "discs/Probe Disc.iso",
            "expected_sha256": sha256(&first),
        }))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{queued}");
    assert_eq!(queued["source_kind"], "s3_object");
    assert_eq!(queued["source_filename"], "Probe Disc.iso");
    assert_eq!(queued["bytes_expected"], first.len());
    let id = queued["id"].as_str().unwrap().to_owned();

    // Written over before the runner fetches it: the import still gets the
    // version it was asked for.
    server
        .put(&bucket, "discs/Probe Disc.iso", iso_image(*b"TWO "))
        .await;
    let done = harness.settle(&id).await;
    assert_eq!(done["state"], "complete", "{done}");
    assert_eq!(
        done["source_reference"],
        format!("s3://{bucket}/discs/Probe Disc.iso")
    );
    let artifact = done["artifact_id"].as_str().unwrap();
    let (_, manifest) = harness
        .send(
            &harness.admin,
            Method::GET,
            &format!("/api/v1/artifacts/{artifact}/manifest"),
            None,
        )
        .await;
    assert_eq!(
        manifest["components"][0]["content"]["sha256"],
        sha256(&first),
        "{manifest}"
    );
    assert!(
        !manifest.to_string().contains("X-Amz-"),
        "no presigned URL in the manifest: {manifest}"
    );

    let audit: Value = sqlx::query_scalar(
        "SELECT metadata FROM audit_events WHERE action = 'import.s3_requested' AND target_id = $1",
    )
    .bind(&id)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert!(audit["version_id"].is_string(), "{audit}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn what_will_not_do_is_refused_before_anything_is_queued() {
    let Some(server) = server() else { return };
    let harness = harness(1024).await;
    let bucket = server.make_bucket().await;
    server.put(&bucket, "big.iso", iso_image(*b"BIG ")).await;
    let integration = harness
        .integration(&server, &[&bucket], &server.secret_key)
        .await;
    let ask = |bucket: &str, key: &str| json!({ "integration_id": integration, "bucket": bucket, "key": key });

    let (status, problem) = harness.import(ask("other-bucket", "big.iso")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("does not allow bucket")
    );

    let (status, problem) = harness.import(ask(&bucket, "nope.iso")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert_eq!(problem["code"], "REFERENCE_NOT_FOUND");

    let (status, problem) = harness.import(ask(&bucket, "big.iso")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("past this server's limit")
    );

    let (status, problem) = harness.import(ask(&bucket, "a\nb")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");

    // A dot segment would be normalised into another bucket's path.
    let (status, problem) = harness
        .import(ask(&bucket, "../other-bucket/big.iso"))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert!(
        problem["detail"].as_str().unwrap().contains("'..'"),
        "{problem}"
    );

    // A wrong secret is the endpoint's refusal, never repeated back.
    let wrong = harness
        .integration(&server, &[&bucket], "a-wrong-secret")
        .await;
    let (status, problem) = harness
        .import(json!({ "integration_id": wrong, "bucket": bucket, "key": "big.iso" }))
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{problem}");
    assert_eq!(problem["code"], "INTEGRATION_AUTH_FAILED");
    assert!(!problem.to_string().contains("a-wrong-secret"));

    // A viewer may not.
    let viewer = support::sign_in(&harness.pool, Role::Viewer).await;
    let (status, _) = harness
        .send(
            &viewer,
            Method::POST,
            "/api/v1/imports/s3",
            Some(ask(&bucket, "big.iso")),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn trying_an_s3_integration_checks_every_bucket() {
    let Some(server) = server() else { return };
    let harness = harness(1024).await;
    let bucket = server.make_bucket().await;
    let good = harness
        .integration(&server, &[&bucket], &server.secret_key)
        .await;
    let (status, tried) = harness
        .send(
            &harness.admin,
            Method::POST,
            &format!("/api/v1/integrations/{good}/test"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{tried}");
    assert_eq!(tried["ok"], true, "{tried}");

    let missing = harness
        .integration(&server, &[&bucket, "tangible-not-made"], &server.secret_key)
        .await;
    let (_, tried) = harness
        .send(
            &harness.admin,
            Method::POST,
            &format!("/api/v1/integrations/{missing}/test"),
            None,
        )
        .await;
    assert_eq!(tried["ok"], false, "{tried}");
    assert!(
        tried["detail"]
            .as_str()
            .unwrap()
            .contains("tangible-not-made")
    );

    let wrong = harness
        .integration(&server, &[&bucket], "a-wrong-secret")
        .await;
    let (_, tried) = harness
        .send(
            &harness.admin,
            Method::POST,
            &format!("/api/v1/integrations/{wrong}/test"),
            None,
        )
        .await;
    assert_eq!(tried["ok"], false, "{tried}");
    assert_eq!(tried["error_code"], "INTEGRATION_AUTH_FAILED");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_versioned_object_loses_only_the_version_imported() {
    let Some(server) = server() else { return };
    let harness = harness(64 * 1024 * 1024).await;
    let bucket = server.make_bucket().await;
    let key = "discs/Removed Disc.iso";
    let first = iso_image(*b"RM1 ");
    server.put(&bucket, key, first.clone()).await;
    let integration = harness
        .integration_after(&server, &[&bucket], &server.secret_key, "remove")
        .await;
    let (status, queued) = harness
        .import(json!({ "integration_id": integration, "bucket": bucket, "key": key }))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{queued}");
    let id = queued["id"].as_str().unwrap().to_owned();
    let done = harness.settle(&id).await;
    assert_eq!(done["state"], "complete", "{done}");
    assert_eq!(done["source_removal"]["state"], "verifying", "{done}");
    let imported = harness.pinned_version(&id).await;

    // A newer version written after the import is the operator's.
    let second = iso_image(*b"RM2 ");
    server.put(&bucket, key, second.clone()).await;

    let removed = harness.removal(&id).await;
    assert_eq!(removed["state"], "removed", "{removed}");
    assert_eq!(
        removed["detail"],
        format!("Deleted the imported version of the object from bucket {bucket}")
    );
    assert_eq!(removed["files_removed"], 1);
    assert_eq!(removed["bytes_removed"], first.len());
    assert_eq!(
        server.get(&bucket, key, Some(&imported)).await.0,
        404,
        "the imported version is gone"
    );
    assert_eq!(
        server.get(&bucket, key, None).await,
        (200, second),
        "the newer version, and no delete marker, is left"
    );

    let audited: Value = sqlx::query_scalar(
        "SELECT metadata FROM audit_events
         WHERE action = 'integration.source_deleted' AND target_id = $1",
    )
    .bind(&integration)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(audited["removed"]["version_id"], imported.as_str());
    assert_eq!(audited["removed"]["key"], key);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_unversioned_object_is_deleted_only_while_it_is_what_was_imported() {
    let Some(server) = server() else { return };
    let harness = harness(64 * 1024 * 1024).await;
    let bucket = server.make_plain_bucket().await;
    let integration = harness
        .integration_after(&server, &[&bucket], &server.secret_key, "remove")
        .await;

    // Written over after the import: the same size, another ETag.
    let replaced_key = "discs/Replaced.iso";
    server.put(&bucket, replaced_key, iso_image(*b"OLD ")).await;
    let (_, queued) = harness
        .import(json!({ "integration_id": integration, "bucket": bucket, "key": replaced_key }))
        .await;
    let replaced = queued["id"].as_str().unwrap().to_owned();
    assert_eq!(harness.settle(&replaced).await["state"], "complete");
    let newer = iso_image(*b"NEW ");
    server.put(&bucket, replaced_key, newer.clone()).await;
    let kept = harness.removal(&replaced).await;
    assert_eq!(kept["state"], "kept", "{kept}");
    assert!(
        kept["detail"].as_str().unwrap().contains("changed since"),
        "{kept}"
    );
    assert_eq!(server.get(&bucket, replaced_key, None).await, (200, newer));

    // Left alone since: deleted.
    let key = "discs/Unchanged.iso";
    server.put(&bucket, key, iso_image(*b"SAME")).await;
    let (_, queued) = harness
        .import(json!({ "integration_id": integration, "bucket": bucket, "key": key }))
        .await;
    let unchanged = queued["id"].as_str().unwrap().to_owned();
    assert_eq!(harness.settle(&unchanged).await["state"], "complete");
    let removed = harness.removal(&unchanged).await;
    assert_eq!(removed["state"], "removed", "{removed}");
    assert_eq!(
        removed["detail"],
        format!("Deleted the object from bucket {bucket}")
    );
    assert_eq!(server.get(&bucket, key, None).await.0, 404);
}
