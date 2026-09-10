// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The hardware-free burn engine.
//!
//! These exercise the burn safety rules in the only place they can be
//! exercised without destroying discs. The failure cases matter more than the
//! happy path: a write that dies partway, a drive that reports success while
//! producing a bad disc, and an erase requested without confirmation are the
//! situations where getting it wrong costs physical media.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::PathBuf;

use sha2::{Digest as _, Sha256};
use tangible_burn::{
    BlankRequest, BurnEngine, BurnPlan, CancelToken, CollectingSink, DriveRef, EngineError,
    FakeBehaviour, FakeEngine, PlannedInput, PreflightFailure, WriteMode,
};
use tangible_domain::{BurnAttemptId, DriveId, Sha256Digest, WorkerId};
use tempfile::TempDir;

fn digest_of(bytes: &[u8]) -> Sha256Digest {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Sha256Digest::from_bytes(hasher.finalize().into())
}

fn drive() -> DriveRef {
    DriveRef {
        worker_id: WorkerId::generate(),
        drive_id: DriveId::generate(),
        device_alias: "/dev/disc-block".to_owned(),
    }
}

struct Fixture {
    dir: TempDir,
    drive: DriveRef,
    payload: Vec<u8>,
    staged: PathBuf,
}

fn fixture(payload_len: usize) -> Fixture {
    let dir = TempDir::new().expect("temp dir");
    let payload: Vec<u8> = (0..payload_len)
        .map(|i| u8::try_from(i % 251).unwrap_or(0))
        .collect();
    let staged = dir.path().join("disc.iso");
    std::fs::write(&staged, &payload).expect("stage");
    Fixture {
        dir,
        drive: drive(),
        payload,
        staged,
    }
}

fn plan_for(fixture: &Fixture) -> BurnPlan {
    BurnPlan {
        attempt_id: BurnAttemptId::generate(),
        drive: fixture.drive.clone(),
        inputs: vec![PlannedInput {
            staged_path: fixture.staged.clone(),
            sha256: digest_of(&fixture.payload),
            length_bytes: fixture.payload.len() as u64,
        }],
        tracks: Vec::new(),
        mode: WriteMode::DataDiscAtOnce,
        accepted_profiles: vec!["CD-R".to_owned()],
        speed: None,
        finalize: true,
        eject_on_success: true,
        total_bytes: fixture.payload.len() as u64,
    }
}

fn engine(fixture: &Fixture, behaviour: FakeBehaviour) -> FakeEngine {
    FakeEngine::new(fixture.dir.path().join("media")).with_behaviour(behaviour)
}

// --- the happy path ----------------------------------------------------------

#[tokio::test]
async fn a_clean_burn_writes_and_verifies() {
    let fixture = fixture(200_000);
    let engine = engine(&fixture, FakeBehaviour::default());
    let plan = plan_for(&fixture);
    let sink = CollectingSink::new();

    let preflight = engine.preflight(&plan).await.expect("preflight");
    assert!(preflight.passed(), "failures: {:?}", preflight.failures);

    let write = engine.write(&plan, &sink).await.expect("write");
    assert!(write.engine_reported_success);
    assert_eq!(write.bytes_written, fixture.payload.len() as u64);

    let verify = engine.verify(&plan, &sink).await.expect("verify");
    assert!(verify.matched);
    assert_eq!(verify.bytes_compared, fixture.payload.len() as u64);
}

#[tokio::test]
async fn verification_is_a_real_read_back_not_a_recorded_verdict() {
    // The simulated medium is a file, so a wrong byte is detected exactly as
    // it would be on a real drive.
    let fixture = fixture(8192);
    let engine = engine(&fixture, FakeBehaviour::default());
    let plan = plan_for(&fixture);

    engine
        .write(&plan, &CollectingSink::new())
        .await
        .expect("write");

    let medium = engine.medium_path(&fixture.drive);
    let written = std::fs::read(&medium).expect("read medium");
    assert_eq!(
        written, fixture.payload,
        "the medium holds the source bytes"
    );
}

#[tokio::test]
async fn progress_is_reported_while_writing() {
    let fixture = fixture(200_000);
    let engine = engine(&fixture, FakeBehaviour::default());
    let sink = CollectingSink::new();

    engine
        .write(&plan_for(&fixture), &sink)
        .await
        .expect("write");

    let progress: Vec<_> = sink
        .events()
        .into_iter()
        .filter(|event| event.code == "WRITE_PROGRESS")
        .collect();
    assert!(progress.len() > 1, "expected incremental progress");
    for event in &progress {
        let fraction = event.progress.expect("a progress figure");
        assert!((0.0..=1.0).contains(&fraction), "out of range: {fraction}");
    }
}

// --- the rules that protect physical media -------------------------------------

#[tokio::test]
async fn a_write_is_refused_when_preflight_would_fail() {
    // The engine re-checks rather than trusting the caller. A burn destroys
    // media, so the last thing able to stop a mistake should also try to.
    let fixture = fixture(1024);
    let engine = engine(
        &fixture,
        FakeBehaviour {
            medium_read_only: true,
            ..FakeBehaviour::default()
        },
    );

    let error = engine
        .write(&plan_for(&fixture), &CollectingSink::new())
        .await
        .expect_err("must refuse");
    assert!(matches!(error, EngineError::PreflightNotPassed));
    assert!(
        !engine.medium_path(&fixture.drive).exists(),
        "a refused write must not touch the medium"
    );
}

