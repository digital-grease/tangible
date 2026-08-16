// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Worker-side protocol state.
//!
//! Most of these assert something the worker must *not* do. The expensive
//! mistakes here are all of that shape: writing a second disc, dropping the
//! only record of an attempt, or stopping a laser mid-burn.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::time::Duration;

use serde_json::json;
use tangible_burn::{
    EventBuffer, EventError, Lease, RecoveryDirective, RecoveryRecord, RecoveryStore, WorkerStage,
    plan_for,
};
use tangible_domain::{ArtifactId, BurnAttemptId, BurnJobId, Sha256Digest};
use tempfile::TempDir;
use time::OffsetDateTime;

fn now() -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH
}

fn buffer() -> EventBuffer {
    EventBuffer::new(BurnAttemptId::generate())
}

fn record_progress(buffer: &mut EventBuffer, fraction: f32) -> u64 {
    buffer
        .record(
            "progress",
            WorkerStage::Writing,
            "WRITE_PROGRESS",
            Some(fraction),
            json!({}),
            now(),
        )
        .expect("record")
}

fn record_stage(buffer: &mut EventBuffer, stage: WorkerStage) -> u64 {
    buffer
        .record(
            "stage_changed",
            stage,
            "STAGE_CHANGED",
            None,
            json!({}),
            now(),
        )
        .expect("record")
}

// --- sequencing ---------------------------------------------------------------

#[test]
fn sequences_start_at_one_and_increase_without_gaps() {
    // The server deduplicates by sequence, so a gap would look like a lost
    // event forever and a repeat would collide.
    let mut buffer = buffer();
    assert_eq!(record_stage(&mut buffer, WorkerStage::Staging), 1);
    assert_eq!(record_progress(&mut buffer, 0.5), 2);
    assert_eq!(record_stage(&mut buffer, WorkerStage::Writing), 3);
    assert_eq!(buffer.next_sequence(), 4);
}

#[test]
fn a_batch_does_not_consume_what_it_returns() {
    // A batch sent and lost must be sent again, so batching cannot remove.
    let mut buffer = buffer();
    record_stage(&mut buffer, WorkerStage::Staging);
    record_stage(&mut buffer, WorkerStage::Writing);

    let first = buffer.batch(10);
    let second = buffer.batch(10);
    assert_eq!(first, second);
    assert_eq!(buffer.pending_count(), 2);
}

#[test]
fn acknowledgement_prunes_only_what_the_server_holds() {
    let mut buffer = buffer();
    for _ in 0..5 {
        record_progress(&mut buffer, 0.1);
    }

    buffer.acknowledge(3);
    assert_eq!(buffer.acknowledged_through(), 3);
    assert_eq!(buffer.pending_count(), 2);
    assert_eq!(
        buffer.batch(10).first().map(|event| event.sequence),
        Some(4),
        "the next batch resumes after the acknowledgement"
    );
}

#[test]
fn a_stale_acknowledgement_is_ignored_rather_than_treated_as_an_error() {
    // A delayed response overtaking a newer one is normal networking, not a
    // fault, and must not un-prune events the server already has.
    let mut buffer = buffer();
    for _ in 0..5 {
        record_progress(&mut buffer, 0.1);
    }

    buffer.acknowledge(4);
    buffer.acknowledge(2);

    assert_eq!(buffer.acknowledged_through(), 4);
    assert_eq!(buffer.pending_count(), 1);
}

#[test]
fn a_fully_acknowledged_buffer_is_drained() {
    let mut buffer = buffer();
    let last = record_stage(&mut buffer, WorkerStage::Completing);
    assert!(!buffer.is_drained());
    buffer.acknowledge(last);
    assert!(buffer.is_drained());
}

#[test]
fn a_resumed_buffer_never_reuses_a_sequence_the_server_holds() {
    // After a restart, numbering continues past what was acknowledged.
    // Restarting at 1 would collide with events the server already persisted.
    let attempt = BurnAttemptId::generate();
    let buffer = EventBuffer::resumed(attempt, 42, 128);
    assert_eq!(buffer.next_sequence(), 43);
    assert_eq!(buffer.acknowledged_through(), 42);
    assert!(buffer.is_drained());
}

// --- network loss --------------------------------------------------------------

#[test]
fn events_accumulate_while_disconnected_and_replay_in_order() {
    // The whole point of the buffer: a worker mid-write keeps working and
    // keeps recording, then resends everything when the server returns.
    let mut buffer = buffer();
    record_stage(&mut buffer, WorkerStage::Writing);
    for step in 1..=20 {
        #[allow(clippy::cast_precision_loss)]
        record_progress(&mut buffer, step as f32 / 20.0);
    }
    record_stage(&mut buffer, WorkerStage::Verifying);

    assert_eq!(buffer.pending_count(), 22);
    let replay = buffer.batch(usize::MAX);
    let sequences: Vec<u64> = replay.iter().map(|event| event.sequence).collect();
    assert_eq!(sequences, (1..=22).collect::<Vec<_>>());
}

