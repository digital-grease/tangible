// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Burn state machines.
//!
//! Two machines, deliberately separate:
//!
//! - [`BurnJobState`] is the user's intent to produce a physical copy. It can
//!   be retried, and a retry creates a new attempt.
//! - [`BurnAttemptState`] is one execution against one drive. It is
//!   **append-only in meaning**: a failed attempt is never rewritten as
//!   successful.
//!
//! The separation exists because a burn consumes physical media. A job that
//! failed twice and succeeded on the third try has destroyed two discs, and
//! the record has to show that rather than presenting a clean success.
//!
//! Three burn invariants are enforced here in types:
//!
//! - a write begins only after hash verification and media preflight;
//! - a successful tool exit is not verified media, so `Written` and
//!   `Verified` are distinct states;
//! - the system distinguishes write success from read-back verification,
//!   which is why verification failure has its own terminal state rather
//!   than collapsing into "failed".

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::enums::EnumParseError;

/// Top-level state of a burn job.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum BurnJobState {
    /// Waiting for a worker with a compatible drive. Where every job starts.
    #[default]
    Queued,
    /// Claimed by a worker under a lease.
    Leased,
    /// The worker is copying burn input to local storage.
    Staging,
    /// Staged and ready, but the drive has no usable medium.
    WaitingForMedia,
    /// Checking drive capability and medium against the plan.
    Preflighting,
    /// Preflight passed. Everything needed to write is in place.
    Ready,
    /// Writing to the medium. The destructive phase.
    Writing,
    /// Closing the session or finalizing the disc.
    Finalizing,
    /// Reading the medium back and comparing.
    Verifying,
    /// Written and verified according to policy. Terminal.
    Complete,
    /// Failed. Terminal until an operator requests a new attempt.
    Failed,
    /// Cancelled before writing began. Terminal.
    Canceled,
    /// Stopped and requires a human decision. Terminal until acted on.
    NeedsAttention,
}

/// State of one execution attempt against one drive.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum BurnAttemptState {
    /// Lease acquired, nothing done yet. Where every attempt starts.
    #[default]
    Claimed,
    /// Copying burn input locally and re-verifying its hash.
    Staging,
    /// Inspecting drive and medium.
    Preflighting,
    /// Writing. Media is being consumed from this point.
    Writing,
    /// The engine exited successfully. **Not** verified media.
    Written,
    /// Reading back and comparing.
    Verifying,
    /// Read back and matched. Terminal, and the only clean success.
    Verified,
    /// The write completed but read-back did not match.
    ///
    /// Distinct from [`Self::WriteFailed`]: the disc exists and is physically
    /// written but does not hold what was intended. It must be tracked so it
    /// can be destroyed rather than shelved.
    VerificationFailed,
    /// The write itself failed. Media state is unknown and likely ruined.
    WriteFailed,
    /// Failed before any write began. No media was consumed.
    FailedBeforeWrite,
    /// Abandoned before writing, at the operator's request.
    Canceled,
    /// The worker lost its lease or restarted mid-attempt.
    ///
    /// Terminal for this attempt. The worker records locally and reconciles
    /// before accepting other work, and must never start a second write.
    Interrupted,
}

/// Why a burn transition was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BurnTransitionError {
    /// The edge is not part of the machine.
    #[error("cannot move a burn job from {from} to {to}")]
    InvalidJob {
        /// Current state.
        from: BurnJobState,
        /// Requested state.
        to: BurnJobState,
    },

    /// The edge is not part of the attempt machine.
    #[error("cannot move a burn attempt from {from} to {to}")]
    InvalidAttempt {
        /// Current state.
        from: BurnAttemptState,
        /// Requested state.
        to: BurnAttemptState,
    },

    /// The job or attempt has already finished.
    #[error("burn is already in the terminal state {state}")]
    AlreadyTerminal {
        /// The terminal state.
        state: String,
    },

    /// Cancellation was requested after writing began.
    ///
    /// Refused because the medium is already being consumed. Stopping mid-write
    /// produces a ruined disc, so the operation is an abort with a recorded
    /// outcome, never a clean cancel.
    #[error("cannot cancel a burn once writing has started")]
    WriteInProgress,

    /// A write was requested without preflight having passed.
    #[error("cannot begin writing from {from}: preflight must pass first")]
    PreflightNotPassed {
        /// The state the caller tried to write from.
        from: BurnJobState,
    },
}

