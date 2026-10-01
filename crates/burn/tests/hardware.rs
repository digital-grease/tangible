// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Tests against a real optical drive.
//!
//! Everything here goes through [`CombinedEngine`], xorriso and cdrdao behind
//! the routing a worker running `auto` uses, so a pass covers the wiring as
//! well as the engines.
//!
//! Two gates, because the tests are not equally harmless:
//!
//! - `TANGIBLE_HARDWARE_TESTS=1` runs the ones that only ask the drive
//!   questions. They change nothing.
//! - `TANGIBLE_HARDWARE_WRITE=1`, as well, runs the ones that burn. Each burns
//!   one disc and a CD-R cannot be burned twice, so they refuse to write unless
//!   preflight finds a blank disc, and they are best run one at a time by name.
//!
//! `TANGIBLE_HARDWARE_DEVICE` names the drive (default `/dev/sr0`).
//! `TANGIBLE_XORRISO_BIN` and `TANGIBLE_CDRDAO_BIN` point at the tools when
//! they are not on `PATH`; a wrapper that runs them in a container works if it
//! passes the drive through, mounts the temporary directory at the same path,
//! and runs as a user in the drive's group.
//!
//! ```bash
//! TANGIBLE_HARDWARE_TESTS=1 cargo test -p tangible-burn --test hardware \
//!   -- --test-threads=1 --nocapture
//! TANGIBLE_HARDWARE_TESTS=1 TANGIBLE_HARDWARE_WRITE=1 \
//!   cargo test -p tangible-burn --test hardware an_iso -- --nocapture
//! ```

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use tangible_burn::plan::{PlannedIndex, PlannedTrack, TrackCheck, TrackOutcome};
use tangible_burn::{
    BurnEngine, BurnEvent, BurnPlan, CdrdaoEngine, CombinedEngine, DriveRef, EngineError,
    EventSink, PlannedInput, WriteMode, XorrisoEngine,
};
use tangible_domain::cd::SampleByteOrder;
use tangible_domain::{BurnAttemptId, DriveId, Sha256Digest, WorkerId};
use tempfile::TempDir;

/// Prints what the engine reports, so a run on a drive can be read live.
struct Print;

impl EventSink for Print {
    fn emit(&self, event: BurnEvent) {
        println!(
            "  [{}] {} {}",
            event.stage,
            event.code,
            event
                .progress
                .map(|fraction| format!("{:.0}% ", fraction * 100.0))
                .unwrap_or_default()
                + &event.message
        );
    }
}

/// The drive, when hardware tests were asked for.
fn device() -> Option<String> {
    if std::env::var("TANGIBLE_HARDWARE_TESTS").unwrap_or_default() != "1" {
        eprintln!("skipping: set TANGIBLE_HARDWARE_TESTS=1 to run tests against a drive");
        return None;
    }
    Some(std::env::var("TANGIBLE_HARDWARE_DEVICE").unwrap_or_else(|_| "/dev/sr0".to_owned()))
}

/// The drive, when burning a disc was asked for as well.
fn device_for_writing() -> Option<String> {
    let device = device()?;
    if std::env::var("TANGIBLE_HARDWARE_WRITE").unwrap_or_default() != "1" {
        eprintln!("skipping: set TANGIBLE_HARDWARE_WRITE=1 as well to burn a disc");
        return None;
    }
    Some(device)
}

fn tool(variable: &str, default: &str) -> String {
    std::env::var(variable).unwrap_or_else(|_| default.to_owned())
}

/// The engines a worker running `auto` would have, with their working files
/// under `dir`.
async fn engine(dir: &Path) -> CombinedEngine<XorrisoEngine, CdrdaoEngine> {
    let scratch = dir.join("readback");
    std::fs::create_dir_all(&scratch).expect("scratch");
    let mut xorriso =
        XorrisoEngine::at(tool("TANGIBLE_XORRISO_BIN", "xorriso")).with_scratch_dir(scratch);
    let mut cdrdao = CdrdaoEngine::at(tool("TANGIBLE_CDRDAO_BIN", "cdrdao"), dir.join("toc"));
    xorriso.probe_version().await.expect("xorriso answers");
    cdrdao.probe_version().await.expect("cdrdao answers");
    CombinedEngine::new(xorriso, cdrdao)
}

