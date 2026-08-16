// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! End-to-end import orchestration.
//!
//! The first tests that exercise the whole pipeline: staged bytes through
//! hashing, detection, registration and manifest publication.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use tangible_api::{ImportCheckpoint, ImportError, ImportPipeline, ImportRequest};
use tangible_domain::enums::{ArtifactFormat, ValidationState};
use tangible_domain::{ImportJobId, ImportState, LogicalPath};
use tangible_storage::{FilesystemStore, IngestLimits, ManifestStore, StagingManager};
use tempfile::TempDir;

const SECTOR: usize = 2048;
const SYSTEM_AREA: usize = 16 * SECTOR;

fn fast() -> IngestLimits {
    IngestLimits {
        max_bytes: None,
        fsync: false,
    }
}

fn path(text: &str) -> LogicalPath {
    LogicalPath::parse(text).expect("valid path")
}

async fn pipeline() -> (TempDir, ImportPipeline) {
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
    (dir, ImportPipeline::new(staging, objects, manifests))
}

/// A minimal but structurally valid ISO of `blocks` 2048-byte blocks.
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

    // Pad to exactly the declared extent so the image is not reported as
    // truncated.
    bytes.resize(blocks as usize * SECTOR, 0);
    bytes
}

fn request(import_id: ImportJobId, filename: &str) -> ImportRequest {
    ImportRequest {
        import_id,
        source_kind: "upload".to_owned(),
        source_filename: Some(filename.to_owned()),
        source_reference: Some("upload:test".to_owned()),
        limits: fast(),
    }
}

// --- the happy path ----------------------------------------------------------

#[tokio::test]
async fn an_iso_imports_end_to_end() {
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    let image = iso_image("EXAMPLE_DISC", 20);
    area.write(&path("disc.iso"), &image).await.expect("stage");

    let mut checkpoint = ImportCheckpoint::default();
    let outcome = pipeline
        .run(&request(import_id, "disc.iso"), &mut checkpoint)
        .await
        .expect("import");

    assert_eq!(outcome.state, ImportState::Complete);
    assert!(outcome.manifest_path.exists());

    let manifest = pipeline
        .manifests()
        .read(outcome.artifact_id)
        .await
        .expect("read manifest");
    assert_eq!(manifest.classification.format, ArtifactFormat::Iso);
    assert_eq!(manifest.components.len(), 1);
    assert_eq!(manifest.classification.total_bytes, image.len() as u64);
    assert!(manifest.classification.format_confidence > 0.9);
}

#[tokio::test]
async fn the_bytes_land_in_the_object_store() {
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    area.write(&path("disc.iso"), &iso_image("X", 20))
        .await
        .expect("stage");

    let mut checkpoint = ImportCheckpoint::default();
    pipeline
        .run(&request(import_id, "disc.iso"), &mut checkpoint)
        .await
        .expect("import");

    let promoted = checkpoint.promoted.get("disc.iso").expect("promoted");
    let stat = pipeline
        .objects()
        .stat(&promoted.digest)
        .await
        .expect("stat")
        .expect("present");
    assert_eq!(stat.size_bytes, promoted.length_bytes);
}

#[tokio::test]
async fn a_multi_file_artifact_records_every_component() {
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    area.write(&path("disc.cue"), b"FILE \"disc.bin\" BINARY")
        .await
        .expect("stage");
    area.write(&path("disc.bin"), &iso_image("MULTI", 20))
        .await
        .expect("stage");

    let mut checkpoint = ImportCheckpoint::default();
    let outcome = pipeline
        .run(&request(import_id, "disc.cue"), &mut checkpoint)
        .await
        .expect("import");

    let manifest = pipeline
        .manifests()
        .read(outcome.artifact_id)
        .await
        .expect("read");
    assert_eq!(manifest.components.len(), 2);
    // Ordinals must be unique and stable; the manifest validates this on read.
    let mut ordinals: Vec<u32> = manifest.components.iter().map(|c| c.ordinal).collect();
    ordinals.sort_unstable();
    assert_eq!(ordinals, vec![0, 1]);
}

// --- detection feeds the manifest --------------------------------------------

