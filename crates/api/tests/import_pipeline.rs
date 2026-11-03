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
        physical_dump: None,
        lineage: None,
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

// --- cdrdao TOC and BIN ----------------------------------------------------------

/// A TOC in the shape cdrdao writes when it reads a disc: an absolute path it
/// had on another machine, one file for every track, a generated gap, and the
/// all-zero ISRC a drive reports for a track without one.
const READ_BACK_TOC: &[u8] = b"CD_ROM\n\
CATALOG \"1234567890123\"\n\
\n\
// Track 1\n\
TRACK MODE1\n\
NO COPY\n\
DATAFILE \"/work/readback/disc.bin\" 00:01:00 // length in bytes: 153600\n\
\n\
// Track 2\n\
TRACK AUDIO\n\
COPY\n\
NO PRE_EMPHASIS\n\
TWO_CHANNEL_AUDIO\n\
ISRC \"000000000000\"\n\
SILENCE 00:00:02\n\
FILE \"/work/readback/disc.bin\" #153600 0 00:00:25\n\
START 00:00:02\n";

#[tokio::test]
async fn a_toc_and_its_bin_import_as_one_artifact_that_knows_its_tracks() {
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    area.write(&path("disc.toc"), READ_BACK_TOC)
        .await
        .expect("stage");
    area.write(
        &path("disc.bin"),
        &vec![0_u8; 75 * SECTOR + 25 * RAW_SECTOR],
    )
    .await
    .expect("stage");

    let mut checkpoint = ImportCheckpoint::default();
    let outcome = pipeline
        .run(&request(import_id, "disc.toc"), &mut checkpoint)
        .await
        .expect("import");
    let manifest = pipeline
        .manifests()
        .read(outcome.artifact_id)
        .await
        .expect("read");

    assert_eq!(manifest.classification.format, ArtifactFormat::TocBin);
    assert_eq!(manifest.classification.media_family, Some(MediaFamily::Cd));
    assert_eq!(manifest.classification.detectors[0].name, "toc-parser");
    assert!(manifest.classification.format_confidence > 0.98);

    let descriptor = manifest
        .components
        .iter()
        .find(|component| component.logical_path.as_str() == "disc.toc")
        .expect("the descriptor is a component");
    let data = manifest
        .components
        .iter()
        .find(|component| component.logical_path.as_str() == "disc.bin")
        .expect("the data is a component");
    assert_eq!(descriptor.role, ComponentRole::Descriptor);
    assert_eq!(data.role, ComponentRole::TrackData);
    assert_eq!(
        manifest.components.len(),
        2,
        "one file, named twice, is one component"
    );

    let Topology::CdTracks {
        descriptor_component_id,
        catalog,
        tracks,
        ..
    } = manifest.topology
    else {
        panic!("expected a track topology, got {:?}", manifest.topology);
    };
    assert_eq!(descriptor_component_id, descriptor.id);
    assert_eq!(catalog.as_deref(), Some("1234567890123"));
    assert_eq!(tracks.len(), 2);

    assert_eq!(tracks[0].mode, "MODE1/2048");
    assert_eq!(tracks[0].sector_count, 75);
    assert_eq!(tracks[0].start_lba, 0);
    assert_eq!(tracks[0].flags, Vec::<TrackFlag>::new());

    assert_eq!(tracks[1].mode, "AUDIO");
    assert_eq!(tracks[1].component_id, data.id);
    assert_eq!(tracks[1].file_offset_bytes, 75 * SECTOR as u64);
    assert_eq!(tracks[1].start_lba, 77, "the generated gap is on the disc");
    assert_eq!(tracks[1].sector_count, 25);
    assert_eq!(tracks[1].pregap_sectors, 2);
    assert_eq!(
        tracks[1].isrc, None,
        "an all-zero ISRC is the absence of one"
    );
    assert_eq!(tracks[1].flags, vec![TrackFlag::DigitalCopyPermitted]);
    assert_eq!(
        tracks[1].sample_byte_order,
        Some(SampleByteOrder::BigEndian),
        "no SWAP: cdrdao's own files hold big-endian samples"
    );

    // The path cdrdao recorded was on another machine. It was matched by
    // name, and the manifest says so.
    assert!(
        checkpoint
            .warnings
            .iter()
            .any(|warning| warning.contains("/work/readback/disc.bin")),
        "{:?}",
        checkpoint.warnings
    );
}

