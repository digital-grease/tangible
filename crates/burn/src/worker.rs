// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Worker-side protocol state.
//!
//! Three things a burn worker must get right, none of which involve HTTP and
//! all of which are testable without a server:
//!
//! - **Event sequencing.** Every event carries a monotonic sequence scoped to
//!   the attempt, and the server acknowledges the highest it has persisted.
//!   Unacknowledged events are retained and resent, which makes submission
//!   idempotent: a replayed batch collides on sequence rather than duplicating.
//! - **Lease behaviour during a write.** A lease is how the server knows a
//!   worker is alive. It is deliberately *not* permission to keep writing.
//!   Losing one mid-write must never stop the laser, because stopping ruins the
//!   disc and the worker is the only thing that can finish it.
//! - **Local recovery state.** Written before every irreversible transition and
//!   cleared only after the server acknowledges completion, so a worker that
//!   restarts knows what it was doing and can say so rather than guess.
//!
//! The rule underneath all of it: a worker never starts a second write. Not on
//! restart, not on lease loss, not because the previous process is gone. The
//! cost of a wrong retry is a destroyed disc and, worse, a physical copy record
//! that does not correspond to any disc that exists.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tangible_domain::{ArtifactId, BurnAttemptId, BurnJobId, Sha256Digest};
use time::OffsetDateTime;

/// The stage a worker believes it is in.
///
/// Recorded locally so a restarted worker can report what it was doing rather
/// than infer it from whatever the drive happens to look like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStage {
    /// Claimed, nothing done.
    Claimed,
    /// Downloading and verifying components.
    Staging,
    /// Waiting for the operator to insert a disc.
    WaitingForMedia,
    /// Checking drive and medium against the plan.
    Preflighting,
    /// Writing. The irreversible one.
    Writing,
    /// Closing the disc.
    Finalizing,
    /// Reading back and comparing.
    Verifying,
    /// Done, awaiting server acknowledgement.
    Completing,
}

impl WorkerStage {
    /// Whether reaching this stage may already have consumed media.
    ///
    /// The dividing line for recovery. Before it, local state can simply be
    /// discarded; at or after it, a disc physically exists and something must
    /// account for it.
    #[must_use]
    pub const fn may_have_consumed_media(&self) -> bool {
        matches!(
            self,
            Self::Writing | Self::Finalizing | Self::Verifying | Self::Completing
        )
    }

    /// Whether cancellation at this stage is clean.
    #[must_use]
    pub const fn is_safely_cancellable(&self) -> bool {
        matches!(
            self,
            Self::Claimed | Self::Staging | Self::WaitingForMedia | Self::Preflighting
        )
    }
}

/// One event a worker reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerEvent {
    /// Monotonic within the attempt, starting at 1.
    pub sequence: u64,
    /// When the worker observed it.
    #[serde(with = "time::serde::rfc3339")]
    pub worker_time: OffsetDateTime,
    /// Event type, e.g. `stage_changed`.
    pub event_type: String,
    /// Stage it relates to.
    pub stage: WorkerStage,
    /// Stable machine-readable code.
    pub code: String,
    /// Progress from 0 to 1, when the stage reports it.
    pub progress: Option<f32>,
    /// Bounded structured detail.
    pub data: serde_json::Value,
}

/// Why an event could not be recorded.
///
/// There is deliberately no non-monotonic variant. The buffer assigns
/// sequences itself rather than accepting one from a caller, so a reused or
/// backwards sequence cannot be expressed, which is a stronger guarantee than
/// an error would be, since the server relies on that ordering to deduplicate.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EventError {
    /// The buffer is full.
    ///
    /// Bounded on purpose: a worker cut off for hours must not exhaust memory,
    /// and progress events are the ones worth dropping when it comes to that.
    #[error("event buffer is full at {capacity} events")]
    BufferFull {
        /// The cap.
        capacity: usize,
    },
}