#[tokio::test]
async fn a_truncated_image_still_imports_but_carries_the_warning() {
    // Preservation over tidiness: a bad dump is worth keeping, and the record
    // must say why it is bad rather than refusing it or silently fixing it.
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");

    let mut image = iso_image("TRUNCATED", 1000);
    image.truncate(SYSTEM_AREA + 4 * SECTOR); // far short of the declared extent
    area.write(&path("disc.iso"), &image).await.expect("stage");

    let mut checkpoint = ImportCheckpoint::default();
    let outcome = pipeline
        .run(&request(import_id, "disc.iso"), &mut checkpoint)
        .await
        .expect("a damaged image must still import");

    assert!(
        outcome.warnings.iter().any(|w| w.contains("truncated")),
        "warnings were {:?}",
        outcome.warnings
    );

    let manifest = pipeline
        .manifests()
        .read(outcome.artifact_id)
        .await
        .expect("read");
    assert_eq!(
        manifest.validation.state,
        ValidationState::ValidWithWarnings
    );
    assert!(!manifest.validation.validators.is_empty());
}

#[tokio::test]
async fn an_unrecognised_image_is_imported_as_unknown_rather_than_guessed() {
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    area.write(&path("mystery.iso"), &vec![0x42_u8; SYSTEM_AREA + SECTOR])
        .await
        .expect("stage");

    let mut checkpoint = ImportCheckpoint::default();
    let outcome = pipeline
        .run(&request(import_id, "mystery.iso"), &mut checkpoint)
        .await
        .expect("import");

    let manifest = pipeline
        .manifests()
        .read(outcome.artifact_id)
        .await
        .expect("read");
    assert_eq!(
        manifest.classification.format,
        ArtifactFormat::Unknown,
        "an .iso extension must not manufacture a format"
    );
}

#[tokio::test]
async fn detection_does_not_promise_burn_support() {
    // Detecting a format says nothing about whether any engine can write it.
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    area.write(&path("disc.iso"), &iso_image("X", 20))
        .await
        .expect("stage");

    let mut checkpoint = ImportCheckpoint::default();
    let outcome = pipeline
        .run(&request(import_id, "disc.iso"), &mut checkpoint)
        .await
        .expect("import");

    let manifest = pipeline
        .manifests()
        .read(outcome.artifact_id)
        .await
        .expect("read");
    assert_eq!(manifest.compatibility.burn_support.state, "unknown");
    assert!(manifest.compatibility.target_claims.is_empty());
}

// --- restart behaviour --------------------------------------------------------

#[tokio::test]
async fn a_resumed_import_reuses_the_same_artifact_identity() {
    // Otherwise a restart would create a second artifact for one set of bytes.
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    area.write(&path("disc.iso"), &iso_image("RESUME", 20))
        .await
        .expect("stage");

    let mut checkpoint = ImportCheckpoint::default();
    let first = pipeline
        .run(&request(import_id, "disc.iso"), &mut checkpoint)
        .await
        .expect("first run");

    let second = pipeline
        .run(&request(import_id, "disc.iso"), &mut checkpoint)
        .await
        .expect("second run");

    assert_eq!(first.artifact_id, second.artifact_id);
    assert_eq!(
        pipeline.manifests().list().await.expect("list").len(),
        1,
        "a resumed import must not publish a second manifest"
    );
}

#[tokio::test]
async fn resuming_does_not_re_promote_already_stored_components() {
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    area.write(&path("disc.iso"), &iso_image("X", 20))
        .await
        .expect("stage");

    let mut checkpoint = ImportCheckpoint::default();
    pipeline
        .run(&request(import_id, "disc.iso"), &mut checkpoint)
        .await
        .expect("first");
    let after_first = checkpoint.promoted.clone();

    pipeline
        .run(&request(import_id, "disc.iso"), &mut checkpoint)
        .await
        .expect("second");

    assert_eq!(
        after_first, checkpoint.promoted,
        "the recorded promotions must be unchanged by a resume"
    );
}

#[tokio::test]
async fn a_checkpoint_survives_serialization() {
    // It is persisted with the job, so it has to round-trip.
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    area.write(&path("disc.iso"), &iso_image("PERSIST", 20))
        .await
        .expect("stage");

    let mut checkpoint = ImportCheckpoint::default();
    let first = pipeline
        .run(&request(import_id, "disc.iso"), &mut checkpoint)
        .await
        .expect("first");

    let json = serde_json::to_string(&checkpoint).expect("serialize");
    let mut restored: ImportCheckpoint = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored, checkpoint);

    let second = pipeline
        .run(&request(import_id, "disc.iso"), &mut restored)
        .await
        .expect("resume from a restored checkpoint");
    assert_eq!(first.artifact_id, second.artifact_id);
}

