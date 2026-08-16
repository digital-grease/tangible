// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The import state machine.
//!
//! Transitions live here rather than in route handlers or repositories, and
//! invalid ones return typed errors instead of being silently tolerated.
//!
//! The happy path is:
//!
//! ```text
//! requested -> acquiring -> staged -> hashing -> inspecting -> registering -> complete
//! ```
//!
//! Any active state may fail, be cancelled, or be quarantined. Two rules make
//! the machine safe to retry and safe to interrupt:
//!
//! - **Cancellation cannot unpublish a registered artifact**. Once bytes are
//!   in the content-addressed store and the artifact row exists, cancelling
//!   the *job* must not remove them.
//! - **A retry re-enters the last safe stage**, not the beginning. Re-running
//!   acquisition after hashing has already succeeded would re-download
//!   gigabytes for nothing.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::enums::EnumParseError;

/// Where an import has got to.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ImportState {
    /// Accepted, not yet started. Where every import starts.
    #[default]
    Requested,
    /// Transferring bytes from the source.
    Acquiring,
    /// Bytes are in the staging area, complete but unverified.
    Staged,
    /// Computing the streaming SHA-256 over staged bytes.
    Hashing,
    /// Parsing structure and running format validation.
    Inspecting,
    /// Moving into the content-addressed store and writing domain rows.
    Registering,
    /// Artifact registered. Terminal, and successful.
    Complete,
    /// Failed for a reason that may succeed on retry.
    FailedRetryable,
    /// Failed for a reason that will not change. Terminal.
    FailedTerminal,
    /// Stopped at the operator's request. Terminal.
    Canceled,
    /// Isolated pending review. Terminal until an operator acts.
    Quarantined,
}

/// Why a transition was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImportTransitionError {
    /// The transition is not part of the machine.
    #[error("cannot move an import from {from} to {to}")]
    Invalid {
        /// Current state.
        from: ImportState,
        /// Requested state.
        to: ImportState,
    },

    /// The job has already finished.
    #[error("import is already in the terminal state {state}")]
    AlreadyTerminal {
        /// The terminal state reached.
        state: ImportState,
    },

    /// Cancellation was requested after the artifact was registered.
    ///
    /// Refused deliberately: the artifact exists and is immutable. Deleting it
    /// is a separate, audited operation, not a side effect of cancelling a job.
    #[error("cannot cancel an import that has already registered its artifact")]
    AlreadyRegistered,

    /// A retry was requested on a job that cannot be retried.
    #[error("import in state {state} cannot be retried")]
    NotRetryable {
        /// The state the job is in.
        state: ImportState,
    },
}