impl BurnJobState {
    /// The persisted textual value.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Leased => "leased",
            Self::Staging => "staging",
            Self::WaitingForMedia => "waiting_for_media",
            Self::Preflighting => "preflighting",
            Self::Ready => "ready",
            Self::Writing => "writing",
            Self::Finalizing => "finalizing",
            Self::Verifying => "verifying",
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::NeedsAttention => "needs_attention",
        }
    }

    /// Every variant, in declaration order.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Queued,
            Self::Leased,
            Self::Staging,
            Self::WaitingForMedia,
            Self::Preflighting,
            Self::Ready,
            Self::Writing,
            Self::Finalizing,
            Self::Verifying,
            Self::Complete,
            Self::Failed,
            Self::Canceled,
            Self::NeedsAttention,
        ]
    }

    /// Whether the job has finished.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Complete | Self::Failed | Self::Canceled | Self::NeedsAttention
        )
    }

    /// Whether the medium is being or has been written in this job.
    ///
    /// The point of no return. Past it, cancellation cannot be clean and a
    /// physical disc exists that must be accounted for.
    #[must_use]
    pub const fn has_started_writing(&self) -> bool {
        matches!(
            self,
            Self::Writing | Self::Finalizing | Self::Verifying | Self::Complete
        )
    }

    /// Whether a physical copy record must exist for this job.
    ///
    /// No physical-copy row without a completed write stage; and, conversely,
    /// a completed write always produced a disc, even a bad one.
    #[must_use]
    pub const fn requires_physical_copy_record(&self) -> bool {
        matches!(self, Self::Verifying | Self::Complete)
    }

    /// Successor states on the happy path.
    fn successors(self) -> &'static [Self] {
        match self {
            Self::Queued => &[Self::Leased],
            Self::Leased => &[Self::Staging],
            // Staging may find no disc in the drive.
            Self::Staging => &[Self::Preflighting, Self::WaitingForMedia],
            // Media arrived; re-run preflight against what was inserted.
            Self::WaitingForMedia => &[Self::Preflighting],
            // Preflight can also discover the medium is unsuitable.
            Self::Preflighting => &[Self::Ready, Self::WaitingForMedia],
            Self::Ready => &[Self::Writing],
            Self::Writing => &[Self::Finalizing],
            // Policy may skip read-back in principle, but it is required for
            // ISO in the main flow.
            Self::Finalizing => &[Self::Verifying, Self::Complete],
            Self::Verifying => &[Self::Complete],
            _ => &[],
        }
    }

    /// Whether an edge exists from this state to `to`.
    #[must_use]
    pub fn can_transition_to(self, to: Self) -> bool {
        if self.is_terminal() {
            return false;
        }
        match to {
            // Any non-terminal state may fail or need a human.
            Self::Failed | Self::NeedsAttention => true,
            // Cancellation is clean only before the medium is touched.
            Self::Canceled => !self.has_started_writing(),
            _ => self.successors().contains(&to),
        }
    }

    /// Move to an explicit state, validating the edge.
    ///
    /// # Errors
    ///
    /// [`BurnTransitionError::AlreadyTerminal`] if finished,
    /// [`BurnTransitionError::PreflightNotPassed`] for a write that skipped
    /// preflight, or [`BurnTransitionError::InvalidJob`] otherwise.
    pub fn transition_to(self, to: Self) -> Result<Self, BurnTransitionError> {
        if self.is_terminal() {
            return Err(BurnTransitionError::AlreadyTerminal {
                state: self.to_string(),
            });
        }
        if to == Self::Writing && self != Self::Ready {
            // Distinguished from a plain invalid edge because this is the
            // preflight-before-write invariant, and the operator should be
            // told exactly which rule stopped them.
            return Err(BurnTransitionError::PreflightNotPassed { from: self });
        }
        if self.can_transition_to(to) {
            Ok(to)
        } else {
            Err(BurnTransitionError::InvalidJob { from: self, to })
        }
    }

    /// Request cancellation.
    ///
    /// # Errors
    ///
    /// [`BurnTransitionError::WriteInProgress`] once the medium is being
    /// written, or [`BurnTransitionError::AlreadyTerminal`] if finished.
    pub fn cancel(self) -> Result<Self, BurnTransitionError> {
        if self.is_terminal() {
            return Err(BurnTransitionError::AlreadyTerminal {
                state: self.to_string(),
            });
        }
        if self.has_started_writing() {
            return Err(BurnTransitionError::WriteInProgress);
        }
        Ok(Self::Canceled)
    }
}