#[tokio::test]
async fn an_import_resumed_from_a_mid_stage_checkpoint_completes() {
    // Simulates a crash after hashing: the promotions are recorded but nothing
    // downstream ran.
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    area.write(&path("disc.iso"), &iso_image("MIDWAY", 20))
        .await
        .expect("stage");

    let mut partial = ImportCheckpoint {
        state: ImportState::Hashing,
        ..ImportCheckpoint::default()
    };

    let outcome = pipeline
        .run(&request(import_id, "disc.iso"), &mut partial)
        .await
        .expect("resume");

    assert_eq!(outcome.state, ImportState::Complete);
    assert!(partial.promoted.contains_key("disc.iso"));
}

// --- failures ------------------------------------------------------------------

#[tokio::test]
async fn an_empty_staging_area_is_refused() {
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    pipeline.open_area(import_id).await.expect("area");

    let mut checkpoint = ImportCheckpoint::default();
    let error = pipeline
        .run(&request(import_id, "nothing.iso"), &mut checkpoint)
        .await
        .expect_err("must refuse");
    assert!(matches!(error, ImportError::NothingStaged { .. }));
}

#[tokio::test]
async fn exceeding_the_size_limit_fails_the_import_and_publishes_nothing() {
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    area.write(&path("disc.iso"), &iso_image("TOOBIG", 20))
        .await
        .expect("stage");

    let mut request = request(import_id, "disc.iso");
    request.limits = IngestLimits {
        max_bytes: Some(64),
        fsync: false,
    };

    let mut checkpoint = ImportCheckpoint::default();
    assert!(pipeline.run(&request, &mut checkpoint).await.is_err());
    assert!(
        pipeline.manifests().list().await.expect("list").is_empty(),
        "a failed import must publish no manifest"
    );
}

#[tokio::test]
async fn a_failed_import_keeps_its_staged_bytes_for_diagnosis() {
    // The staged tree is the only evidence a bad import leaves, so discarding
    // is a separate, deliberate step.
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    area.write(&path("disc.iso"), &iso_image("KEEP", 20))
        .await
        .expect("stage");

    let mut request = request(import_id, "disc.iso");
    request.limits = IngestLimits {
        max_bytes: Some(64),
        fsync: false,
    };
    let mut checkpoint = ImportCheckpoint::default();
    let _ = pipeline.run(&request, &mut checkpoint).await;

    assert!(area.path().join("disc.iso").exists());
}

#[tokio::test]
async fn discarding_cleans_up_after_a_completed_import() {
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    area.write(&path("disc.iso"), &iso_image("CLEAN", 20))
        .await
        .expect("stage");

    let mut checkpoint = ImportCheckpoint::default();
    let outcome = pipeline
        .run(&request(import_id, "disc.iso"), &mut checkpoint)
        .await
        .expect("import");

    assert!(pipeline.discard(import_id).await.expect("discard"));
    assert!(!area.path().exists());

    // The artifact and its manifest outlive the staging area.
    assert!(pipeline.manifests().read(outcome.artifact_id).await.is_ok());
}

#[tokio::test]
async fn two_imports_of_identical_bytes_deduplicate_but_stay_distinct_artifacts() {
    // Same bytes stored once; two catalog entries, because an artifact is a
    // catalog identity rather than a content digest.
    let (_dir, pipeline) = pipeline().await;
    let image = iso_image("SAME", 20);

    let mut ids = Vec::new();
    for _ in 0..2 {
        let import_id = ImportJobId::generate();
        let area = pipeline.open_area(import_id).await.expect("area");
        area.write(&path("disc.iso"), &image).await.expect("stage");
        let mut checkpoint = ImportCheckpoint::default();
        let outcome = pipeline
            .run(&request(import_id, "disc.iso"), &mut checkpoint)
            .await
            .expect("import");
        ids.push(outcome.artifact_id);
    }

    assert_ne!(ids[0], ids[1], "two imports are two artifacts");
    assert_eq!(
        pipeline.manifests().list().await.expect("list").len(),
        2,
        "each artifact has its own manifest"
    );
}