#[tokio::test]
async fn a_staged_input_altered_after_planning_is_caught_before_writing() {
    // The check that makes "hash verified before write" a fact rather than an
    // intention.
    let fixture = fixture(4096);
    let engine = engine(&fixture, FakeBehaviour::default());
    let plan = plan_for(&fixture);

    std::fs::write(&fixture.staged, b"different bytes entirely").expect("tamper");

    let preflight = engine.preflight(&plan).await.expect("preflight");
    assert!(!preflight.passed());
    assert!(
        preflight
            .failures
            .iter()
            .any(|failure| matches!(failure, PreflightFailure::InputDigestMismatch { .. })),
        "failures: {:?}",
        preflight.failures
    );

    assert!(matches!(
        engine.write(&plan, &CollectingSink::new()).await,
        Err(EngineError::PreflightNotPassed)
    ));
}

#[tokio::test]
async fn a_missing_staged_input_is_caught() {
    let fixture = fixture(1024);
    let engine = engine(&fixture, FakeBehaviour::default());
    let plan = plan_for(&fixture);
    std::fs::remove_file(&fixture.staged).expect("remove");

    let preflight = engine.preflight(&plan).await.expect("preflight");
    assert!(
        preflight
            .failures
            .iter()
            .any(|f| matches!(f, PreflightFailure::InputMissing { .. }))
    );
}

#[tokio::test]
async fn a_plan_larger_than_the_medium_is_refused_rather_than_overburned() {
    // One 2048-byte block against a payload several times that size.
    let fixture = fixture(10_000);
    let engine = engine(
        &fixture,
        FakeBehaviour {
            medium_blocks: Some(1),
            ..FakeBehaviour::default()
        },
    );

    let preflight = engine
        .preflight(&plan_for(&fixture))
        .await
        .expect("preflight");
    assert!(
        preflight
            .failures
            .iter()
            .any(|f| matches!(f, PreflightFailure::InsufficientCapacity { .. })),
        "overburning is not offered, so this must simply fail"
    );
}

#[tokio::test]
async fn a_non_blank_medium_is_refused() {
    let fixture = fixture(1024);
    let engine = engine(
        &fixture,
        FakeBehaviour {
            medium_sessions: 1,
            ..FakeBehaviour::default()
        },
    );

    let preflight = engine
        .preflight(&plan_for(&fixture))
        .await
        .expect("preflight");
    assert!(
        preflight
            .failures
            .iter()
            .any(|f| matches!(f, PreflightFailure::MediumNotBlank { .. }))
    );
}

#[tokio::test]
async fn a_medium_of_the_wrong_profile_is_refused() {
    let fixture = fixture(1024);
    let engine = engine(
        &fixture,
        FakeBehaviour {
            medium_profile: Some("DVD-R".to_owned()),
            ..FakeBehaviour::default()
        },
    );

    let preflight = engine
        .preflight(&plan_for(&fixture))
        .await
        .expect("preflight");
    assert!(
        preflight
            .failures
            .iter()
            .any(|f| matches!(f, PreflightFailure::ProfileNotAccepted { .. }))
    );
}

#[tokio::test]
async fn no_medium_is_reported_rather_than_erroring() {
    // "Insert a disc" is information an operator acts on, not an exception.
    let fixture = fixture(1024);
    let engine = engine(
        &fixture,
        FakeBehaviour {
            no_medium: true,
            ..FakeBehaviour::default()
        },
    );

    let preflight = engine
        .preflight(&plan_for(&fixture))
        .await
        .expect("preflight");
    assert_eq!(preflight.failures, vec![PreflightFailure::NoMedium]);
}

// --- a written disc is not a verified disc --------------------------------------

#[tokio::test]
async fn a_successful_write_can_still_fail_verification() {
    // The whole reason write success and verification are separate states: a
    // drive can report success and produce a bad disc.
    let fixture = fixture(8192);
    let engine = engine(
        &fixture,
        FakeBehaviour {
            corrupt_after_write: true,
            ..FakeBehaviour::default()
        },
    );
    let plan = plan_for(&fixture);
    let sink = CollectingSink::new();

    let write = engine.write(&plan, &sink).await.expect("write");
    assert!(
        write.engine_reported_success,
        "the engine reports success, as a real one would"
    );

    let verify = engine.verify(&plan, &sink).await.expect("verify");
    assert!(
        !verify.matched,
        "read-back must catch what the exit code missed"
    );
    assert_eq!(verify.first_mismatch_offset, Some(0));
}