#[tokio::test]
async fn a_toc_the_layout_cannot_record_still_imports_and_says_why() {
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    area.write(
        &path("disc.toc"),
        b"CD_DA\nTRACK AUDIO\nFILE \"a.bin\" 0 0:1:0\nFILE \"b.bin\" 0 0:1:0\n",
    )
    .await
    .expect("stage");
    area.write(&path("a.bin"), &vec![0_u8; 75 * RAW_SECTOR])
        .await
        .expect("stage");
    area.write(&path("b.bin"), &vec![0_u8; 75 * RAW_SECTOR])
        .await
        .expect("stage");

    let mut checkpoint = ImportCheckpoint::default();
    let outcome = pipeline
        .run(&request(import_id, "disc.toc"), &mut checkpoint)
        .await
        .expect("import");
    let manifest = pipeline
        .manifests()
        .read(outcome.artifact_id)
        .await
        .expect("read");

    assert_eq!(manifest.classification.format, ArtifactFormat::TocBin);
    assert_eq!(manifest.components.len(), 3, "every byte kept");
    assert!(
        !matches!(manifest.topology, Topology::CdTracks { .. }),
        "no layout claimed"
    );
    assert!(
        checkpoint
            .warnings
            .iter()
            .any(|warning| warning.contains("more than one file")),
        "{:?}",
        checkpoint.warnings
    );
}

#[tokio::test]
async fn a_cue_and_a_toc_staged_together_claim_no_layout() {
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    area.write(&path("disc.cue"), MIXED_MODE_SHEET)
        .await
        .expect("stage");
    area.write(&path("disc.toc"), READ_BACK_TOC)
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
    assert!(!matches!(manifest.topology, Topology::CdTracks { .. }));
    assert!(
        checkpoint
            .warnings
            .iter()
            .any(|warning| warning.contains("2 descriptors"))
    );
}

#[tokio::test]
async fn a_file_named_toc_that_is_not_one_imports_unclaimed() {
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    area.write(&path("notes.toc"), b"Table of contents:\n1. Intro\n")
        .await
        .expect("stage");
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
    assert_eq!(manifest.classification.format, ArtifactFormat::Iso);
}

// --- CloneCD -----------------------------------------------------------------

fn ccd_fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/../../fixtures/images/ccd/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("fixture")
}

const CCD_SECTOR: usize = 2352;

/// An IMG of `sectors` raw sectors whose first is a mode 1 data sector and
/// the rest audio.
fn clonecd_image(sectors: usize) -> Vec<u8> {
    let mut image = vec![0_u8; sectors * CCD_SECTOR];
    image[..12].copy_from_slice(&[
        0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0,
    ]);
    image[15] = 1;
    image
}

/// Stage a set of files and import it, answering the manifest and what the
/// checkpoint warned.
async fn import_set(
    files: &[(&str, Vec<u8>)],
    descriptor: &str,
) -> (tangible_domain::manifest::ArtifactManifest, Vec<String>) {
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    for (name, bytes) in files {
        area.write(&path(name), bytes).await.expect("stage");
    }
    let mut checkpoint = ImportCheckpoint::default();
    let outcome = pipeline
        .run(&request(import_id, descriptor), &mut checkpoint)
        .await
        .expect("import");
    let manifest = pipeline
        .manifests()
        .read(outcome.artifact_id)
        .await
        .expect("read");
    (manifest, checkpoint.warnings)
}