impl ImportState {
    /// The persisted textual value.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Acquiring => "acquiring",
            Self::Staged => "staged",
            Self::Hashing => "hashing",
            Self::Inspecting => "inspecting",
            Self::Registering => "registering",
            Self::Complete => "complete",
            Self::FailedRetryable => "failed_retryable",
            Self::FailedTerminal => "failed_terminal",
            Self::Canceled => "canceled",
            Self::Quarantined => "quarantined",
        }
    }

    /// Every variant, in declaration order.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Requested,
            Self::Acquiring,
            Self::Staged,
            Self::Hashing,
            Self::Inspecting,
            Self::Registering,
            Self::Complete,
            Self::FailedRetryable,
            Self::FailedTerminal,
            Self::Canceled,
            Self::Quarantined,
        ]
    }

    /// Whether the job is finished and will not move on its own.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Complete | Self::FailedTerminal | Self::Canceled | Self::Quarantined
        )
    }

    /// Whether the job is progressing and holds a lease.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Requested
                | Self::Acquiring
                | Self::Staged
                | Self::Hashing
                | Self::Inspecting
                | Self::Registering
        )
    }

    /// Whether the job has stopped but may still resume.
    ///
    /// A third category alongside active and terminal: the job holds no lease
    /// and is making no progress, but its work so far is intact and a retry
    /// can pick it up. Collapsing this into either of the others would be
    /// wrong: treating it as active would imply a live lease, and treating it
    /// as terminal would discard resumable work.
    #[must_use]
    pub const fn is_awaiting_retry(&self) -> bool {
        matches!(self, Self::FailedRetryable)
    }

    /// Whether the artifact has been committed to the store.
    ///
    /// From `Registering` onward the bytes may already be in the
    /// content-addressed store, so cancellation must not claim to undo it.
    #[must_use]
    pub const fn has_registered_artifact(&self) -> bool {
        matches!(self, Self::Registering | Self::Complete)
    }

    /// The next state on the happy path, if there is one.
    #[must_use]
    pub const fn next_on_success(&self) -> Option<Self> {
        match self {
            Self::Requested => Some(Self::Acquiring),
            Self::Acquiring => Some(Self::Staged),
            Self::Staged => Some(Self::Hashing),
            Self::Hashing => Some(Self::Inspecting),
            Self::Inspecting => Some(Self::Registering),
            Self::Registering => Some(Self::Complete),
            _ => None,
        }
    }

    /// Advance one step along the happy path.
    ///
    /// # Errors
    ///
    /// [`ImportTransitionError::AlreadyTerminal`] if the job has finished, or
    /// [`ImportTransitionError::Invalid`] if there is no successor.
    pub fn advance(self) -> Result<Self, ImportTransitionError> {
        if self.is_terminal() {
            return Err(ImportTransitionError::AlreadyTerminal { state: self });
        }
        self.next_on_success()
            .ok_or(ImportTransitionError::Invalid {
                from: self,
                to: Self::Complete,
            })
    }

    /// Move to an explicit state, validating the edge.
    ///
    /// # Errors
    ///
    /// [`ImportTransitionError::Invalid`] if the edge is not in the machine,
    /// or [`ImportTransitionError::AlreadyTerminal`] if the job has finished.
    pub fn transition_to(self, to: Self) -> Result<Self, ImportTransitionError> {
        if self.is_terminal() {
            return Err(ImportTransitionError::AlreadyTerminal { state: self });
        }
        if self.can_transition_to(to) {
            Ok(to)
        } else {
            Err(ImportTransitionError::Invalid { from: self, to })
        }
    }

    /// Whether an edge exists from this state to `to`.
    #[must_use]
    pub fn can_transition_to(self, to: Self) -> bool {
        if self.is_terminal() {
            return false;
        }
        // Any active state may fail, be cancelled, or be quarantined, except
        // that cancelling after registration is refused, which `cancel`
        // enforces with a clearer error than "invalid transition".
        match to {
            Self::FailedRetryable | Self::FailedTerminal | Self::Quarantined => true,
            Self::Canceled => !self.has_registered_artifact(),
            _ => self.next_on_success() == Some(to),
        }
    }

    /// Request cancellation.
    ///
    /// # Errors
    ///
    /// [`ImportTransitionError::AlreadyRegistered`] once the artifact exists,
    /// or [`ImportTransitionError::AlreadyTerminal`] if the job has finished.
    pub fn cancel(self) -> Result<Self, ImportTransitionError> {
        if self.is_terminal() {
            return Err(ImportTransitionError::AlreadyTerminal { state: self });
        }
        if self.has_registered_artifact() {
            return Err(ImportTransitionError::AlreadyRegistered);
        }
        Ok(Self::Canceled)
    }

    /// The stage a retry should re-enter.
    ///
    /// Work already completed is not repeated: a failure during inspection
    /// re-enters inspection, because the bytes are staged and hashed already.
    /// Only a failure that leaves nothing usable returns to the beginning.
    #[must_use]
    pub const fn retry_stage(failed_during: Self) -> Self {
        match failed_during {
            // Nothing was transferred, or the transfer is untrustworthy.
            Self::Requested | Self::Acquiring => Self::Requested,
            // Bytes are staged; resume from hashing.
            Self::Staged | Self::Hashing => Self::Hashing,
            // Hash is known; re-inspect.
            Self::Inspecting => Self::Inspecting,
            // Registration is idempotent by artifact identity, so
            // re-entering it is safe.
            Self::Registering => Self::Registering,
            other => other,
        }
    }

    /// Resume a retryable failure.
    ///
    /// # Errors
    ///
    /// [`ImportTransitionError::NotRetryable`] unless the state is
    /// [`ImportState::FailedRetryable`].
    pub const fn retry_from(self, failed_during: Self) -> Result<Self, ImportTransitionError> {
        if !matches!(self, Self::FailedRetryable) {
            return Err(ImportTransitionError::NotRetryable { state: self });
        }
        Ok(Self::retry_stage(failed_during))
    }
}

impl fmt::Display for ImportState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ImportState {
    type Err = EnumParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::all()
            .iter()
            .copied()
            .find(|state| state.as_str() == value)
            .ok_or_else(|| EnumParseError {
                kind: "ImportState",
                value: value.to_owned(),
            })
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_happy_path_runs_end_to_end() {
        let mut state = ImportState::default();
        assert_eq!(state, ImportState::Requested);

        let expected = [
            ImportState::Acquiring,
            ImportState::Staged,
            ImportState::Hashing,
            ImportState::Inspecting,
            ImportState::Registering,
            ImportState::Complete,
        ];
        for want in expected {
            state = state.advance().expect("happy path advances");
            assert_eq!(state, want);
        }
        assert!(state.is_terminal());
    }

    #[test]
    fn a_completed_import_cannot_advance_again() {
        let error = ImportState::Complete.advance().expect_err("must refuse");
        assert_eq!(
            error,
            ImportTransitionError::AlreadyTerminal {
                state: ImportState::Complete
            }
        );
    }

    #[test]
    fn stages_cannot_be_skipped() {
        // Registering without hashing would put unverified bytes in the store.
        let error = ImportState::Staged
            .transition_to(ImportState::Registering)
            .expect_err("must refuse");
        assert_eq!(
            error,
            ImportTransitionError::Invalid {
                from: ImportState::Staged,
                to: ImportState::Registering,
            }
        );
    }