#[tokio::test]
async fn a_verification_result_always_states_its_limits() {
    // A result that does not say what it did not check invites being read as
    // a stronger guarantee than it is.
    let fixture = fixture(4096);
    let engine = engine(&fixture, FakeBehaviour::default());
    let plan = plan_for(&fixture);

    engine
        .write(&plan, &CollectingSink::new())
        .await
        .expect("write");
    let verify = engine
        .verify(&plan, &CollectingSink::new())
        .await
        .expect("verify");

    assert!(!verify.limitations.is_empty());
    assert!(
        verify
            .limitations
            .iter()
            .any(|limit| limit.contains("player") || limit.contains("console")),
        "must say it proves nothing about playback: {:?}",
        verify.limitations
    );
}

// --- failures and cancellation ---------------------------------------------------

#[tokio::test]
async fn a_write_that_fails_partway_reports_how_far_it_got() {
    let fixture = fixture(100_000);
    let engine = engine(
        &fixture,
        FakeBehaviour {
            fail_write_after_bytes: Some(40_000),
            ..FakeBehaviour::default()
        },
    );

    let error = engine
        .write(&plan_for(&fixture), &CollectingSink::new())
        .await
        .expect_err("must fail");
    match error {
        EngineError::WriteFailed { bytes_written, .. } => {
            assert_eq!(bytes_written, 40_000);
        }
        other => panic!("expected WriteFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn a_failed_write_leaves_a_partially_written_medium() {
    // A real failure ruins the disc, and the simulation must too, so recovery
    // code is exercised against the situation that actually occurs.
    let fixture = fixture(100_000);
    let engine = engine(
        &fixture,
        FakeBehaviour {
            fail_write_after_bytes: Some(40_000),
            ..FakeBehaviour::default()
        },
    );

    let _ = engine
        .write(&plan_for(&fixture), &CollectingSink::new())
        .await;

    let medium = engine.medium_path(&fixture.drive);
    assert!(medium.exists(), "the disc was consumed and must exist");
    let length = std::fs::metadata(&medium).expect("metadata").len();
    assert!(
        length < fixture.payload.len() as u64,
        "the medium must be short, not complete"
    );
}

#[tokio::test]
async fn cancelling_before_a_write_stops_it() {
    let fixture = fixture(1024);
    let cancel = CancelToken::new();
    let engine = engine(&fixture, FakeBehaviour::default()).with_cancel(cancel.clone());
    cancel.cancel();

    assert!(matches!(
        engine
            .write(&plan_for(&fixture), &CollectingSink::new())
            .await,
        Err(EngineError::Cancelled)
    ));
}

#[tokio::test]
async fn an_unavailable_drive_is_reported_distinctly() {
    let fixture = fixture(1024);
    let engine = engine(
        &fixture,
        FakeBehaviour {
            drive_unavailable: true,
            ..FakeBehaviour::default()
        },
    );

    assert!(matches!(
        engine.probe_drive(&fixture.drive).await,
        Err(EngineError::DriveUnavailable { .. })
    ));
    assert!(matches!(
        engine.inspect_medium(&fixture.drive).await,
        Err(EngineError::DriveUnavailable { .. })
    ));
    assert!(matches!(
        engine.eject(&fixture.drive).await,
        Err(EngineError::DriveUnavailable { .. })
    ));
}

// --- erasing ---------------------------------------------------------------------

#[tokio::test]
async fn erasing_without_confirmation_is_refused() {
    // Destroying whatever is in the drive must never be a side effect.
    let fixture = fixture(1024);
    let engine = engine(&fixture, FakeBehaviour::default());

    let error = engine
        .blank(
            &BlankRequest {
                drive: fixture.drive.clone(),
                full: false,
                confirmed_destructive: false,
            },
            &CollectingSink::new(),
        )
        .await
        .expect_err("must refuse");
    assert!(matches!(error, EngineError::DestructiveNotConfirmed));
}

#[tokio::test]
async fn a_confirmed_erase_clears_the_medium() {
    let fixture = fixture(4096);
    let engine = engine(&fixture, FakeBehaviour::default());
    engine
        .write(&plan_for(&fixture), &CollectingSink::new())
        .await
        .expect("write");
    assert!(engine.medium_path(&fixture.drive).exists());

    let report = engine
        .blank(
            &BlankRequest {
                drive: fixture.drive.clone(),
                full: true,
                confirmed_destructive: true,
            },
            &CollectingSink::new(),
        )
        .await
        .expect("blank");

    assert!(report.erased);
    assert!(!engine.medium_path(&fixture.drive).exists());
}

// --- capabilities ------------------------------------------------------------------

#[tokio::test]
async fn capabilities_are_reported_as_observations() {
    let fixture = fixture(1024);
    let engine = engine(&fixture, FakeBehaviour::default());

    let capabilities = engine.probe_drive(&fixture.drive).await.expect("probe");
    assert!(capabilities.write_profiles.contains(&"CD-R".to_owned()));
    assert!(
        !capabilities.engine_evidence.is_empty(),
        "a capability claim should record what reported it"
    );
}

#[tokio::test]
async fn the_engine_identifies_itself_for_the_attempt_record() {
    let fixture = fixture(1024);
    let engine = engine(&fixture, FakeBehaviour::default());
    assert_eq!(engine.name(), "fake");
    assert!(!engine.version().is_empty());
}