fn drive(device: &str) -> DriveRef {
    DriveRef {
        worker_id: WorkerId::generate(),
        drive_id: DriveId::generate(),
        device_alias: device.to_owned(),
    }
}

fn input(path: PathBuf) -> PlannedInput {
    let bytes = std::fs::read(&path).expect("read input");
    PlannedInput {
        sha256: Sha256Digest::from_bytes(Sha256::digest(&bytes).into()),
        length_bytes: bytes.len() as u64,
        staged_path: path,
    }
}

/// A small ISO 9660 image, built by the xorriso under test.
async fn iso(dir: &Path, megabytes: usize) -> PathBuf {
    let tree = dir.join("tree");
    std::fs::create_dir_all(&tree).expect("tree");
    std::fs::write(
        tree.join("README.TXT"),
        "Tangible hardware test disc. Generated data; nothing from a library.\n",
    )
    .expect("readme");
    let payload: Vec<u8> = (0..megabytes * 1024 * 1024)
        .map(|i| u8::try_from((i * 7 + i / 4093) % 251).unwrap_or(0))
        .collect();
    std::fs::write(tree.join("PAYLOAD.BIN"), payload).expect("payload");

    let image = dir.join("test.iso");
    let status = tokio::process::Command::new(tool("TANGIBLE_XORRISO_BIN", "xorriso"))
        .args(["-as", "mkisofs", "-V", "TANGIBLE_HW_TEST", "-o"])
        .arg(&image)
        .arg(&tree)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .expect("run xorriso");
    assert!(status.success(), "building the test image failed");
    image
}

/// 440 Hz, little-endian, as a CUE sheet's BINARY file stores it.
fn tone(path: &Path, seconds: u64) {
    let frames = seconds * 75 * 588;
    let mut bytes = Vec::with_capacity(usize::try_from(frames * 4).unwrap_or(0));
    for i in 0..frames {
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        let sample =
            (8000.0 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 44100.0).sin()) as i16;
        bytes.extend_from_slice(&sample.to_le_bytes());
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    std::fs::write(path, bytes).expect("tone");
}

fn image_plan(device: &str, image: PlannedInput) -> BurnPlan {
    BurnPlan {
        attempt_id: BurnAttemptId::generate(),
        drive: drive(device),
        total_bytes: image.length_bytes,
        inputs: vec![image],
        tracks: Vec::new(),
        catalog: None,
        mode: WriteMode::DataDiscAtOnce,
        accepted_profiles: Vec::new(),
        speed: None,
        finalize: true,
        eject_on_success: false,
    }
}

/// Preflight, insisting it passes before anything is written.
async fn preflight_or_refuse(engine: &impl BurnEngine, plan: &BurnPlan) {
    let report = engine.preflight(plan).await.expect("preflight ran");
    println!("preflight: {report:#?}");
    assert!(
        report.passed(),
        "preflight did not pass, so nothing was written: {:?}",
        report.failures
    );
}

// --- questions only ----------------------------------------------------------

#[tokio::test]
async fn both_engines_can_reach_the_drive() {
    let Some(device) = device() else { return };
    let dir = TempDir::new().expect("temp dir");
    let engine = engine(dir.path()).await;

    let capabilities = engine
        .probe_drive(&drive(&device))
        .await
        .expect("the drive answers");
    println!("{capabilities:#?}");
    for name in ["xorriso", "cdrdao"] {
        let evidence = capabilities
            .engine_evidence
            .get(name)
            .unwrap_or_else(|| panic!("no word from {name}"));
        assert!(
            !evidence.contains("probe failed") && !evidence.contains("was not among"),
            "{name}: {evidence}"
        );
    }
}

#[tokio::test]
async fn the_medium_in_the_drive_is_described_or_absent() {
    let Some(device) = device() else { return };
    let dir = TempDir::new().expect("temp dir");
    let engine = engine(dir.path()).await;

    match engine.inspect_medium(&drive(&device)).await {
        Ok(medium) => {
            println!("{medium:#?}");
            assert_ne!(medium.profile, "unknown", "a disc the drive cannot read");
        }
        Err(EngineError::NoMedium { .. }) => println!("the tray is empty"),
        Err(error) => panic!("the drive did not answer: {error}"),
    }
}