    #[test]
    fn the_machine_never_runs_backwards() {
        assert!(
            ImportState::Inspecting
                .transition_to(ImportState::Acquiring)
                .is_err()
        );
        assert!(
            ImportState::Complete
                .transition_to(ImportState::Hashing)
                .is_err()
        );
    }

    #[test]
    fn any_active_state_may_fail_or_be_quarantined() {
        for state in ImportState::all()
            .iter()
            .copied()
            .filter(ImportState::is_active)
        {
            for outcome in [
                ImportState::FailedRetryable,
                ImportState::FailedTerminal,
                ImportState::Quarantined,
            ] {
                assert_eq!(
                    state.transition_to(outcome),
                    Ok(outcome),
                    "{state} must be able to reach {outcome}"
                );
            }
        }
    }

    #[test]
    fn terminal_states_accept_no_transitions_at_all() {
        for terminal in ImportState::all()
            .iter()
            .copied()
            .filter(ImportState::is_terminal)
        {
            for target in ImportState::all().iter().copied() {
                assert!(
                    !terminal.can_transition_to(target),
                    "{terminal} must not reach {target}"
                );
            }
        }
    }

    // --- the two rules that matter -----------------------------------------

    #[test]
    fn cancellation_cannot_unpublish_a_registered_artifact() {
        // The bytes are in the content-addressed store and the artifact is
        // immutable; removing it is a separate audited operation, not a side
        // effect of cancelling a job.
        for registered in [ImportState::Registering, ImportState::Complete] {
            let error = registered.cancel().expect_err("must refuse");
            assert!(
                matches!(
                    error,
                    ImportTransitionError::AlreadyRegistered
                        | ImportTransitionError::AlreadyTerminal { .. }
                ),
                "{registered} produced {error:?}"
            );
        }
    }

    #[test]
    fn cancellation_is_allowed_before_registration() {
        for early in [
            ImportState::Requested,
            ImportState::Acquiring,
            ImportState::Staged,
            ImportState::Hashing,
            ImportState::Inspecting,
        ] {
            assert_eq!(early.cancel(), Ok(ImportState::Canceled), "{early}");
        }
    }

    #[test]
    fn retry_resumes_the_last_safe_stage_rather_than_restarting() {
        // Re-running acquisition after a successful hash would re-download
        // gigabytes for nothing.
        assert_eq!(
            ImportState::retry_stage(ImportState::Inspecting),
            ImportState::Inspecting
        );
        assert_eq!(
            ImportState::retry_stage(ImportState::Hashing),
            ImportState::Hashing
        );
        // A staged-but-unhashed transfer resumes at hashing, not acquisition.
        assert_eq!(
            ImportState::retry_stage(ImportState::Staged),
            ImportState::Hashing
        );
    }

    #[test]
    fn a_failed_transfer_restarts_from_the_beginning() {
        // Nothing trustworthy was produced, so there is nothing to resume.
        assert_eq!(
            ImportState::retry_stage(ImportState::Acquiring),
            ImportState::Requested
        );
    }

    #[test]
    fn only_retryable_failures_can_be_retried() {
        for state in [
            ImportState::FailedTerminal,
            ImportState::Canceled,
            ImportState::Complete,
            ImportState::Acquiring,
        ] {
            assert_eq!(
                state.retry_from(ImportState::Hashing),
                Err(ImportTransitionError::NotRetryable { state })
            );
        }
        assert_eq!(
            ImportState::FailedRetryable.retry_from(ImportState::Inspecting),
            Ok(ImportState::Inspecting)
        );
    }

    // --- persistence --------------------------------------------------------

    #[test]
    fn states_round_trip_through_text_and_serde() {
        for state in ImportState::all() {
            let text = state.as_str();
            assert_eq!(&text.parse::<ImportState>().expect("parse"), state);
            let json = serde_json::to_string(state).expect("serialize");
            assert_eq!(json, format!("\"{text}\""));
            assert_eq!(
                &serde_json::from_str::<ImportState>(&json).expect("deserialize"),
                state
            );
        }
    }

    #[test]
    fn every_state_is_exactly_one_of_active_terminal_or_awaiting_retry() {
        // The three categories must partition the machine. A state in none of
        // them would be unreachable by any scheduler; a state in two would be
        // handled twice.
        for state in ImportState::all() {
            let categories = usize::from(state.is_active())
                + usize::from(state.is_terminal())
                + usize::from(state.is_awaiting_retry());
            assert_eq!(
                categories, 1,
                "{state} belongs to {categories} categories, must be exactly 1"
            );
        }
    }

    #[test]
    fn a_retryable_failure_holds_no_lease_but_is_not_finished() {
        let state = ImportState::FailedRetryable;
        assert!(state.is_awaiting_retry());
        assert!(!state.is_active(), "must not appear to hold a lease");
        assert!(!state.is_terminal(), "must not discard resumable work");
    }
}