impl BurnAttemptState {
    /// The persisted textual value.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Claimed => "claimed",
            Self::Staging => "staging",
            Self::Preflighting => "preflighting",
            Self::Writing => "writing",
            Self::Written => "written",
            Self::Verifying => "verifying",
            Self::Verified => "verified",
            Self::VerificationFailed => "verification_failed",
            Self::WriteFailed => "write_failed",
            Self::FailedBeforeWrite => "failed_before_write",
            Self::Canceled => "canceled",
            Self::Interrupted => "interrupted",
        }
    }

    /// Every variant, in declaration order.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Claimed,
            Self::Staging,
            Self::Preflighting,
            Self::Writing,
            Self::Written,
            Self::Verifying,
            Self::Verified,
            Self::VerificationFailed,
            Self::WriteFailed,
            Self::FailedBeforeWrite,
            Self::Canceled,
            Self::Interrupted,
        ]
    }

    /// Whether this attempt has finished.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Verified
                | Self::VerificationFailed
                | Self::WriteFailed
                | Self::FailedBeforeWrite
                | Self::Canceled
                | Self::Interrupted
        )
    }

    /// Whether this attempt consumed physical media.
    ///
    /// True from the moment writing starts, including every failure after
    /// that point. Used to decide whether a physical copy record is owed:
    /// a ruined disc is still a disc, and shelving it unrecorded is how bad
    /// media gets reused.
    #[must_use]
    pub const fn consumed_media(&self) -> bool {
        matches!(
            self,
            Self::Writing
                | Self::Written
                | Self::Verifying
                | Self::Verified
                | Self::VerificationFailed
                | Self::WriteFailed
                | Self::Interrupted
        )
    }

    /// Whether the attempt succeeded outright.
    ///
    /// Only [`Self::Verified`]. A successful tool exit is not verified media,
    /// so [`Self::Written`] deliberately does not qualify.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(self, Self::Verified)
    }

    /// Successor states.
    fn successors(self) -> &'static [Self] {
        match self {
            Self::Claimed => &[Self::Staging],
            Self::Staging => &[Self::Preflighting],
            Self::Preflighting => &[Self::Writing],
            Self::Writing => &[Self::Written],
            Self::Written => &[Self::Verifying],
            Self::Verifying => &[Self::Verified, Self::VerificationFailed],
            _ => &[],
        }
    }

    /// Whether an edge exists from this state to `to`.
    #[must_use]
    pub fn can_transition_to(self, to: Self) -> bool {
        if self.is_terminal() {
            return false;
        }
        match to {
            // A lease can be lost at any live moment.
            Self::Interrupted => true,
            // Which failure applies depends on whether media was touched.
            Self::WriteFailed => self.consumed_media(),
            Self::FailedBeforeWrite | Self::Canceled => !self.consumed_media(),
            _ => self.successors().contains(&to),
        }
    }

    /// Move to an explicit state, validating the edge.
    ///
    /// # Errors
    ///
    /// [`BurnTransitionError::AlreadyTerminal`] if finished, or
    /// [`BurnTransitionError::InvalidAttempt`] if the edge does not exist.
    pub fn transition_to(self, to: Self) -> Result<Self, BurnTransitionError> {
        if self.is_terminal() {
            return Err(BurnTransitionError::AlreadyTerminal {
                state: self.to_string(),
            });
        }
        if self.can_transition_to(to) {
            Ok(to)
        } else {
            Err(BurnTransitionError::InvalidAttempt { from: self, to })
        }
    }
}

impl fmt::Display for BurnJobState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for BurnAttemptState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BurnJobState {
    type Err = EnumParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::all()
            .iter()
            .copied()
            .find(|state| state.as_str() == value)
            .ok_or_else(|| EnumParseError {
                kind: "BurnJobState",
                value: value.to_owned(),
            })
    }
}

impl FromStr for BurnAttemptState {
    type Err = EnumParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::all()
            .iter()
            .copied()
            .find(|state| state.as_str() == value)
            .ok_or_else(|| EnumParseError {
                kind: "BurnAttemptState",
                value: value.to_owned(),
            })
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    // --- job machine --------------------------------------------------------