fn role_of(manifest: &tangible_domain::manifest::ArtifactManifest, name: &str) -> ComponentRole {
    manifest
        .components
        .iter()
        .find(|component| component.logical_path.as_str() == name)
        .unwrap_or_else(|| panic!("{name} is a component"))
        .role
}

#[tokio::test]
async fn a_clonecd_set_imports_as_one_artifact_with_its_tracks_and_subchannel() {
    let (manifest, warnings) = import_set(
        &[
            ("Game.ccd", ccd_fixture("mixed-mode.ccd")),
            ("Game.img", clonecd_image(750)),
            ("Game.sub", vec![0_u8; 750 * 96]),
        ],
        "Game.ccd",
    )
    .await;
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(manifest.classification.format, ArtifactFormat::CcdImgSub);
    assert_eq!(manifest.classification.media_family, Some(MediaFamily::Cd));
    assert_eq!(manifest.classification.detectors[0].name, "ccd-parser");
    assert!(manifest.classification.format_confidence > 0.98);
    assert_eq!(role_of(&manifest, "Game.ccd"), ComponentRole::Descriptor);
    assert_eq!(role_of(&manifest, "Game.img"), ComponentRole::TrackData);
    assert_eq!(role_of(&manifest, "Game.sub"), ComponentRole::Subchannel);

    let Topology::CdTracks {
        catalog,
        session_count,
        tracks,
        subchannel,
        ..
    } = &manifest.topology
    else {
        panic!("expected a track topology, got {:?}", manifest.topology);
    };
    assert_eq!(catalog.as_deref(), Some("0123456789012"));
    assert_eq!(*session_count, 1);
    let shape: Vec<_> = tracks
        .iter()
        .map(|t| {
            (
                t.number,
                t.mode.as_str(),
                t.start_lba,
                t.sector_count,
                t.pregap_sectors,
            )
        })
        .collect();
    assert_eq!(
        shape,
        [
            (1, "MODE1/2352", 0, 300, 0),
            (2, "AUDIO", 300, 300, 150),
            (3, "AUDIO", 600, 150, 0),
        ]
    );
    assert_eq!(tracks[1].isrc.as_deref(), Some("USRC17607839"));
    assert_eq!(tracks[2].flags, vec![TrackFlag::DigitalCopyPermitted]);
    let subchannel = subchannel.as_ref().expect("recorded");
    assert!(subchannel.present);
    assert_eq!(subchannel.representation.as_deref(), Some("pw96_linear"));
}

#[tokio::test]
async fn a_clonecd_image_without_its_subchannel_says_so() {
    let (manifest, warnings) = import_set(
        &[
            ("disc.CCD", ccd_fixture("data-only.ccd")),
            ("disc.img", clonecd_image(300)),
        ],
        "disc.CCD",
    )
    .await;
    assert_eq!(manifest.classification.format, ArtifactFormat::CcdImgSub);
    assert!(
        warnings.iter().any(|w| w.contains("no subchannel file")),
        "{warnings:?}"
    );
    let Topology::CdTracks {
        tracks, subchannel, ..
    } = &manifest.topology
    else {
        panic!("{:?}", manifest.topology);
    };
    assert_eq!(tracks.len(), 1);
    assert!(!subchannel.as_ref().expect("recorded").present);
    assert!(
        manifest
            .components
            .iter()
            .all(|component| component.role != ComponentRole::Subchannel)
    );
}

#[tokio::test]
async fn a_clonecd_image_that_cannot_be_laid_out_is_kept_whole_and_still_marked_as_subchannel() {
    let (manifest, warnings) = import_set(
        &[
            ("multi.ccd", ccd_fixture("multi-session.ccd")),
            ("multi.img", clonecd_image(600)),
            ("multi.sub", vec![0_u8; 600 * 96]),
        ],
        "multi.ccd",
    )
    .await;
    assert_eq!(manifest.classification.format, ArtifactFormat::CcdImgSub);
    assert!(
        matches!(manifest.topology, Topology::Unknown),
        "{:?}",
        manifest.topology
    );
    assert!(
        warnings.iter().any(|w| w.contains("2 sessions")),
        "{warnings:?}"
    );
    assert_eq!(manifest.components.len(), 3, "every file kept");
    // Still subchannel data, which is what stops it being burned as it is.
    assert_eq!(role_of(&manifest, "multi.sub"), ComponentRole::Subchannel);
}

