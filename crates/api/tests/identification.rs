// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Preservation hashes through the real import pipeline, catalogue and
//! runner (docs/plans/dat-matching.md H1): every component's CRC32, MD5 and
//! SHA-1, and every CD track's over the bytes Redump cuts the track's BIN
//! from, checked against the hashes of those bytes alone.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tangible_api::identification::{
    IdentificationHandle, IdentificationRunner, IdentificationSettings,
};
use tangible_api::{ApiState, ImportCheckpoint, ImportPipeline, ImportRequest, catalog, router};
use tangible_domain::{ImportJobId, LogicalPath};
use tangible_storage::preservation::{PreservationHashes, hash_bytes};
use tangible_storage::{FilesystemStore, IngestLimits, ManifestStore, StagingManager};
use tempfile::TempDir;
use tower::ServiceExt as _;

mod support;

const RAW: usize = 2352;

struct Harness {
    _dir: TempDir,
    objects: FilesystemStore,
    pipeline: ImportPipeline,
    database: tangible_db::Database,
    runner: IdentificationRunner,
    router: axum::Router,
}

async fn harness(max_attempts: i32) -> Harness {
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
    let session = support::sign_in(database.pool(), tangible_domain::Role::Operator).await;
    let handle = IdentificationHandle::new();
    let runner = IdentificationRunner::new(
        database.clone(),
        objects.clone(),
        manifests.clone(),
        handle.clone(),
        IdentificationSettings {
            owner: format!("test-{}", uuid::Uuid::now_v7()),
            retry_after: std::time::Duration::ZERO,
            max_attempts,
            ..IdentificationSettings::default()
        },
    );
    let state = ApiState::with_manifests(database.clone(), manifests).with_identification(handle);
    Harness {
        _dir: dir,
        objects,
        pipeline,
        database,
        runner,
        router: support::signed_in(router(state), session),
    }
}

impl Harness {
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