    #[test]
    fn the_job_happy_path_runs_end_to_end() {
        let path = [
            BurnJobState::Leased,
            BurnJobState::Staging,
            BurnJobState::Preflighting,
            BurnJobState::Ready,
            BurnJobState::Writing,
            BurnJobState::Finalizing,
            BurnJobState::Verifying,
            BurnJobState::Complete,
        ];
        let mut state = BurnJobState::default();
        for want in path {
            state = state
                .transition_to(want)
                .unwrap_or_else(|error| panic!("{state} -> {want} should be legal: {error}"));
        }
        assert!(state.is_terminal());
    }

    #[test]
    fn writing_requires_preflight_to_have_passed() {
        // Every route into Writing that skips Ready must be refused, and
        // refused with the reason rather than a generic error.
        for from in [
            BurnJobState::Queued,
            BurnJobState::Leased,
            BurnJobState::Staging,
            BurnJobState::WaitingForMedia,
            BurnJobState::Preflighting,
        ] {
            let error = from
                .transition_to(BurnJobState::Writing)
                .expect_err("must refuse");
            assert_eq!(
                error,
                BurnTransitionError::PreflightNotPassed { from },
                "{from} must not reach Writing directly"
            );
        }
        assert_eq!(
            BurnJobState::Ready.transition_to(BurnJobState::Writing),
            Ok(BurnJobState::Writing)
        );
    }

    #[test]
    fn a_burn_cannot_be_cleanly_cancelled_once_writing() {
        // The medium is already being consumed; stopping produces a ruined
        // disc, so this is an abort with a recorded outcome, not a cancel.
        for writing in [
            BurnJobState::Writing,
            BurnJobState::Finalizing,
            BurnJobState::Verifying,
        ] {
            assert_eq!(
                writing.cancel(),
                Err(BurnTransitionError::WriteInProgress),
                "{writing} must refuse a clean cancel"
            );
        }
    }

    #[test]
    fn a_burn_can_be_cancelled_before_the_medium_is_touched() {
        for early in [
            BurnJobState::Queued,
            BurnJobState::Leased,
            BurnJobState::Staging,
            BurnJobState::WaitingForMedia,
            BurnJobState::Preflighting,
            BurnJobState::Ready,
        ] {
            assert_eq!(early.cancel(), Ok(BurnJobState::Canceled), "{early}");
        }
    }

    #[test]
    fn waiting_for_media_can_be_entered_and_left() {
        // Inserting a disc must resume the job, and preflight re-runs against
        // whatever was actually inserted rather than trusting the earlier look.
        assert_eq!(
            BurnJobState::Staging.transition_to(BurnJobState::WaitingForMedia),
            Ok(BurnJobState::WaitingForMedia)
        );
        assert_eq!(
            BurnJobState::WaitingForMedia.transition_to(BurnJobState::Preflighting),
            Ok(BurnJobState::Preflighting)
        );
        // Preflight may also reject what was inserted and ask again.
        assert_eq!(
            BurnJobState::Preflighting.transition_to(BurnJobState::WaitingForMedia),
            Ok(BurnJobState::WaitingForMedia)
        );
    }

    #[test]
    fn a_completed_write_always_owes_a_physical_copy_record() {
        // The rule holds in both directions: a completed write owes a record,
        // and nothing that stopped before the write does.
        assert!(BurnJobState::Complete.requires_physical_copy_record());
        assert!(BurnJobState::Verifying.requires_physical_copy_record());
        for no_disc in [
            BurnJobState::Queued,
            BurnJobState::Ready,
            BurnJobState::Canceled,
        ] {
            assert!(!no_disc.requires_physical_copy_record(), "{no_disc}");
        }
    }

    #[test]
    fn terminal_job_states_accept_nothing() {
        for terminal in BurnJobState::all()
            .iter()
            .copied()
            .filter(BurnJobState::is_terminal)
        {
            for target in BurnJobState::all().iter().copied() {
                assert!(
                    !terminal.can_transition_to(target),
                    "{terminal} must not reach {target}"
                );
            }
        }
    }

    // --- attempt machine ----------------------------------------------------

    #[test]
    fn the_attempt_happy_path_ends_at_verified() {
        let path = [
            BurnAttemptState::Staging,
            BurnAttemptState::Preflighting,
            BurnAttemptState::Writing,
            BurnAttemptState::Written,
            BurnAttemptState::Verifying,
            BurnAttemptState::Verified,
        ];
        let mut state = BurnAttemptState::default();
        for want in path {
            state = state
                .transition_to(want)
                .unwrap_or_else(|error| panic!("{state} -> {want}: {error}"));
        }
        assert!(state.is_success());
    }

