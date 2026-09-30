// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The cdrdao engine, running the real tool.
//!
//! cdrdao cannot write to a file the way xorriso can, so no burn happens here.
//! What does happen is the part of preflight that decides whether a burn is
//! allowed to start: the engine writes the table of contents for a plan, runs
//! the real `cdrdao show-toc` over it and real files, and compares cdrdao's
//! reading with the plan track by track. Every layout shape the writer
//! produces is checked by the tool that will act on it, and so are the
//! objections cdrdao prints and then exits 0 from.
//!
//! The drive-facing commands run too, against a device that is not there, so
//! the engine's handling of the tool's real refusal is tested rather than
//! assumed.
//!
//! These need cdrdao, so they are gated. Set `TANGIBLE_CDRDAO_TESTS=1` to run
//! them, and `TANGIBLE_CDRDAO_BIN` if the binary is not on `PATH`. A wrapper
//! that runs cdrdao in a container works, provided it mounts the temporary
//! directory at the same path. Skipped rather than failed when absent, so a
//! developer without the tool still gets a green suite.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::PathBuf;

use sha2::{Digest as _, Sha256};
use tangible_burn::cdrdao::CdrdaoEngine;
use tangible_burn::plan::{PlannedIndex, PlannedTrack, PreflightFailure};
use tangible_burn::{
    BurnEngine, BurnPlan, CollectingSink, DriveRef, EngineError, PlannedInput, WriteMode,
};
use tangible_domain::cd::{SampleByteOrder, TrackFlag};
use tangible_domain::{BurnAttemptId, DriveId, Sha256Digest, WorkerId};
use tempfile::TempDir;

const RAW: u64 = 2352;

/// A device node that does not exist, in every environment these run in.
const NO_DRIVE: &str = "/dev/tangible-no-such-drive";

/// Whether these tests were asked for, and with which binary.
fn binary() -> Option<String> {
    if std::env::var("TANGIBLE_CDRDAO_TESTS").unwrap_or_default() != "1" {
        eprintln!("skipping: set TANGIBLE_CDRDAO_TESTS=1 to run the cdrdao engine tests");
        return None;
    }
    Some(std::env::var("TANGIBLE_CDRDAO_BIN").unwrap_or_else(|_| "cdrdao".to_owned()))
}

/// Files on disk and an engine that keeps its tables of contents beside them.
struct Fixture {
    dir: TempDir,
    engine: CdrdaoEngine,
}