// --- MDS/MDF -----------------------------------------------------------------

fn mds_fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/../../fixtures/images/mds/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("fixture")
}

#[tokio::test]
async fn an_mds_cd_imports_with_its_tracks_laid_out_and_pregaps_generated() {
    let (manifest, warnings) = import_set(
        &[
            ("Game.mds", mds_fixture("mixed-mode.mds")),
            ("Game.mdf", clonecd_image(450)),
        ],
        "Game.mds",
    )
    .await;
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(manifest.classification.format, ArtifactFormat::MdsMdf);
    assert_eq!(manifest.classification.media_family, Some(MediaFamily::Cd));
    assert_eq!(manifest.classification.detectors[0].name, "mds-parser");
    assert!(manifest.classification.format_confidence > 0.98);
    assert_eq!(role_of(&manifest, "Game.mds"), ComponentRole::Descriptor);
    assert_eq!(role_of(&manifest, "Game.mdf"), ComponentRole::TrackData);

    let Topology::CdTracks { tracks, .. } = &manifest.topology else {
        panic!("expected a track topology, got {:?}", manifest.topology);
    };
    let shape: Vec<_> = tracks
        .iter()
        .map(|t| {
            (
                t.number,
                t.mode.as_str(),
                t.start_lba,
                t.sector_count,
                t.pregap_sectors,
            )
        })
        .collect();
    assert_eq!(
        shape,
        [
            (1, "MODE1/2352", 0, 300, 0),
            (2, "AUDIO", 450, 100, 150),
            (3, "AUDIO", 550, 50, 0),
        ]
    );
}

#[tokio::test]
async fn an_mds_dvd_whose_data_file_is_an_iso_is_described_as_one() {
    let (manifest, warnings) = import_set(
        &[
            ("Film.mds", mds_fixture("dvd.mds")),
            ("Film.mdf", iso_image("FILM_DISC", 64)),
        ],
        "Film.mds",
    )
    .await;
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(manifest.classification.format, ArtifactFormat::MdsMdf);
    assert_eq!(manifest.classification.media_family, Some(MediaFamily::Dvd));
    assert_eq!(role_of(&manifest, "Film.mdf"), ComponentRole::TrackData);
    let Topology::SingleTrackBlockImage {
        block_size,
        block_count,
        volume_labels,
        ..
    } = &manifest.topology
    else {
        panic!(
            "expected a block image topology, got {:?}",
            manifest.topology
        );
    };
    assert_eq!((*block_size, *block_count), (2048, 64));
    assert_eq!(volume_labels, &["FILM_DISC"]);

    // What the ISO inspector says of the MDF is said of the import.
    let mut short = iso_image("FILM_DISC", 64);
    short.truncate(40 * 2048);
    let (_, warnings) = import_set(
        &[("Film.mds", mds_fixture("dvd.mds")), ("Film.mdf", short)],
        "Film.mds",
    )
    .await;
    assert!(
        warnings.iter().any(|w| w.contains("truncated")),
        "{warnings:?}"
    );
}

#[tokio::test]
async fn an_mds_image_with_interleaved_subchannel_is_imported_whole_saying_why() {
    let (manifest, warnings) = import_set(
        &[
            ("sub.mds", mds_fixture("subchannel.mds")),
            ("sub.mdf", vec![0_u8; 100 * 2448]),
        ],
        "sub.mds",
    )
    .await;
    assert_eq!(manifest.classification.format, ArtifactFormat::MdsMdf);
    assert!(
        matches!(manifest.topology, Topology::Unknown),
        "{:?}",
        manifest.topology
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("interleaves subchannel")),
        "{warnings:?}"
    );
    assert_eq!(manifest.components.len(), 2, "every file kept");
}