    #[test]
    fn a_written_disc_is_not_a_verified_disc() {
        // A successful tool exit is not verified media, which is the whole
        // reason these are separate states.
        assert!(!BurnAttemptState::Written.is_success());
        assert!(!BurnAttemptState::Written.is_terminal());
        assert!(BurnAttemptState::Verified.is_success());
    }

    #[test]
    fn an_attempt_cannot_jump_from_writing_to_verified() {
        // Skipping the read-back would let a tool's exit code stand in for
        // verification, which is exactly what these separate states exist to
        // prevent.
        assert!(
            BurnAttemptState::Writing
                .transition_to(BurnAttemptState::Verified)
                .is_err()
        );
        assert!(
            BurnAttemptState::Written
                .transition_to(BurnAttemptState::Verified)
                .is_err()
        );
    }

    #[test]
    fn verification_failure_is_distinct_from_write_failure() {
        // A verification failure produced a real disc that must be tracked and
        // destroyed; a pre-write failure produced nothing.
        assert!(BurnAttemptState::VerificationFailed.consumed_media());
        assert!(BurnAttemptState::WriteFailed.consumed_media());
        assert!(!BurnAttemptState::FailedBeforeWrite.consumed_media());
        assert!(!BurnAttemptState::Canceled.consumed_media());
    }

    #[test]
    fn a_failed_attempt_can_never_be_rewritten_as_successful() {
        // Attempt state is append-only in meaning. Retrying creates a new
        // attempt; it does not edit this one.
        for failed in [
            BurnAttemptState::WriteFailed,
            BurnAttemptState::VerificationFailed,
            BurnAttemptState::FailedBeforeWrite,
            BurnAttemptState::Interrupted,
            BurnAttemptState::Canceled,
        ] {
            for target in BurnAttemptState::all().iter().copied() {
                assert!(
                    !failed.can_transition_to(target),
                    "{failed} must not become {target}"
                );
            }
        }
    }

    #[test]
    fn a_lease_can_be_lost_at_any_live_moment() {
        // The worker records locally, reconciles, and must not start a second
        // write.
        for live in BurnAttemptState::all()
            .iter()
            .copied()
            .filter(|state| !BurnAttemptState::is_terminal(state))
        {
            assert_eq!(
                live.transition_to(BurnAttemptState::Interrupted),
                Ok(BurnAttemptState::Interrupted),
                "{live} must be interruptible"
            );
        }
    }

    #[test]
    fn pre_write_failures_cannot_be_recorded_as_write_failures() {
        // Mislabelling would imply a disc was consumed when none was.
        for before in [
            BurnAttemptState::Claimed,
            BurnAttemptState::Staging,
            BurnAttemptState::Preflighting,
        ] {
            assert!(
                before.transition_to(BurnAttemptState::WriteFailed).is_err(),
                "{before} consumed no media, so WriteFailed is wrong"
            );
            assert_eq!(
                before.transition_to(BurnAttemptState::FailedBeforeWrite),
                Ok(BurnAttemptState::FailedBeforeWrite)
            );
        }
    }

    #[test]
    fn an_attempt_that_consumed_media_cannot_be_merely_cancelled() {
        for consumed in [
            BurnAttemptState::Writing,
            BurnAttemptState::Written,
            BurnAttemptState::Verifying,
        ] {
            assert!(
                consumed.transition_to(BurnAttemptState::Canceled).is_err(),
                "{consumed} destroyed a disc; cancelling would hide it"
            );
        }
    }

    // --- persistence --------------------------------------------------------

    #[test]
    fn both_machines_round_trip_through_text_and_serde() {
        for state in BurnJobState::all() {
            let text = state.as_str();
            assert_eq!(&text.parse::<BurnJobState>().expect("parse"), state);
            let json = serde_json::to_string(state).expect("serialize");
            assert_eq!(json, format!("\"{text}\""));
        }
        for state in BurnAttemptState::all() {
            let text = state.as_str();
            assert_eq!(&text.parse::<BurnAttemptState>().expect("parse"), state);
            let json = serde_json::to_string(state).expect("serialize");
            assert_eq!(json, format!("\"{text}\""));
        }
    }

    #[test]
    fn unknown_state_text_is_rejected() {
        assert!("melting".parse::<BurnJobState>().is_err());
        assert!("melting".parse::<BurnAttemptState>().is_err());
    }
}
