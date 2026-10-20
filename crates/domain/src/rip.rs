// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Ripping the disc in a drive.
//!
//! A rip is the other direction from a burn: the drive's worker reads a disc
//! someone owns and sends what it read, which becomes an import. Like an
//! erasure it belongs to one drive, and only that drive's worker takes it.
//!
//! Unlike a burn or an erase, reading changes nothing on the disc, so a rip
//! can be withdrawn at any point before its import starts, and a failed one
//! is simply asked for again.
//!
//! ```text
//! queued -> reading -> uploading -> registering -> complete
//! queued | reading | uploading -> canceled
//! queued | reading | uploading -> refused | failed
//! registering -> failed           (the import failed)
//! ```
//!
//! `queued` reaches `refused` and `failed` only through the worker, which
//! decides from what is in the drive, or when a restarted worker's rip is
//! closed.

use crate::enums::RipState;

/// A transition the machine does not allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a rip cannot go from {from} to {to}")]
pub struct RipTransitionError {
    /// Where it was.
    pub from: RipState,
    /// Where it was asked to go.
    pub to: RipState,
}

/// The files a CD rip produces: cdrdao's table of contents and its data.
///
/// The worker may send only these, or [`IMAGE_OUTPUT`]. Fixed names keep a
/// worker from choosing where in the staging area its bytes land, and say
/// what the import will find.
pub const CD_OUTPUTS: [&str; 2] = ["disc.toc", "disc.bin"];

/// The file a DVD or Blu-ray rip produces: the disc's sectors in order, an
/// image of its filesystem.
pub const IMAGE_OUTPUT: &str = "disc.iso";

/// Every name a rip may send.
pub const RIP_OUTPUTS: [&str; 3] = ["disc.toc", "disc.bin", IMAGE_OUTPUT];

/// The largest table of contents a rip may send: cdrdao's are a few
/// kilobytes.
pub const MAX_TOC_BYTES: u64 = 1024 * 1024;

/// The largest CD data file: a 99-minute CD of raw 2352-byte sectors is
/// about 1.05 GB; this leaves room and no more.
pub const MAX_CD_DATA_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// The largest disc image: a quad-layer BDXL holds 128 GB.
pub const MAX_IMAGE_BYTES: u64 = 128 * 1000 * 1000 * 1000;

impl RipState {
    /// Whether the rip is still going.
    #[must_use]
    pub const fn is_open(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Reading | Self::Uploading | Self::Registering
        )
    }

    /// Whether nothing further can happen to it.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !self.is_open()
    }

    /// Whether the rip occupies its drive: from when the worker takes it
    /// until everything it read has been sent.
    #[must_use]
    pub const fn holds_the_drive(self) -> bool {
        matches!(self, Self::Reading | Self::Uploading)
    }

    /// Whether a person may still withdraw it. Once the import has started,
    /// the rip ends with the import.
    #[must_use]
    pub const fn can_cancel(self) -> bool {
        matches!(self, Self::Queued | Self::Reading | Self::Uploading)
    }

    /// Whether the worker may send files now.
    #[must_use]
    pub const fn accepts_uploads(self) -> bool {
        self.holds_the_drive()
    }

    /// Move to `next`, if the machine allows it.
    ///
    /// # Errors
    ///
    /// [`RipTransitionError`] for any other move.
    pub fn transition_to(self, next: Self) -> Result<Self, RipTransitionError> {
        let allowed = match self {
            Self::Queued => matches!(
                next,
                Self::Reading | Self::Canceled | Self::Refused | Self::Failed
            ),
            Self::Reading => matches!(
                next,
                Self::Uploading | Self::Canceled | Self::Refused | Self::Failed
            ),
            Self::Uploading => matches!(
                next,
                Self::Registering | Self::Canceled | Self::Refused | Self::Failed
            ),
            Self::Registering => matches!(next, Self::Complete | Self::Failed),
            Self::Complete | Self::Canceled | Self::Refused | Self::Failed => false,
        };
        if allowed {
            Ok(next)
        } else {
            Err(RipTransitionError {
                from: self,
                to: next,
            })
        }
    }
}

/// Whether `name` is one of the files a CD rip may send.
#[must_use]
pub fn is_cd_output(name: &str) -> bool {
    CD_OUTPUTS.contains(&name)
}

/// Whether `name` is one of the files any rip may send.
#[must_use]
pub fn is_rip_output(name: &str) -> bool {
    RIP_OUTPUTS.contains(&name)
}