#[tokio::test]
async fn an_mds_descriptor_without_its_data_file_says_so() {
    let (manifest, warnings) = import_set(
        &[("lonely.mds", mds_fixture("single-data.mds"))],
        "lonely.mds",
    )
    .await;
    assert_eq!(manifest.classification.format, ArtifactFormat::MdsMdf);
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("there is no lonely.mdf")),
        "{warnings:?}"
    );
    assert!(matches!(manifest.topology, Topology::Unknown));
}

#[tokio::test]
async fn an_mds_data_file_is_found_in_the_descriptors_folder_regardless_of_case() {
    let (manifest, warnings) = import_set(
        &[
            ("Disc/Game.mds", mds_fixture("single-data.mds")),
            ("Disc/GAME.MDF", clonecd_image(300)),
        ],
        "Game.mds",
    )
    .await;
    assert!(
        warnings.iter().any(|w| w.contains("regardless of case")),
        "{warnings:?}"
    );
    assert_eq!(
        role_of(&manifest, "Disc/GAME.MDF"),
        ComponentRole::TrackData
    );
    let Topology::CdTracks { tracks, .. } = &manifest.topology else {
        panic!("{:?}", manifest.topology);
    };
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].sector_count, 300);
}

// --- CHD ---------------------------------------------------------------------

fn chd_fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/../../fixtures/images/chd/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("fixture")
}

fn evidence(manifest: &tangible_domain::manifest::ArtifactManifest) -> String {
    manifest.classification.detectors[0].evidence.join("\n")
}

#[tokio::test]
async fn a_cd_chd_imports_as_one_with_its_tracks_in_evidence() {
    let (manifest, warnings) =
        import_set(&[("Game.chd", chd_fixture("mixed-mode.chd"))], "Game.chd").await;
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(manifest.classification.format, ArtifactFormat::Chd);
    assert_eq!(manifest.classification.media_family, Some(MediaFamily::Cd));
    assert_eq!(manifest.classification.detectors[0].name, "chd-reader");
    assert!(manifest.classification.format_confidence > 0.98);
    assert_eq!(role_of(&manifest, "Game.chd"), ComponentRole::PrimaryImage);
    // Its tracks are inside compressed hunks, which a track layout cannot
    // place (OWNER_QUEUE 13).
    assert!(matches!(manifest.topology, Topology::Unknown));
    let evidence = evidence(&manifest);
    for line in [
        "CHD version 5: 752 units of 2448 bytes in hunks of 19584, compressed with cdlz, cdzl, cdfl",
        "data SHA-1 548059e51d9b6b244a81e37589037e9fc12fb3c3",
        "metadata: CHT2, CHT2, CHT2",
        "track 1: MODE1_RAW, 300 frames",
        "track 2: AUDIO, 300 frames, pregap 150 in the data",
        "track 3: AUDIO, 150 frames",
    ] {
        assert!(evidence.contains(line), "{line:?} in {evidence}");
    }
}

#[tokio::test]
async fn a_chd_says_what_disc_it_holds_or_that_it_holds_none() {
    let medium = |manifest: &tangible_domain::manifest::ArtifactManifest| {
        manifest.classification.media_family
    };
    let (dvd, warnings) = import_set(&[("Film.chd", chd_fixture("dvd.chd"))], "Film.chd").await;
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(medium(&dvd), Some(MediaFamily::Dvd));

    let (hd, warnings) = import_set(&[("drive.chd", chd_fixture("hd.chd"))], "drive.chd").await;
    assert_eq!(hd.classification.format, ArtifactFormat::Chd);
    assert_eq!(medium(&hd), None);
    assert!(
        warnings.iter().any(|w| w.contains("hard disk")),
        "{warnings:?}"
    );

    let (_, warnings) = import_set(&[("delta.chd", chd_fixture("delta.chd"))], "delta.chd").await;
    assert!(
        warnings.iter().any(|w| w.contains("without that parent")),
        "{warnings:?}"
    );
}

