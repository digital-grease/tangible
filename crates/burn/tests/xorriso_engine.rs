// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The xorriso engine, running the real tool.
//!
//! The target is a file rather than a drive, through xorriso's own `stdio:` mechanism,
//! so a complete burn happens here: the real binary, the real arguments, the
//! real progress output, and a real read-back comparison of what landed. The
//! only thing missing is the laser.
//!
//! That is worth more than it sounds. The dangerous parts of a burn engine are
//! the arguments it passes, what it makes of the tool's output, and whether it
//! believes a write it should not. All three run here.
//!
//! These tests need xorriso, so they are gated. Set `TANGIBLE_XORRISO_TESTS=1`
//! to run them, and `TANGIBLE_XORRISO_BIN` if the binary is not on `PATH`.
//! They are skipped rather than failed when the tool is absent, because a
//! developer without it should still get a green suite; CI installs it and
//! sets the flag.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use tangible_burn::xorriso::XorrisoEngine;
use tangible_burn::{BurnEngine, BurnPlan, CollectingSink, DriveRef, PlannedInput, WriteMode};
use tangible_domain::{BurnAttemptId, DriveId, Sha256Digest, WorkerId};
use tempfile::TempDir;

/// Whether these tests were asked for, and with which binary.
fn binary() -> Option<String> {
    if std::env::var("TANGIBLE_XORRISO_TESTS").unwrap_or_default() != "1" {
        eprintln!("skipping: set TANGIBLE_XORRISO_TESTS=1 to run the xorriso engine tests");
        return None;
    }
    Some(std::env::var("TANGIBLE_XORRISO_BIN").unwrap_or_else(|_| "xorriso".to_owned()))
}

fn digest_of(bytes: &[u8]) -> Sha256Digest {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Sha256Digest::from_bytes(hasher.finalize().into())
}

/// A payload big enough that the tool reports progress while writing it.
fn payload(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from(i % 251).unwrap_or(0))
        .collect()
}

fn plan(staged: &Path, target: &Path, bytes: &[u8]) -> BurnPlan {
    BurnPlan {
        attempt_id: BurnAttemptId::generate(),
        drive: DriveRef {
            worker_id: WorkerId::generate(),
            drive_id: DriveId::generate(),
            // Not a device node, so the engine addresses it as a file. Same
            // arguments, same tool, no hardware.
            device_alias: target.display().to_string(),
        },
        inputs: vec![PlannedInput {
            staged_path: staged.to_path_buf(),
            sha256: digest_of(bytes),
            length_bytes: bytes.len() as u64,
        }],
        tracks: Vec::new(),
        catalog: None,
        mode: WriteMode::DataDiscAtOnce,
        accepted_profiles: vec![],
        speed: None,
        finalize: false,
        eject_on_success: false,
        total_bytes: bytes.len() as u64,
    }
}

struct Fixture {
    dir: TempDir,
    staged: PathBuf,
    target: PathBuf,
    bytes: Vec<u8>,
}

fn fixture(len: usize) -> Fixture {
    // Under the directory the wrapper mounts, when one is configured, so the
    // paths mean the same thing to the tool as to the test.
    let dir = match std::env::var("TANGIBLE_XORRISO_WORKDIR") {
        Ok(root) => TempDir::new_in(root).expect("temp dir"),
        Err(_) => TempDir::new().expect("temp dir"),
    };
    let bytes = payload(len);
    let staged = dir.path().join("source.iso");
    std::fs::write(&staged, &bytes).expect("stage the image");
    let target = dir.path().join("medium.img");

    Fixture {
        dir,
        staged,
        target,
        bytes,
    }
}

#[tokio::test]
async fn the_tool_identifies_itself() {
    let Some(binary) = binary() else { return };
    let mut engine = XorrisoEngine::at(&binary);

    let version = engine.probe_version().await.expect("a version");
    assert!(!version.version.is_empty());
    assert_eq!(engine.version(), version.version);
    assert_eq!(engine.name(), "xorriso");
}

#[tokio::test]
async fn something_that_is_not_xorriso_is_refused() {
    let Some(_) = binary() else { return };
    // A worker whose image installed the wrong thing should hear about it at
    // startup rather than at the first burn.
    let mut engine = XorrisoEngine::at("/bin/echo");
    assert!(engine.probe_version().await.is_err());
}