/// The largest a file of this name may be, when it is one a rip may send.
#[must_use]
pub fn max_output_bytes(name: &str) -> Option<u64> {
    match name {
        "disc.toc" => Some(MAX_TOC_BYTES),
        "disc.bin" => Some(MAX_CD_DATA_BYTES),
        IMAGE_OUTPUT => Some(MAX_IMAGE_BYTES),
        _ => None,
    }
}

/// Whether `names` make up one whole rip: a CD's table of contents and data,
/// or one disc image. Never a mixture, and never a part.
#[must_use]
pub fn is_complete_output_set(names: &[String]) -> bool {
    let mut names: Vec<&str> = names.iter().map(String::as_str).collect();
    names.sort_unstable();
    names.dedup();
    names == ["disc.bin", "disc.toc"] || names == [IMAGE_OUTPUT]
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use RipState::{Canceled, Complete, Failed, Queued, Reading, Refused, Registering, Uploading};

    #[test]
    fn a_rip_runs_from_queued_to_complete() {
        assert_eq!(Queued.transition_to(Reading), Ok(Reading));
        assert_eq!(Reading.transition_to(Uploading), Ok(Uploading));
        assert_eq!(Uploading.transition_to(Registering), Ok(Registering));
        assert_eq!(Registering.transition_to(Complete), Ok(Complete));
    }

    #[test]
    fn nothing_is_skipped() {
        assert!(Queued.transition_to(Uploading).is_err());
        assert!(
            Reading.transition_to(Registering).is_err(),
            "not without sending"
        );
        assert!(
            Uploading.transition_to(Complete).is_err(),
            "not without importing"
        );
    }

    #[test]
    fn reading_is_harmless_to_stop() {
        for state in [Queued, Reading, Uploading] {
            assert!(state.can_cancel());
            assert_eq!(state.transition_to(Canceled), Ok(Canceled));
        }
        assert!(!Registering.can_cancel());
        assert!(Registering.transition_to(Canceled).is_err());
    }

    #[test]
    fn the_worker_can_refuse_or_fail_until_it_has_sent_everything() {
        for state in [Queued, Reading, Uploading] {
            assert_eq!(state.transition_to(Refused), Ok(Refused));
            assert_eq!(state.transition_to(Failed), Ok(Failed));
        }
        assert!(Registering.transition_to(Refused).is_err());
        assert_eq!(
            Registering.transition_to(Failed),
            Ok(Failed),
            "the import failed"
        );
    }

    #[test]
    fn every_ending_is_final() {
        for end in [Complete, Canceled, Refused, Failed] {
            assert!(end.is_terminal());
            for next in RipState::all() {
                assert!(end.transition_to(*next).is_err(), "{end} -> {next}");
            }
        }
    }

    #[test]
    fn only_reading_and_sending_hold_the_drive() {
        for state in RipState::all() {
            assert_eq!(
                state.holds_the_drive(),
                matches!(state, Reading | Uploading),
                "{state}"
            );
            assert_eq!(state.accepts_uploads(), state.holds_the_drive());
        }
    }

    #[test]
    fn a_rip_is_a_cd_pair_or_one_image() {
        let set = |names: &[&str]| {
            is_complete_output_set(&names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>())
        };
        assert!(set(&["disc.toc", "disc.bin"]));
        assert!(set(&["disc.bin", "disc.toc", "disc.toc"]));
        assert!(set(&["disc.iso"]));
        for wrong in [
            &[][..],
            &["disc.toc"][..],
            &["disc.iso", "disc.toc"][..],
            &["disc.iso", "disc.toc", "disc.bin"][..],
            &["other.iso"][..],
        ] {
            assert!(!set(wrong), "{wrong:?}");
        }
        assert_eq!(max_output_bytes("disc.toc"), Some(MAX_TOC_BYTES));
        assert_eq!(max_output_bytes("disc.iso"), Some(MAX_IMAGE_BYTES));
        assert_eq!(max_output_bytes("disc.img"), None);
        assert!(is_rip_output("disc.iso") && !is_cd_output("disc.iso"));
    }

    #[test]
    fn a_cd_rip_sends_only_its_two_files() {
        assert!(is_cd_output("disc.toc"));
        assert!(is_cd_output("disc.bin"));
        for other in ["disc.iso", "../disc.bin", "DISC.BIN", "disc.toc/x", ""] {
            assert!(!is_cd_output(other), "{other}");
        }
    }
}