/// Holds events until the server acknowledges them.
///
/// The buffer is what makes network loss survivable. A worker mid-write keeps
/// working and keeps recording; when connectivity returns it resends
/// everything unacknowledged, and the server deduplicates by sequence.
#[derive(Debug)]
pub struct EventBuffer {
    attempt_id: BurnAttemptId,
    pending: VecDeque<WorkerEvent>,
    next_sequence: u64,
    acknowledged_through: u64,
    capacity: usize,
}

/// Default number of unacknowledged events retained.
pub const DEFAULT_EVENT_CAPACITY: usize = 4096;

impl EventBuffer {
    /// A buffer for one attempt.
    #[must_use]
    pub fn new(attempt_id: BurnAttemptId) -> Self {
        Self::with_capacity(attempt_id, DEFAULT_EVENT_CAPACITY)
    }

    /// A buffer with an explicit cap.
    #[must_use]
    pub fn with_capacity(attempt_id: BurnAttemptId, capacity: usize) -> Self {
        Self {
            attempt_id,
            pending: VecDeque::new(),
            next_sequence: 1,
            acknowledged_through: 0,
            capacity: capacity.max(1),
        }
    }

    /// The attempt these events belong to.
    #[must_use]
    pub const fn attempt_id(&self) -> BurnAttemptId {
        self.attempt_id
    }

    /// The sequence the next recorded event will carry.
    #[must_use]
    pub const fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    /// The highest sequence the server has persisted.
    #[must_use]
    pub const fn acknowledged_through(&self) -> u64 {
        self.acknowledged_through
    }

    /// How many events are waiting to be accepted.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Record an event, assigning it the next sequence.
    ///
    /// # Errors
    ///
    /// [`EventError::BufferFull`] when the cap is reached and nothing can be
    /// dropped.
    pub fn record(
        &mut self,
        event_type: impl Into<String>,
        stage: WorkerStage,
        code: impl Into<String>,
        progress: Option<f32>,
        data: serde_json::Value,
        now: OffsetDateTime,
    ) -> Result<u64, EventError> {
        if self.pending.len() >= self.capacity {
            // Shed the oldest progress event rather than a state change: losing
            // a percentage reading costs nothing, losing "the write started"
            // loses the audit trail.
            let droppable = self
                .pending
                .iter()
                .position(|event| event.event_type == "progress");
            match droppable {
                Some(index) => {
                    self.pending.remove(index);
                }
                None => {
                    return Err(EventError::BufferFull {
                        capacity: self.capacity,
                    });
                }
            }
        }

        let sequence = self.next_sequence;
        self.next_sequence += 1;
        self.pending.push_back(WorkerEvent {
            sequence,
            worker_time: now,
            event_type: event_type.into(),
            stage,
            code: code.into(),
            progress: progress.map(|value| value.clamp(0.0, 1.0)),
            data,
        });
        Ok(sequence)
    }

    /// The next batch to send, oldest first.
    ///
    /// Does not remove anything: events are only dropped once the server says
    /// it has them. A batch sent and lost is simply sent again.
    #[must_use]
    pub fn batch(&self, max: usize) -> Vec<WorkerEvent> {
        self.pending.iter().take(max).cloned().collect()
    }

    /// Record the server's acknowledgement.
    ///
    /// Acknowledgements are monotonic; an older one is ignored rather than
    /// treated as an error, because a delayed response overtaking a newer one
    /// is a normal network occurrence and not a fault.
    pub fn acknowledge(&mut self, through_sequence: u64) {
        if through_sequence <= self.acknowledged_through {
            return;
        }
        self.acknowledged_through = through_sequence;
        self.pending
            .retain(|event| event.sequence > through_sequence);
    }

    /// Whether everything recorded has been accepted.
    #[must_use]
    pub fn is_drained(&self) -> bool {
        self.pending.is_empty()
    }

    /// Resume a buffer after a restart.
    ///
    /// The worker knows from its recovery record which sequence the server had
    /// reached; numbering continues after it so a resumed attempt cannot reuse
    /// a sequence the server already holds.
    #[must_use]
    pub fn resumed(attempt_id: BurnAttemptId, acknowledged_through: u64, capacity: usize) -> Self {
        Self {
            attempt_id,
            pending: VecDeque::new(),
            next_sequence: acknowledged_through + 1,
            acknowledged_through,
            capacity: capacity.max(1),
        }
    }
}