    async fn send(&self, method: &str, uri: &str) -> (StatusCode, Value) {
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn identification(&self, artifact: &str) -> Value {
        let (status, view) = self
            .send(
                "GET",
                &format!("/api/v1/artifacts/{artifact}/identification"),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{view}");
        view
    }

    async fn identify(&self, artifact: &str) {
        assert!(
            self.runner
                .identify(uuid::Uuid::parse_str(artifact).unwrap())
                .await
                .unwrap(),
            "the artifact was pending"
        );
    }
}

fn same(view: &Value, expected: &PreservationHashes) {
    assert_eq!(view["crc32"], expected.crc32.as_str(), "{view}");
    assert_eq!(view["md5"], expected.md5.as_str(), "{view}");
    assert_eq!(view["sha1"], expected.sha1.as_str(), "{view}");
    assert_eq!(view["length_bytes"], expected.length, "{view}");
}

fn component<'a>(view: &'a Value, path: &str) -> &'a Value {
    view["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["logical_path"] == path)
        .unwrap_or_else(|| panic!("{path} in {view}"))
}

/// Raw sectors, each tagged with its number and this test's tag, so the
/// image is this test's own and every range differs.
fn sectors(count: usize, tag: &str) -> Vec<u8> {
    let mut bytes = vec![0_u8; count * RAW];
    for (n, sector) in bytes.chunks_mut(RAW).enumerate() {
        sector[100..108].copy_from_slice(&(n as u64).to_le_bytes());
        sector[108..108 + tag.len()].copy_from_slice(tag.as_bytes());
    }
    bytes
}

fn tag() -> String {
    uuid::Uuid::now_v7().simple().to_string()
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_cue_bin_gets_each_files_and_each_tracks_hashes() {
    let h = harness(3).await;
    // A data track of 100 sectors, then an audio track with a 75-sector
    // pregap in the file and 125 sectors after it: Redump's track 2 BIN is
    // the last 200 sectors.
    let sheet = b"FILE \"disc.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n  \
        TRACK 02 AUDIO\n    INDEX 00 00:01:25\n    INDEX 01 00:02:25\n"
        .to_vec();
    let bin = sectors(300, &tag());
    let artifact = h
        .import(&[("disc.cue", sheet.clone()), ("disc.bin", bin.clone())])
        .await;
    let before = h.identification(&artifact).await;
    assert_eq!(before["state"], "pending", "{before}");
    assert_eq!(component(&before, "disc.bin")["sha1"], Value::Null);

    h.identify(&artifact).await;
    let view = h.identification(&artifact).await;
    assert_eq!(view["state"], "done", "{view}");
    assert!(view["hashed_at"].is_string());
    same(component(&view, "disc.cue"), &hash_bytes(&sheet));
    same(component(&view, "disc.bin"), &hash_bytes(&bin));
    let tracks = view["tracks"].as_array().unwrap();
    assert_eq!(tracks.len(), 2, "{view}");
    assert_eq!(tracks[0]["number"], 1);
    same(&tracks[0], &hash_bytes(&bin[..100 * RAW]));
    assert_eq!(tracks[1]["number"], 2);
    same(&tracks[1], &hash_bytes(&bin[100 * RAW..]));
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_clonecd_image_gets_its_tracks_hashes_from_the_img() {
    let h = harness(3).await;
    let descriptor = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/images/ccd/mixed-mode.ccd"
    ))
    .unwrap();
    let mut img = sectors(750, &tag());
    img[..12].copy_from_slice(&[
        0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0,
    ]);
    img[15] = 1;
    let sub = vec![7_u8; 750 * 96];
    let artifact = h
        .import(&[
            ("Game.ccd", descriptor),
            ("Game.img", img.clone()),
            ("Game.sub", sub.clone()),
        ])
        .await;
    h.identify(&artifact).await;
    let view = h.identification(&artifact).await;
    same(component(&view, "Game.sub"), &hash_bytes(&sub));
    let tracks = view["tracks"].as_array().unwrap();
    // The fixture's tracks: 0..300, 300..600 (its pregap included), 600..750.
    for (track, (start, end)) in tracks.iter().zip([(0, 300), (300, 600), (600, 750)]) {
        same(track, &hash_bytes(&img[start * RAW..end * RAW]));
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_unreadable_object_fails_saying_why_and_can_be_asked_for_again() {
    let h = harness(1).await;
    let bin = sectors(10, &tag());
    let artifact = h
        .import(&[
            (
                "disc.cue",
                b"FILE \"disc.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n"
                    .to_vec(),
            ),
            ("disc.bin", bin.clone()),
        ])
        .await;
    let digest: tangible_domain::Sha256Digest = {
        use sha2::{Digest as _, Sha256};
        hex::encode(Sha256::digest(&bin)).parse().unwrap()
    };
    let object = h.objects.object_path(&digest);
    let kept = std::fs::read(&object).unwrap();
    let mut permissions = std::fs::metadata(&object).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(&object, permissions).unwrap();
    std::fs::remove_file(&object).unwrap();

    h.identify(&artifact).await;
    let failed = h.identification(&artifact).await;
    assert_eq!(failed["state"], "failed", "{failed}");
    assert!(
        failed["error"]
            .as_str()
            .unwrap()
            .contains("disc.bin could not be read"),
        "{failed}"
    );

    // Put back, and asked for again.
    std::fs::write(&object, &kept).unwrap();
    let (status, queued) = h
        .send(
            "POST",
            &format!("/api/v1/artifacts/{artifact}/identification/rehash"),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{queued}");
    assert_eq!(queued["state"], "pending");
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events
         WHERE action = 'artifact.rehash_requested' AND target_id = $1 AND actor_type = 'user'",
    )
    .bind(&artifact)
    .fetch_one(h.database.pool())
    .await
    .unwrap();
    assert_eq!(audited, 1, "asking again is audited");
    h.identify(&artifact).await;
    let view = h.identification(&artifact).await;
    assert_eq!(view["state"], "done", "{view}");
    same(&view["tracks"][0], &hash_bytes(&bin));

    let (status, _) = h
        .send(
            "GET",
            "/api/v1/artifacts/01a1088f-0165-7652-819b-cac12fcbe043/identification",
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