#[tokio::test]
async fn a_chd_this_reader_cannot_read_is_still_a_chd_and_says_why() {
    let mut old = chd_fixture("single-data.chd");
    old[12..16].copy_from_slice(&4_u32.to_be_bytes());
    let (manifest, warnings) = import_set(&[("old.chd", old)], "old.chd").await;
    assert_eq!(manifest.classification.format, ArtifactFormat::Chd);
    assert_eq!(manifest.classification.media_family, None);
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("version 4") && w.contains("chdman copy")),
        "{warnings:?}"
    );

    // Named like one and not one: imported, not claimed.
    let (manifest, _) = import_set(&[("fake.chd", vec![0_u8; 4096])], "fake.chd").await;
    assert_ne!(manifest.classification.format, ArtifactFormat::Chd);
}

// --- archives (docs/plans/archives.md) -----------------------------------------

fn archive_fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/../../fixtures/images/archives/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("fixture")
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(bytes))
}

#[tokio::test]
async fn an_archive_is_unpacked_and_its_disc_imported_with_the_archive_recorded() {
    for name in [
        "game.zip",
        "game.7z",
        "game.tar",
        "game.tar.gz",
        "game.tar.xz",
    ] {
        let bytes = archive_fixture(name);
        let (dir, pipeline) = pipeline().await;
        let import_id = ImportJobId::generate();
        let area = pipeline.open_area(import_id).await.expect("area");
        area.write(&path(name), &bytes).await.expect("stage");
        let mut checkpoint = ImportCheckpoint::default();
        let outcome = pipeline
            .run(&request(import_id, name), &mut checkpoint)
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let manifest = pipeline
            .manifests()
            .read(outcome.artifact_id)
            .await
            .expect("read");

        assert_eq!(
            manifest.classification.format,
            ArtifactFormat::CueBin,
            "{name}"
        );
        let mut paths: Vec<&str> = manifest
            .components
            .iter()
            .map(|component| component.logical_path.as_str())
            .collect();
        paths.sort_unstable();
        assert_eq!(paths, ["Game/Game.bin", "Game/Game.cue"], "{name}");
        let container = manifest
            .origin
            .container
            .as_ref()
            .expect("the archive is recorded");
        assert_eq!(container.sha256.to_hex(), sha256_hex(&bytes), "{name}");
        assert_eq!(container.length_bytes, bytes.len() as u64);
        assert_eq!(container.files, 2);
        assert_eq!(
            manifest.origin.source_filename.as_deref(),
            Some(name),
            "the name it was received under"
        );
        // Nothing of Tangible's own is left among what was imported.
        assert!(
            area.entries()
                .await
                .expect("entries")
                .iter()
                .all(|path| !path.as_str().starts_with(".tangible-archive")
                    || path.as_str() == ".tangible-archive.json"),
            "{name}"
        );
        assert!(!area.path().join(".tangible-archive").exists(), "{name}");
        drop(dir);
    }
    let formats: Vec<String> = ["game.zip", "game.7z"]
        .iter()
        .map(|name| {
            tangible_image::archive::detect(std::path::Path::new(&format!(
                "{}/../../fixtures/images/archives/{name}",
                env!("CARGO_MANIFEST_DIR")
            )))
            .unwrap()
            .unwrap()
            .as_str()
            .to_owned()
        })
        .collect();
    assert_eq!(formats, ["zip", "7z"]);
}