/// A lease on one attempt.
///
/// Held so the server knows the worker is alive. Deliberately not permission
/// to continue writing: see [`Lease::write_may_continue`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    /// The attempt.
    pub attempt_id: BurnAttemptId,
    /// When the lease lapses.
    pub expires_at: OffsetDateTime,
    /// How long the server granted.
    pub duration: Duration,
}

impl Lease {
    /// Whether the lease has lapsed at `now`.
    #[must_use]
    pub fn is_expired(&self, now: OffsetDateTime) -> bool {
        now >= self.expires_at
    }

    /// Whether renewal should be attempted.
    ///
    /// Renews once half the lease has elapsed, leaving the remaining half as
    /// margin for a slow or briefly unreachable server. Waiting until nearly
    /// expired would make one dropped request enough to lose it.
    #[must_use]
    pub fn should_renew(&self, now: OffsetDateTime) -> bool {
        let half = self.duration / 2;
        let renew_at = self.expires_at - half;
        now >= renew_at
    }

    /// Whether a write in progress may continue despite the lease state.
    ///
    /// Always true, and that is the point rather than an oversight. A lease is
    /// a liveness signal for the server's benefit; the laser is already
    /// burning. Stopping mid-write guarantees a ruined disc, whereas finishing
    /// and reconciling afterwards may not. The worker instead refuses to start
    /// anything *new* without a valid lease.
    #[must_use]
    pub const fn write_may_continue(&self) -> bool {
        true
    }

    /// Whether new work may be claimed.
    #[must_use]
    pub fn may_claim_new_work(&self, now: OffsetDateTime) -> bool {
        !self.is_expired(now)
    }
}

/// What a worker records locally before every irreversible transition.
///
/// Written atomically and cleared only after the server acknowledges terminal
/// completion. Its presence on startup means an attempt was in flight, and its
/// stage says how far it got.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecoveryRecord {
    /// The attempt.
    pub attempt_id: BurnAttemptId,
    /// The job it belongs to.
    pub burn_job_id: BurnJobId,
    /// The stage reached.
    pub stage: WorkerStage,
    /// Fingerprint of the drive, so a record is not applied to a drive that
    /// has since been swapped.
    pub drive_identity: String,
    /// The artifact being written.
    pub artifact_id: ArtifactId,
    /// Digest of the manifest used.
    pub manifest_sha256: Sha256Digest,
    /// Highest sequence the server acknowledged.
    pub last_event_sequence: u64,
    /// Engine in use.
    pub engine: String,
    /// When the record was last written.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// Why local recovery state could not be handled.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    /// The record could not be read or written.
    #[error("{operation} failed for {path}")]
    Io {
        /// What was attempted.
        operation: &'static str,
        /// Path involved.
        path: PathBuf,
        /// Cause.
        #[source]
        source: std::io::Error,
    },
    /// The record on disk is not readable.
    ///
    /// Treated as "an attempt was in flight and its details are unknown"
    /// rather than "no attempt", because the safe reading of a corrupt record
    /// is the pessimistic one.
    #[error("the recovery record at {path} is unreadable")]
    Corrupt {
        /// Path involved.
        path: PathBuf,
    },
}

/// Persists the recovery record for one worker.
#[derive(Debug, Clone)]
pub struct RecoveryStore {
    path: PathBuf,
}