impl Fixture {
    fn new(program: &str) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let engine = CdrdaoEngine::at(program, dir.path().join("toc"));
        Self { dir, engine }
    }

    /// Stage a file of `sectors` raw sectors and describe it as an input.
    fn input(&self, name: &str, sectors: u64) -> PlannedInput {
        let bytes: Vec<u8> = (0..sectors * RAW)
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        let path = self.dir.path().join(name);
        std::fs::write(&path, &bytes).expect("stage");
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        PlannedInput {
            staged_path: path,
            sha256: Sha256Digest::from_bytes(hasher.finalize().into()),
            length_bytes: bytes.len() as u64,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

fn track(number: u32, mode: &str, sectors: u64) -> PlannedTrack {
    PlannedTrack {
        number,
        session: 1,
        mode: mode.to_owned(),
        input: 0,
        file_offset_bytes: 0,
        start_lba: 0,
        sector_count: sectors,
        pregap_sectors: 0,
        indexes: vec![PlannedIndex {
            number: 1,
            relative_lba: 0,
        }],
        isrc: None,
        sample_byte_order: Some(SampleByteOrder::LittleEndian),
        flags: Vec::new(),
    }
}

fn plan(inputs: Vec<PlannedInput>, tracks: Vec<PlannedTrack>) -> BurnPlan {
    let total_bytes = inputs.iter().map(|input| input.length_bytes).sum();
    BurnPlan {
        attempt_id: BurnAttemptId::generate(),
        drive: DriveRef {
            worker_id: WorkerId::generate(),
            drive_id: DriveId::generate(),
            device_alias: NO_DRIVE.to_owned(),
        },
        inputs,
        tracks,
        catalog: None,
        mode: WriteMode::TocDiscAtOnce,
        accepted_profiles: vec!["CD-R".to_owned()],
        speed: None,
        finalize: true,
        eject_on_success: true,
        total_bytes,
    }
}

/// The shape almost every game CD comes in: data, then audio after a gap the
/// burner generates.
fn mixed_mode(fixture: &Fixture) -> BurnPlan {
    let mut audio = track(2, "AUDIO", 3000);
    audio.file_offset_bytes = RAW * 1000;
    audio.pregap_sectors = 150;
    plan(
        vec![fixture.input("disc.bin", 4000)],
        vec![track(1, "MODE2/2352", 1000), audio],
    )
}

fn assert_accepted(failures: &[PreflightFailure]) {
    assert!(
        failures.is_empty(),
        "cdrdao read the table of contents differently from the plan: {failures:#?}"
    );
}

// --- the table of contents, read back by the tool that will write it ---------

#[tokio::test]
async fn a_mixed_mode_disc_is_read_as_the_plan_means_it() {
    let Some(program) = binary() else { return };
    let fixture = Fixture::new(&program);

    let failures = fixture
        .engine
        .check_table_of_contents(&mixed_mode(&fixture))
        .await
        .expect("the check runs");
    assert_accepted(&failures);
}

#[tokio::test]
async fn a_gap_in_the_file_with_flags_codes_and_an_index_is_read_as_meant() {
    // Everything the writer can say about one track, at once.
    let Some(program) = binary() else { return };
    let fixture = Fixture::new(&program);

    let mut data = track(1, "MODE1/2352", 1000);
    data.flags = vec![TrackFlag::DigitalCopyPermitted];
    let mut audio = track(2, "AUDIO", 3000);
    audio.file_offset_bytes = RAW * 1000;
    audio.pregap_sectors = 150;
    audio.indexes = vec![
        PlannedIndex {
            number: 0,
            relative_lba: 0,
        },
        PlannedIndex {
            number: 1,
            relative_lba: 150,
        },
        PlannedIndex {
            number: 2,
            relative_lba: 900,
        },
    ];
    audio.isrc = Some("USRC17607839".to_owned());
    audio.flags = vec![
        TrackFlag::DigitalCopyPermitted,
        TrackFlag::PreEmphasis,
        TrackFlag::FourChannel,
    ];
    let mut with_everything = plan(vec![fixture.input("disc.bin", 4000)], vec![data, audio]);
    with_everything.catalog = Some("1234567890123".to_owned());

    let failures = fixture
        .engine
        .check_table_of_contents(&with_everything)
        .await
        .expect("the check runs");
    assert_accepted(&failures);
}

#[tokio::test]
async fn one_file_per_track_in_either_byte_order_is_read_as_meant() {
    let Some(program) = binary() else { return };
    let fixture = Fixture::new(&program);

    let first = track(1, "AUDIO", 600);
    let mut second = track(2, "AUDIO", 450);
    second.input = 1;
    second.pregap_sectors = 150;
    second.sample_byte_order = Some(SampleByteOrder::BigEndian);
    let audio_disc = plan(
        vec![
            fixture.input("track01.bin", 600),
            fixture.input("track02.bin", 450),
        ],
        vec![first, second],
    );

    let failures = fixture
        .engine
        .check_table_of_contents(&audio_disc)
        .await
        .expect("the check runs");
    assert_accepted(&failures);
}

// --- what cdrdao objects to and exits 0 from ------------------------------------

#[tokio::test]
async fn a_file_that_has_gone_is_an_objection_even_though_cdrdao_exits_zero() {
    let Some(program) = binary() else { return };
    let fixture = Fixture::new(&program);
    let disc = mixed_mode(&fixture);
    std::fs::remove_file(fixture.path("disc.bin")).expect("remove");

    let failures = fixture
        .engine
        .check_table_of_contents(&disc)
        .await
        .expect("the check runs");
    assert!(
        failures.iter().any(|failure| matches!(
            failure,
            PreflightFailure::TableOfContentsRefused { cause } if cause.contains("disc.bin")
        )),
        "{failures:#?}"
    );
}

#[tokio::test]
async fn a_file_too_short_for_its_tracks_is_an_objection() {
    let Some(program) = binary() else { return };
    let fixture = Fixture::new(&program);
    let mut disc = mixed_mode(&fixture);
    // The layout says four thousand sectors; the file on disk has two.
    disc.inputs[0] = fixture.input("disc.bin", 2000);

    let failures = fixture
        .engine
        .check_table_of_contents(&disc)
        .await
        .expect("the check runs");
    assert!(
        matches!(
            failures.first(),
            Some(PreflightFailure::TableOfContentsRefused { .. })
        ),
        "{failures:#?}"
    );
}

#[tokio::test]
async fn a_track_under_four_seconds_is_an_objection_from_the_tool_too() {
    // Preflight's own layout check catches this first. This is the tool saying
    // the same thing, which is what makes the layout check more than a guess.
    let Some(program) = binary() else { return };
    let fixture = Fixture::new(&program);
    let short = plan(
        vec![fixture.input("short.bin", 299)],
        vec![track(1, "AUDIO", 299)],
    );

    let failures = fixture
        .engine
        .check_table_of_contents(&short)
        .await
        .expect("the check runs");
    assert!(
        failures.iter().any(|failure| matches!(
            failure,
            PreflightFailure::TableOfContentsRefused { cause } if cause.contains("4 seconds")
        )),
        "{failures:#?}"
    );
}

// --- the tool and the drive -------------------------------------------------------

#[tokio::test]
async fn the_tool_names_its_version() {
    let Some(program) = binary() else { return };
    let mut engine = CdrdaoEngine::at(program, std::env::temp_dir());
    let version = engine.probe_version().await.expect("a version");
    assert!(!version.is_empty());
    assert_eq!(engine.version(), version);
}

#[tokio::test]
async fn a_drive_that_is_not_there_is_unavailable_rather_than_empty() {
    // A device mapping fault and an empty tray have different remedies, and
    // the operator is told which.
    let Some(program) = binary() else { return };
    let fixture = Fixture::new(&program);
    let drive = mixed_mode(&fixture).drive;

    for result in [
        fixture.engine.inspect_medium(&drive).await.map(|_| ()),
        fixture.engine.probe_drive(&drive).await.map(|_| ()),
        fixture.engine.eject(&drive).await,
    ] {
        assert!(
            matches!(result, Err(EngineError::DriveUnavailable { .. })),
            "{result:?}"
        );
    }
}

#[tokio::test]
async fn a_write_to_a_drive_that_is_not_there_is_reported_as_not_written() {
    // The real write arguments reaching the real tool. It refuses before
    // writing anything, and the report says so rather than succeeding.
    let Some(program) = binary() else { return };
    let fixture = Fixture::new(&program);
    let disc = mixed_mode(&fixture);
    let sink = CollectingSink::new();

    let report = fixture
        .engine
        .write(&disc, &sink)
        .await
        .expect("the tool ran");
    assert!(!report.engine_reported_success);
    assert_eq!(report.bytes_written, 0);
    assert!(!report.finalized);
    assert!(
        report
            .diagnostics
            .iter()
            .any(|line| line.contains(NO_DRIVE)),
        "{:?}",
        report.diagnostics
    );
    assert!(
        fixture
            .path("toc")
            .join(format!("{}.toc", disc.attempt_id))
            .is_file(),
        "the table of contents the drive was told to write is kept"
    );
}

#[tokio::test]
async fn preflight_refuses_on_the_table_of_contents_before_asking_for_a_disc() {
    // With no drive at all, a table of contents cdrdao objects to is still
    // reported: that check never needed a disc.
    let Some(program) = binary() else { return };
    let fixture = Fixture::new(&program);
    let disc = mixed_mode(&fixture);
    std::fs::remove_file(fixture.path("disc.bin")).expect("remove");

    let report = fixture.engine.preflight(&disc).await.expect("preflight");
    assert!(!report.passed());
    assert!(report.medium.is_none(), "no disc was asked about");
    assert!(
        report
            .failures
            .iter()
            .all(|failure| matches!(failure, PreflightFailure::TableOfContentsRefused { .. })),
        "{:#?}",
        report.failures
    );
}
