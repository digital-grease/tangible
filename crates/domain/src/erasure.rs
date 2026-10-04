// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Erasing a rewritable disc.
//!
//! An erasure is its own operation, never a side effect of a burn: it
//! destroys whatever is on the disc in one named drive, so somebody has to
//! ask for it, by name, for that drive. A burn refuses a disc that is not
//! blank; this is how such a disc becomes blank.
//!
//! The machine is short. A request is queued, the drive's worker takes it and
//! erases, and it ends in exactly one of five ways. Only a queued request can
//! be withdrawn: once a drive has started erasing, stopping it leaves the disc
//! unusable until it is erased again, which is worse than letting it finish.

use crate::enums::ErasureState;

/// A transition the machine does not allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("an erasure cannot go from {from} to {to}")]
pub struct ErasureTransitionError {
    /// Where it was.
    pub from: ErasureState,
    /// Where it was asked to go.
    pub to: ErasureState,
}

impl ErasureState {
    /// Whether the request is still going: queued or erasing.
    #[must_use]
    pub const fn is_open(self) -> bool {
        matches!(self, Self::Queued | Self::Erasing)
    }

    /// Whether nothing further can happen to it.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !self.is_open()
    }

    /// Whether the disc may have been changed.
    ///
    /// True for an erase that ran, whatever it achieved; false when nothing
    /// was done to the disc at all.
    #[must_use]
    pub const fn touched_the_disc(self) -> bool {
        matches!(self, Self::Erased | Self::Failed)
    }

    /// Move to `next`, if the machine allows it.
    ///
    /// # Errors
    ///
    /// [`ErasureTransitionError`] for any other move.
    pub fn transition_to(self, next: Self) -> Result<Self, ErasureTransitionError> {
        let allowed = match self {
            Self::Queued => matches!(next, Self::Erasing | Self::Canceled),
            // Refused and AlreadyBlank are reached from Erasing too: the
            // worker decides what is in the drive only once it has the drive.
            Self::Erasing => matches!(
                next,
                Self::Erased | Self::AlreadyBlank | Self::Refused | Self::Failed
            ),
            Self::Erased | Self::AlreadyBlank | Self::Refused | Self::Failed | Self::Canceled => {
                false
            }
        };
        if allowed {
            Ok(next)
        } else {
            Err(ErasureTransitionError {
                from: self,
                to: next,
            })
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use ErasureState::{AlreadyBlank, Canceled, Erased, Erasing, Failed, Queued, Refused};

    #[test]
    fn a_queued_erasure_is_taken_or_withdrawn() {
        assert_eq!(Queued.transition_to(Erasing), Ok(Erasing));
        assert_eq!(Queued.transition_to(Canceled), Ok(Canceled));
        assert!(Queued.transition_to(Erased).is_err(), "not without erasing");
    }

    #[test]
    fn an_erasure_in_progress_cannot_be_withdrawn() {
        // Stopping a drive mid-erase leaves the disc needing another erase.
        assert!(Erasing.transition_to(Canceled).is_err());
        for end in [Erased, AlreadyBlank, Refused, Failed] {
            assert_eq!(Erasing.transition_to(end), Ok(end));
        }
    }

    #[test]
    fn every_ending_is_final() {
        for end in [Erased, AlreadyBlank, Refused, Failed, Canceled] {
            assert!(end.is_terminal());
            for next in ErasureState::all() {
                assert!(end.transition_to(*next).is_err(), "{end} -> {next}");
            }
        }
    }

    #[test]
    fn only_an_erase_that_ran_may_have_changed_the_disc() {
        assert!(Erased.touched_the_disc());
        assert!(Failed.touched_the_disc());
        for untouched in [Queued, Erasing, AlreadyBlank, Refused, Canceled] {
            assert!(!untouched.touched_the_disc(), "{untouched}");
        }
    }
}