// --- burning: each of these uses up a disc ------------------------------------

#[tokio::test]
async fn an_iso_is_burned_and_reads_back_identical() {
    // xorriso, through the same routing a worker uses: preflight, write, then
    // the read-back over SCSI that the kernel's stale block-device size made
    // necessary.
    let Some(device) = device_for_writing() else {
        return;
    };
    let dir = TempDir::new().expect("temp dir");
    let engine = engine(dir.path()).await;
    let plan = image_plan(&device, input(iso(dir.path(), 4).await));

    preflight_or_refuse(&engine, &plan).await;
    let report = engine.write(&plan, &Print).await.expect("the write ran");
    println!("write: {report:#?}");
    assert!(report.engine_reported_success, "{:?}", report.diagnostics);
    assert_eq!(report.engine, "xorriso", "the image went to xorriso");

    let verified = engine
        .verify(&plan, &Print)
        .await
        .expect("the read-back ran");
    println!("verify: {verified:#?}");
    assert!(verified.matched, "the disc does not hold the image");
    assert_eq!(verified.bytes_compared, plan.total_bytes);
}

#[tokio::test]
async fn a_mixed_mode_disc_is_burned_and_its_data_track_reads_back() {
    // cdrdao, through the same routing: a data track, a generated gap, and a
    // little-endian tone. Verified track by track: the data track compared
    // byte for byte, the audio track measured, the result partial. The data
    // track, which is an ISO, is then read back with xorriso as well.
    let Some(device) = device_for_writing() else {
        return;
    };
    let dir = TempDir::new().expect("temp dir");
    let engine = engine(dir.path()).await;

    let image = input(iso(dir.path(), 2).await);
    let audio_path = dir.path().join("tone.bin");
    tone(&audio_path, 30);
    let audio = input(audio_path);
    let data_sectors = image.length_bytes / 2048;
    let track = |number: u32, mode: &str, input: usize, sectors: u64, pregap: u64| PlannedTrack {
        number,
        session: 1,
        mode: mode.to_owned(),
        input,
        file_offset_bytes: 0,
        start_lba: 0,
        sector_count: sectors,
        pregap_sectors: pregap,
        indexes: vec![PlannedIndex {
            number: 1,
            relative_lba: 0,
        }],
        isrc: None,
        sample_byte_order: Some(SampleByteOrder::LittleEndian),
        flags: Vec::new(),
    };
    let plan = BurnPlan {
        tracks: vec![
            track(1, "MODE1/2048", 0, data_sectors, 0),
            track(2, "AUDIO", 1, audio.length_bytes / 2352, 150),
        ],
        total_bytes: image.length_bytes + audio.length_bytes,
        inputs: vec![image.clone(), audio],
        mode: WriteMode::TocDiscAtOnce,
        ..image_plan(&device, image.clone())
    };

    preflight_or_refuse(&engine, &plan).await;
    let report = engine.write(&plan, &Print).await.expect("the write ran");
    println!("write: {report:#?}");
    assert!(report.engine_reported_success, "{:?}", report.diagnostics);
    assert_eq!(report.engine, "cdrdao", "the layout went to cdrdao");

    let verified = engine
        .verify(&plan, &Print)
        .await
        .expect("the read-back ran");
    println!("verify: {verified:#?}");
    assert!(verified.matched, "{:?}", verified.tracks);
    assert!(verified.is_partial(), "audio is measured, not compared");
    assert_eq!(verified.method, "track_hash_compare");
    assert_eq!(
        verified
            .tracks
            .iter()
            .map(|track| (track.check, track.outcome))
            .collect::<Vec<_>>(),
        vec![
            (TrackCheck::ByteCompare, TrackOutcome::Match),
            (TrackCheck::LengthAndReadable, TrackOutcome::Match),
        ]
    );

    let data_track = image_plan(&device, image);
    let verified = engine
        .verify(&data_track, &Print)
        .await
        .expect("the read-back ran");
    assert!(verified.matched, "the data track does not hold the image");
}