#[test]
fn a_full_buffer_sheds_progress_before_state_changes() {
    // A worker cut off for hours must not exhaust memory, and a percentage
    // reading is worth less than the record that the write started.
    let mut buffer = EventBuffer::with_capacity(BurnAttemptId::generate(), 4);
    record_stage(&mut buffer, WorkerStage::Writing);
    for _ in 0..3 {
        record_progress(&mut buffer, 0.5);
    }
    assert_eq!(buffer.pending_count(), 4);

    record_progress(&mut buffer, 0.9);

    let kinds: Vec<String> = buffer
        .batch(10)
        .into_iter()
        .map(|event| event.event_type)
        .collect();
    assert!(
        kinds.contains(&"stage_changed".to_owned()),
        "the state change must survive: {kinds:?}"
    );
    assert_eq!(buffer.pending_count(), 4);
}

#[test]
fn a_buffer_of_only_state_changes_refuses_rather_than_dropping_one() {
    // Nothing here is safe to discard, so the worker is told instead.
    let mut buffer = EventBuffer::with_capacity(BurnAttemptId::generate(), 2);
    record_stage(&mut buffer, WorkerStage::Writing);
    record_stage(&mut buffer, WorkerStage::Verifying);

    let error = buffer
        .record(
            "stage_changed",
            WorkerStage::Completing,
            "STAGE_CHANGED",
            None,
            json!({}),
            now(),
        )
        .expect_err("must refuse");
    assert_eq!(error, EventError::BufferFull { capacity: 2 });
}

#[test]
fn progress_is_clamped_when_recorded() {
    let mut buffer = buffer();
    record_progress(&mut buffer, 7.5);
    assert_eq!(buffer.batch(1)[0].progress, Some(1.0));
}

// --- leases ---------------------------------------------------------------------

fn lease(seconds: i64) -> Lease {
    Lease {
        attempt_id: BurnAttemptId::generate(),
        expires_at: now() + time::Duration::seconds(seconds),
        duration: Duration::from_secs(u64::try_from(seconds).unwrap_or(0)),
    }
}

#[test]
fn renewal_is_attempted_once_half_the_lease_has_elapsed() {
    // Half remaining is the margin for a slow or briefly unreachable server.
    // Waiting longer would make one dropped request enough to lose the lease.
    let lease = lease(90);
    assert!(!lease.should_renew(now()));
    assert!(!lease.should_renew(now() + time::Duration::seconds(44)));
    assert!(lease.should_renew(now() + time::Duration::seconds(45)));
    assert!(lease.should_renew(now() + time::Duration::seconds(80)));
}

#[test]
fn an_expired_lease_blocks_new_work() {
    let lease = lease(60);
    assert!(lease.may_claim_new_work(now()));
    assert!(!lease.may_claim_new_work(now() + time::Duration::seconds(60)));
    assert!(lease.is_expired(now() + time::Duration::seconds(61)));
}

#[test]
fn losing_a_lease_never_stops_a_write_in_progress() {
    // The rule that matters most here. A lease is a liveness signal for the
    // server; the laser is already burning. Stopping guarantees a ruined disc,
    // whereas finishing and reconciling afterwards may not.
    let expired = lease(60);
    let long_after = now() + time::Duration::seconds(100_000);

    assert!(expired.is_expired(long_after));
    assert!(!expired.may_claim_new_work(long_after));
    assert!(
        expired.write_may_continue(),
        "an expired lease must not interrupt an active write"
    );
}

// --- stages ---------------------------------------------------------------------

#[test]
fn the_media_consumption_boundary_is_the_write() {
    for before in [
        WorkerStage::Claimed,
        WorkerStage::Staging,
        WorkerStage::WaitingForMedia,
        WorkerStage::Preflighting,
    ] {
        assert!(!before.may_have_consumed_media(), "{before:?}");
        assert!(before.is_safely_cancellable(), "{before:?}");
    }
    for after in [
        WorkerStage::Writing,
        WorkerStage::Finalizing,
        WorkerStage::Verifying,
        WorkerStage::Completing,
    ] {
        assert!(after.may_have_consumed_media(), "{after:?}");
        assert!(
            !after.is_safely_cancellable(),
            "{after:?} consumed media, so cancelling is not clean"
        );
    }
}

// --- local recovery state ---------------------------------------------------------

fn record(stage: WorkerStage) -> RecoveryRecord {
    RecoveryRecord {
        attempt_id: BurnAttemptId::generate(),
        burn_job_id: BurnJobId::generate(),
        stage,
        drive_identity: "pioneer-bdr-212:serialhash".to_owned(),
        artifact_id: ArtifactId::generate(),
        manifest_sha256: Sha256Digest::from_bytes([0xab; 32]),
        last_event_sequence: 42,
        engine: "fake".to_owned(),
        updated_at: now(),
    }
}