#[tokio::test]
async fn a_real_write_lands_the_bytes_and_verifies() {
    let Some(binary) = binary() else { return };
    let fixture = fixture(8 * 1024 * 1024);
    let engine = XorrisoEngine::at(&binary);
    let plan = plan(&fixture.staged, &fixture.target, &fixture.bytes);
    let sink = CollectingSink::new();

    let report = engine.write(&plan, &sink).await.expect("the write ran");
    assert!(
        report.engine_reported_success,
        "diagnostics: {:?}",
        report.diagnostics
    );
    assert_eq!(report.engine, "xorriso");

    // The medium holds the image. This is the whole claim a burn makes, and
    // it is checked here against the real bytes the real tool wrote.
    let written = std::fs::read(&fixture.target).expect("read the medium");
    assert!(
        written.starts_with(&fixture.bytes),
        "the medium does not start with the image: {} vs {} bytes",
        written.len(),
        fixture.bytes.len()
    );

    let verified = engine
        .verify(&plan, &sink)
        .await
        .expect("the read-back ran");
    assert!(
        verified.matched,
        "the read-back must match what was written"
    );
    assert_eq!(verified.bytes_compared, fixture.bytes.len() as u64);
    assert!(
        !verified.limitations.is_empty(),
        "a verification result must state its limits"
    );
}

#[tokio::test]
async fn a_read_back_in_uneven_chunks_matches() {
    // Chunk boundaries are where offset arithmetic goes wrong. 1000 blocks
    // does not divide the image, so the last chunk is a partial one.
    let Some(binary) = binary() else { return };
    let fixture = fixture(8 * 1024 * 1024 + 4096);
    let engine = XorrisoEngine::at(&binary).with_read_back_chunk(1000);
    let plan = plan(&fixture.staged, &fixture.target, &fixture.bytes);
    let sink = CollectingSink::new();
    engine.write(&plan, &sink).await.expect("the write ran");

    let verified = engine
        .verify(&plan, &sink)
        .await
        .expect("the read-back ran");
    assert!(verified.matched, "{verified:?}");
    assert_eq!(verified.bytes_compared, fixture.bytes.len() as u64);
    let chunks = sink
        .events()
        .iter()
        .filter(|event| event.code == "VERIFY_PROGRESS")
        .count();
    assert_eq!(chunks, 5, "4098 blocks in chunks of 1000");
}

#[tokio::test]
async fn a_wrong_byte_in_a_later_chunk_is_found() {
    let Some(binary) = binary() else { return };
    let fixture = fixture(8 * 1024 * 1024);
    let engine = XorrisoEngine::at(&binary).with_read_back_chunk(1000);
    let plan = plan(&fixture.staged, &fixture.target, &fixture.bytes);
    let sink = CollectingSink::new();
    engine.write(&plan, &sink).await.expect("the write ran");

    // Block 3500, in the fourth chunk.
    let mut written = std::fs::read(&fixture.target).expect("read the medium");
    written[3500 * 2048 + 7] ^= 0xff;
    std::fs::write(&fixture.target, &written).expect("corrupt the medium");

    let verified = engine
        .verify(&plan, &sink)
        .await
        .expect("the read-back ran");
    assert!(!verified.matched, "{verified:?}");
}

#[tokio::test]
async fn a_read_back_leaves_nothing_behind() {
    // Up to a quarter of a gigabyte per verification, in a worker's state.
    let Some(binary) = binary() else { return };
    let fixture = fixture(1024 * 1024);
    let scratch = TempDir::new_in(fixture.dir.path()).expect("scratch");
    let engine = XorrisoEngine::at(&binary).with_scratch_dir(scratch.path());
    let plan = plan(&fixture.staged, &fixture.target, &fixture.bytes);
    engine
        .write(&plan, &CollectingSink::new())
        .await
        .expect("the write ran");
    engine
        .verify(&plan, &CollectingSink::new())
        .await
        .expect("the read-back ran");

    let left: Vec<_> = std::fs::read_dir(scratch.path()).expect("list").collect();
    assert!(left.is_empty(), "{left:?}");
}

#[tokio::test]
async fn a_write_reports_progress_while_it_runs() {
    // A twenty-minute write with no progress is a progress bar that only
    // moves at the end.
    let Some(binary) = binary() else { return };
    let fixture = fixture(64 * 1024 * 1024);
    let engine = XorrisoEngine::at(&binary);
    let sink = CollectingSink::new();

    engine
        .write(
            &plan(&fixture.staged, &fixture.target, &fixture.bytes),
            &sink,
        )
        .await
        .expect("the write ran");

    let events = sink.events();
    assert!(
        events.iter().any(|event| event.code == "WRITE_PROGRESS"),
        "no progress was reported: {events:?}"
    );
    assert!(
        events.iter().any(|event| event.code == "WRITE_COMPLETED"),
        "the completion was not reported: {events:?}"
    );
    for event in &events {
        if let Some(progress) = event.progress {
            assert!(
                (0.0..=1.0).contains(&progress),
                "{progress} is not a fraction"
            );
        }
    }
}