#[tokio::test]
async fn a_refused_archive_says_why_and_imports_nothing() {
    for (name, code) in [
        ("encrypted.7z", "ARCHIVE_ENCRYPTED"),
        ("escape.zip", "ARCHIVE_UNSAFE_ENTRY"),
        ("hardlink.tar", "ARCHIVE_UNSAFE_ENTRY"),
    ] {
        let (_dir, pipeline) = pipeline().await;
        let import_id = ImportJobId::generate();
        let area = pipeline.open_area(import_id).await.expect("area");
        area.write(&path(name), &archive_fixture(name))
            .await
            .expect("stage");
        let mut checkpoint = ImportCheckpoint::default();
        let error = pipeline
            .run(&request(import_id, name), &mut checkpoint)
            .await
            .expect_err("refused");
        let ImportError::Archive(archive) = &error else {
            panic!("{name}: {error:?}");
        };
        assert_eq!(archive.code(), code, "{name}: {archive}");
        assert_eq!(checkpoint.artifact_id, None);
        assert!(checkpoint.promoted.is_empty(), "nothing was stored");
    }
}

#[tokio::test]
async fn an_unpacking_that_stopped_part_way_starts_again() {
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    // As a run that set the archive aside and stopped while unpacking.
    area.write(&path(".tangible-archive"), &archive_fixture("game.zip"))
        .await
        .expect("stage");
    area.write(&path(".tangible-unpacking/Game/Game.bin"), b"half")
        .await
        .expect("stage");
    area.write(&path("Game/Game.cue"), b"moved before it stopped")
        .await
        .expect("stage");
    let mut checkpoint = ImportCheckpoint::default();
    let outcome = pipeline
        .run(&request(import_id, "game.zip"), &mut checkpoint)
        .await
        .expect("import");
    let manifest = pipeline
        .manifests()
        .read(outcome.artifact_id)
        .await
        .expect("read");
    assert_eq!(manifest.classification.format, ArtifactFormat::CueBin);
    let bin = manifest
        .components
        .iter()
        .find(|component| component.logical_path.as_str() == "Game/Game.bin")
        .expect("the bin");
    assert_eq!(bin.length_bytes, 2352 * 4, "unpacked whole, not the half");
    assert!(manifest.origin.container.is_some());
}

#[tokio::test]
async fn an_unpacking_that_finished_but_kept_its_archive_finishes() {
    let (_dir, pipeline) = pipeline().await;
    let import_id = ImportJobId::generate();
    let area = pipeline.open_area(import_id).await.expect("area");
    let first = ImportJobId::generate();
    // The files in place and the record written, the archive not yet gone:
    // made by unpacking in another area, then copied.
    let donor = pipeline.open_area(first).await.expect("area");
    donor
        .write(&path("game.zip"), &archive_fixture("game.zip"))
        .await
        .expect("stage");
    let mut donor_checkpoint = ImportCheckpoint::default();
    tangible_api::unpack::open_archive(&donor, &mut donor_checkpoint, 1 << 30)
        .await
        .expect("unpack");
    for name in ["Game/Game.bin", "Game/Game.cue", ".tangible-archive.json"] {
        let bytes = std::fs::read(donor.path().join(name)).unwrap();
        area.write(&path(name), &bytes).await.expect("stage");
    }
    area.write(&path(".tangible-archive"), &archive_fixture("game.zip"))
        .await
        .expect("stage");

    let mut checkpoint = ImportCheckpoint::default();
    let outcome = pipeline
        .run(&request(import_id, "game.zip"), &mut checkpoint)
        .await
        .expect("import");
    assert!(!area.path().join(".tangible-archive").exists());
    let manifest = pipeline
        .manifests()
        .read(outcome.artifact_id)
        .await
        .expect("read");
    assert_eq!(
        manifest.origin.container.expect("recorded").sha256.to_hex(),
        sha256_hex(&archive_fixture("game.zip"))
    );
    assert_eq!(
        manifest.components.len(),
        2,
        "the record is not a component"
    );
}

#[tokio::test]
async fn a_file_that_is_not_an_archive_is_imported_as_it_is() {
    let (manifest, _) = import_set(&[("disc.iso", iso_image("DISC", 64))], "disc.iso").await;
    assert!(manifest.origin.container.is_none());
    assert_eq!(manifest.classification.format, ArtifactFormat::Iso);
}