#[tokio::test]
async fn a_recovery_record_round_trips() {
    let dir = TempDir::new().expect("temp dir");
    let store = RecoveryStore::new(dir.path().join("state").join("attempt.json"));
    let written = record(WorkerStage::Writing);

    store.write(&written).await.expect("write");
    assert_eq!(store.read().await.expect("read"), Some(written));
}

#[tokio::test]
async fn no_record_means_no_attempt_was_in_flight() {
    let dir = TempDir::new().expect("temp dir");
    let store = RecoveryStore::new(dir.path().join("absent.json"));
    assert_eq!(store.read().await.expect("read"), None);
}

#[tokio::test]
async fn rewriting_replaces_the_record_without_leaving_a_temporary() {
    // Atomic replace: a crash mid-write leaves the old record or the new one,
    // never a half-written one that would read as corrupt and escalate an
    // otherwise recoverable attempt.
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("attempt.json");
    let store = RecoveryStore::new(&path);

    let mut first = record(WorkerStage::Staging);
    store.write(&first).await.expect("first");
    first.stage = WorkerStage::Writing;
    first.last_event_sequence = 99;
    store.write(&first).await.expect("second");

    let read = store.read().await.expect("read").expect("present");
    assert_eq!(read.stage, WorkerStage::Writing);
    assert_eq!(read.last_event_sequence, 99);

    let strays: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read dir")
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "part"))
        .collect();
    assert!(strays.is_empty(), "left a temporary file behind");
}

#[tokio::test]
async fn a_corrupt_record_is_reported_rather_than_read_as_absent() {
    // The safe reading of a corrupt record is "an attempt was in flight and
    // its details are unknown", not "nothing happened".
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("attempt.json");
    std::fs::write(&path, b"{ this is not json").expect("write");

    let store = RecoveryStore::new(&path);
    assert!(store.read().await.is_err());
}

#[tokio::test]
async fn clearing_is_idempotent() {
    let dir = TempDir::new().expect("temp dir");
    let store = RecoveryStore::new(dir.path().join("attempt.json"));
    store
        .write(&record(WorkerStage::Completing))
        .await
        .expect("write");

    store.clear().await.expect("clear");
    store.clear().await.expect("clearing again must be safe");
    assert_eq!(store.read().await.expect("read"), None);
}

// --- the rule that protects physical media -----------------------------------------

#[test]
fn no_recovery_directive_ever_permits_writing() {
    // The single most important property in this module. A worker that
    // restarts and finds its previous process gone cannot know whether the
    // laser already ran. Writing again destroys a second disc and produces a
    // physical copy record that may match nothing.
    for directive in [
        RecoveryDirective::ResumeReporting,
        RecoveryDirective::ResumeVerification,
        RecoveryDirective::MarkNeedsAttention,
        RecoveryDirective::DiscardPrewriteState,
        RecoveryDirective::WorkerRevoked,
    ] {
        assert!(
            !plan_for(directive).may_write,
            "{directive:?} must never permit a write"
        );
    }
}

#[test]
fn only_a_pre_write_discard_releases_the_worker_for_new_work() {
    assert!(plan_for(RecoveryDirective::DiscardPrewriteState).may_accept_new_work);
    for held in [
        RecoveryDirective::ResumeReporting,
        RecoveryDirective::ResumeVerification,
        RecoveryDirective::MarkNeedsAttention,
        RecoveryDirective::WorkerRevoked,
    ] {
        assert!(
            !plan_for(held).may_accept_new_work,
            "{held:?} must not release the worker"
        );
    }
}

#[test]
fn state_needing_human_attention_is_kept_not_discarded() {
    // The record is the evidence a human will need.
    let plan = plan_for(RecoveryDirective::MarkNeedsAttention);
    assert!(!plan.discard_local_state);
}

#[test]
fn only_pre_write_state_is_discarded() {
    assert!(plan_for(RecoveryDirective::DiscardPrewriteState).discard_local_state);
    for kept in [
        RecoveryDirective::ResumeReporting,
        RecoveryDirective::ResumeVerification,
        RecoveryDirective::WorkerRevoked,
    ] {
        assert!(!plan_for(kept).discard_local_state, "{kept:?}");
    }
}

#[test]
fn directives_round_trip_through_serde() {
    for directive in [
        RecoveryDirective::ResumeReporting,
        RecoveryDirective::ResumeVerification,
        RecoveryDirective::MarkNeedsAttention,
        RecoveryDirective::DiscardPrewriteState,
        RecoveryDirective::WorkerRevoked,
    ] {
        let json = serde_json::to_string(&directive).expect("serialize");
        assert_eq!(
            serde_json::from_str::<RecoveryDirective>(&json).expect("deserialize"),
            directive
        );
    }
    assert_eq!(
        serde_json::to_string(&RecoveryDirective::ResumeReporting).expect("serialize"),
        "\"resume_reporting\""
    );
}