impl RecoveryStore {
    /// A store writing to `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Where the record lives.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write the record atomically.
    ///
    /// Temporary file then rename, so a crash mid-write leaves either the old
    /// record or the new one, never a half-written record that would be read
    /// as corrupt and escalate an otherwise recoverable attempt.
    ///
    /// # Errors
    ///
    /// [`RecoveryError::Io`] if the write or rename fails.
    pub async fn write(&self, record: &RecoveryRecord) -> Result<(), RecoveryError> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| RecoveryError::Io {
                    operation: "creating the recovery directory",
                    path: parent.to_path_buf(),
                    source,
                })?;
        }

        let json = serde_json::to_string_pretty(record).map_err(|error| RecoveryError::Io {
            operation: "serializing the recovery record",
            path: self.path.clone(),
            source: std::io::Error::other(error),
        })?;

        let temporary = self.path.with_extension("part");
        tokio::fs::write(&temporary, &json)
            .await
            .map_err(|source| RecoveryError::Io {
                operation: "writing the recovery record",
                path: temporary.clone(),
                source,
            })?;
        tokio::fs::rename(&temporary, &self.path)
            .await
            .map_err(|source| RecoveryError::Io {
                operation: "publishing the recovery record",
                path: self.path.clone(),
                source,
            })
    }

    /// Read the record, if one exists.
    ///
    /// # Errors
    ///
    /// [`RecoveryError::Corrupt`] if a record exists but cannot be parsed, or
    /// [`RecoveryError::Io`] on a read failure.
    pub async fn read(&self) -> Result<Option<RecoveryRecord>, RecoveryError> {
        let text = match tokio::fs::read_to_string(&self.path).await {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(RecoveryError::Io {
                    operation: "reading the recovery record",
                    path: self.path.clone(),
                    source,
                });
            }
        };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|_| RecoveryError::Corrupt {
                path: self.path.clone(),
            })
    }

    /// Remove the record.
    ///
    /// Only after the server acknowledges terminal completion. Clearing it
    /// earlier would lose the evidence that an attempt was in flight, which is
    /// exactly what recovery needs.
    ///
    /// # Errors
    ///
    /// [`RecoveryError::Io`] if removal fails for a reason other than absence.
    pub async fn clear(&self) -> Result<(), RecoveryError> {
        match tokio::fs::remove_file(&self.path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(RecoveryError::Io {
                operation: "clearing the recovery record",
                path: self.path.clone(),
                source,
            }),
        }
    }
}

/// What the server tells a recovering worker to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryDirective {
    /// Carry on reporting events for the attempt.
    ResumeReporting,
    /// The write finished; proceed to verification.
    ResumeVerification,
    /// A human must look at it.
    MarkNeedsAttention,
    /// Nothing was written; drop the local state.
    DiscardPrewriteState,
    /// The worker is no longer trusted.
    WorkerRevoked,
}

/// What a recovering worker may do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryPlan {
    /// Whether local state should be discarded.
    pub discard_local_state: bool,
    /// Whether the worker may accept new work afterwards.
    pub may_accept_new_work: bool,
    /// Whether the worker may write.
    ///
    /// False for every directive. Restarting a write is never a recovery
    /// action, so no path through this function can enable one.
    pub may_write: bool,
}

/// Translate a server directive into what the worker may do.
///
/// The important property is uniform across every branch: `may_write` is
/// always false. A worker that restarts and finds its previous process gone
/// has no way to know whether the laser already ran, and writing again would
/// destroy a second disc while producing a physical copy record for a disc
/// that may not match anything.
#[must_use]
pub const fn plan_for(directive: RecoveryDirective) -> RecoveryPlan {
    // Only one directive changes anything, which is the point: a pre-write
    // discard is the single case where nothing physical happened and the
    // worker can simply forget it and move on.
    //
    // Every other directive keeps the local record and holds the worker. The
    // reasons differ (an attempt still in progress needs its state, one
    // needing attention needs it as evidence for a human, and a revoked worker
    // must not act at all), but the resulting permissions are identical, and
    // that uniformity is worth more than distinguishing them.
    let discardable = matches!(directive, RecoveryDirective::DiscardPrewriteState);
    RecoveryPlan {
        discard_local_state: discardable,
        may_accept_new_work: discardable,
        // Never, under any directive. A worker that restarts cannot know
        // whether the laser already ran, and writing again destroys a second
        // disc while producing a physical copy record that may match nothing.
        may_write: false,
    }
}
