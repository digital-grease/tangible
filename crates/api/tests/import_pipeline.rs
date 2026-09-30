// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! End-to-end import orchestration.
//!
//! The first tests that exercise the whole pipeline: staged bytes through
//! hashing, detection, registration and manifest publication.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use tangible_api::{ImportCheckpoint, ImportError, ImportPipeline, ImportRequest};
use tangible_domain::cd::{SampleByteOrder, TrackFlag};
use tangible_domain::enums::{ArtifactFormat, ComponentRole, MediaFamily, ValidationState};
use tangible_domain::manifest::Topology;
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

// --- CUE and BIN -------------------------------------------------------------

/// Bytes per sector for the raw modes these sheets use.
const RAW_SECTOR: usize = 2352;

/// A mixed-mode sheet: a data track, then audio after a generated gap.
const MIXED_MODE_SHEET: &[u8] = b"CATALOG 1234567890123\n\
FILE \"disc.bin\" BINARY\n\
  TRACK 01 MODE2/2352\n\
    INDEX 01 00:00:00\n\
  TRACK 02 AUDIO\n\
    PREGAP 00:00:02\n\
    ISRC USRC17607839\n\
    FLAGS PRE\n\
    INDEX 01 00:01:00\n";

#[tokio::test]
async fn a_cue_and_its_bin_import_as_one_artifact_that_knows_its_tracks() {
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    area.write(&path("disc.cue"), MIXED_MODE_SHEET)
        .await
        .expect("stage");
    area.write(&path("disc.bin"), &vec![0_u8; RAW_SECTOR * 100])
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

    assert_eq!(manifest.classification.format, ArtifactFormat::CueBin);
    assert_eq!(
        manifest.classification.media_family,
        Some(MediaFamily::Cd),
        "a CUE sheet describes a CD and nothing else"
    );
    assert!(manifest.classification.format_confidence > 0.98);
    assert_eq!(manifest.classification.detectors[0].name, "cue-parser");

    // One artifact, two components, each with the role it actually plays.
    let descriptor = manifest
        .components
        .iter()
        .find(|component| component.logical_path.as_str() == "disc.cue")
        .expect("the descriptor is a component");
    let data = manifest
        .components
        .iter()
        .find(|component| component.logical_path.as_str() == "disc.bin")
        .expect("the data is a component");
    assert_eq!(descriptor.role, ComponentRole::Descriptor);
    assert_eq!(data.role, ComponentRole::TrackData);

    let Topology::CdTracks {
        descriptor_component_id,
        catalog,
        session_count,
        tracks,
        subchannel,
    } = manifest.topology
    else {
        panic!("expected a track topology, got {:?}", manifest.topology);
    };
    assert_eq!(descriptor_component_id, descriptor.id);
    assert_eq!(catalog.as_deref(), Some("1234567890123"));
    assert_eq!(session_count, 1);
    assert!(
        !subchannel.expect("stated").present,
        "a CUE/BIN set carries no subchannel data"
    );

    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].number, 1);
    assert_eq!(tracks[0].mode, "MODE2/2352");
    assert_eq!(tracks[0].component_id, data.id);
    assert_eq!(tracks[0].start_lba, 0);
    assert_eq!(tracks[0].sector_count, 75);
    assert_eq!(tracks[0].file_offset_bytes, 0);

    assert_eq!(tracks[1].mode, "AUDIO");
    assert_eq!(tracks[1].component_id, data.id);
    assert_eq!(tracks[1].file_offset_bytes, 75 * RAW_SECTOR as u64);
    assert_eq!(tracks[1].start_lba, 77, "the generated gap is on the disc");
    assert_eq!(tracks[1].sector_count, 25);
    assert_eq!(tracks[1].pregap_sectors, 2);
    assert_eq!(
        tracks[1].isrc.as_deref(),
        Some("USRC17607839"),
        "a track's recording code is part of the disc"
    );
    assert_eq!(
        tracks[1].flags,
        vec![TrackFlag::PreEmphasis],
        "so is its pre-emphasis, which a player has to undo"
    );
    assert_eq!(
        tracks[1].sample_byte_order,
        Some(SampleByteOrder::LittleEndian),
        "a BINARY file is little-endian, and nothing in its bytes says so"
    );
}

#[tokio::test]
async fn a_cue_whose_bin_was_not_staged_imports_without_claiming_a_layout() {
    // Half a dump. The sheet is worth keeping and the tracks are not known,
    // and the manifest has to say both.
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    area.write(&path("disc.cue"), MIXED_MODE_SHEET)
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

    assert_eq!(manifest.classification.format, ArtifactFormat::CueBin);
    assert_eq!(manifest.topology, Topology::Unknown);
    assert_eq!(
        manifest.validation.state,
        ValidationState::ValidWithWarnings
    );
    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning.contains("disc.bin")),
        "the missing file must be named: {:?}",
        outcome.warnings
    );
}

#[tokio::test]
async fn a_cue_naming_a_file_outside_staging_reaches_nothing() {
    // The trust boundary, exercised through the whole pipeline rather than
    // only in the resolver's own tests.
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    area.write(
        &path("disc.cue"),
        b"FILE \"../../../etc/passwd\" BINARY\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n",
    )
    .await
    .expect("stage");
    area.write(&path("disc.bin"), &vec![0_u8; RAW_SECTOR * 10])
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

    assert_eq!(manifest.topology, Topology::Unknown);
    assert!(
        manifest
            .components
            .iter()
            .all(|component| matches!(component.logical_path.as_str(), "disc.cue" | "disc.bin")),
        "only staged files may appear as components"
    );
    assert!(
        manifest
            .components
            .iter()
            .all(|component| component.role != ComponentRole::TrackData),
        "nothing resolved, so nothing is a track"
    );
}

#[tokio::test]
async fn two_sheets_staged_together_are_not_guessed_apart() {
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    area.write(&path("disc1.cue"), MIXED_MODE_SHEET)
        .await
        .expect("stage");
    area.write(&path("disc2.cue"), MIXED_MODE_SHEET)
        .await
        .expect("stage");
    area.write(&path("disc.bin"), &vec![0_u8; RAW_SECTOR * 100])
        .await
        .expect("stage");

    let mut checkpoint = ImportCheckpoint::default();
    let outcome = pipeline
        .run(&request(import_id, "disc1.cue"), &mut checkpoint)
        .await
        .expect("import");

    let manifest = pipeline
        .manifests()
        .read(outcome.artifact_id)
        .await
        .expect("read");

    assert_eq!(manifest.topology, Topology::Unknown);
    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning.contains("descriptors were staged together")),
        "{:?}",
        outcome.warnings
    );
}

#[tokio::test]
async fn an_iso_describes_its_own_structure() {
    // The other half of topology: an image that is one flat track still says
    // how big it is and what is inside it.
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    area.write(&path("disc.iso"), &iso_image("EXAMPLE_DISC", 20))
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

    let Topology::SingleTrackBlockImage {
        block_size,
        block_count,
        volume_labels,
        filesystems,
    } = manifest.topology
    else {
        panic!(
            "expected a block image topology, got {:?}",
            manifest.topology
        );
    };
    assert_eq!(block_size, 2048);
    assert_eq!(block_count, 20);
    assert_eq!(volume_labels, vec!["EXAMPLE_DISC".to_owned()]);
    assert!(
        filesystems
            .iter()
            .any(|found| found.filesystem_type == "iso9660")
    );
}