#[tokio::test]
async fn a_disc_that_does_not_match_fails_verification() {
    // The case the whole verification step exists for: the write succeeded
    // and the medium does not hold what was intended.
    let Some(binary) = binary() else { return };
    let fixture = fixture(4 * 1024 * 1024);
    let engine = XorrisoEngine::at(&binary);
    let plan = plan(&fixture.staged, &fixture.target, &fixture.bytes);
    let sink = CollectingSink::new();

    engine.write(&plan, &sink).await.expect("the write ran");

    // Corrupt one byte, as a bad disc would.
    let mut written = std::fs::read(&fixture.target).expect("read the medium");
    written[1024] ^= 0xff;
    std::fs::write(&fixture.target, &written).expect("corrupt the medium");

    let verified = engine
        .verify(&plan, &sink)
        .await
        .expect("the read-back ran");
    assert!(
        !verified.matched,
        "a single wrong byte must fail the read-back"
    );
}

#[tokio::test]
async fn preflight_refuses_a_staged_file_that_changed() {
    // "A burn starts only after hash verification" as a fact rather than an
    // intention: bytes staged an hour ago may not be the bytes on disk now.
    let Some(binary) = binary() else { return };
    let fixture = fixture(1024 * 1024);
    let engine = XorrisoEngine::at(&binary);
    let plan = plan(&fixture.staged, &fixture.target, &fixture.bytes);

    // Something else rewrote the staged file after it was planned.
    std::fs::write(&fixture.staged, payload(1024 * 1024 + 1)).expect("rewrite");

    let report = engine.preflight(&plan).await.expect("preflight ran");
    assert!(
        report.failures.iter().any(|failure| matches!(
            failure,
            tangible_burn::PreflightFailure::InputDigestMismatch { .. }
        )),
        "{:?}",
        report.failures
    );
}

#[tokio::test]
async fn preflight_refuses_a_staged_file_that_vanished() {
    let Some(binary) = binary() else { return };
    let fixture = fixture(1024);
    let engine = XorrisoEngine::at(&binary);
    let plan = plan(&fixture.staged, &fixture.target, &fixture.bytes);
    std::fs::remove_file(&fixture.staged).expect("remove the staged file");

    let report = engine.preflight(&plan).await.expect("preflight ran");
    assert!(
        report.failures.iter().any(|failure| matches!(
            failure,
            tangible_burn::PreflightFailure::InputMissing { .. }
        )),
        "{:?}",
        report.failures
    );
}

#[tokio::test]
async fn trouble_after_a_completed_write_is_recorded_without_being_called_a_failure() {
    // Not hypothetical: xorriso 1.5.6 in a container writes the data and then
    // falls over on shutdown, exiting non-zero. Calling that a failed write
    // would record a ruined disc for a disc holding the right bytes; calling
    // it a clean success would hide that the tool had trouble. It is reported
    // as a completed write carrying diagnostics, and the read-back settles it.
    let Some(binary) = binary() else { return };
    let fixture = fixture(2 * 1024 * 1024);
    let engine = XorrisoEngine::at(&binary);
    let plan = plan(&fixture.staged, &fixture.target, &fixture.bytes);

    let report = engine
        .write(&plan, &CollectingSink::new())
        .await
        .expect("the write ran");

    if report.diagnostics.is_empty() {
        // A clean run on a tool that does not have the quirk. Then success
        // must be unqualified.
        assert!(report.engine_reported_success);
    } else {
        // Trouble was reported. Whether the write itself failed is decided by
        // what the medium holds, so the engine must not have guessed.
        let verified = engine
            .verify(&plan, &CollectingSink::new())
            .await
            .expect("the read-back ran");
        assert_eq!(
            report.engine_reported_success, verified.matched,
            "the engine's account and the disc disagree: {report:?}"
        );
    }
}

#[test]
fn xorriso_will_not_pretend_to_write_a_table_of_contents() {
    // There are two engines because this one writes prepared images and
    // does not write per-track modes, audio and exact pregaps. Saying so is
    // what stops a mixed-mode CD being flattened into one data track by an
    // engine that would happily accept it.
    let engine = XorrisoEngine::new();

    assert!(engine.supports_mode(WriteMode::DataDiscAtOnce));
    assert!(engine.supports_mode(WriteMode::DataTrackAtOnce));
    assert!(!engine.supports_mode(WriteMode::TocDiscAtOnce));
}
