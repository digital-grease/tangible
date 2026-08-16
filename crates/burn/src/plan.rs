// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Burn plans, drive observations, and the reports an engine produces.
//!
//! A plan is a fully validated description of one write. It carries no
//! command-line text: engines convert a plan into their own fixed argument
//! array, so nothing a user or a filename supplies can become an argument.
//!
//! The report types are deliberately separate. Structural validity, burn
//! support, write success, read-back verification, and target compatibility
//! are five different claims, and collapsing any of them into "it worked" is
//! how a tool ends up promising something it never checked.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tangible_domain::{DriveId, Sha256Digest, WorkerId};

/// Which drive an operation targets.
///
/// Identity is the worker plus the configured drive, never a device node. The
/// same `/dev/sr0` is a different drive on a different machine, and can even
/// be a different drive on the same machine after a reboot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveRef {
    /// The worker owning the drive.
    pub worker_id: WorkerId,
    /// The drive.
    pub drive_id: DriveId,
    /// Worker-local stable alias, e.g. `/dev/disc-block`.
    ///
    /// An alias rather than the host path, so worker configuration is
    /// identical across machines where enumeration differs.
    pub device_alias: String,
}

/// What a drive can do.
///
/// Observations, not guarantees: a drive reporting a capability may still fail
/// to use it, which is why preflight checks the medium rather than trusting
/// this alone.
// The shape is dictated by the domain model, which lists these as separate
// independent capabilities. Grouping them into an enum would misrepresent
// drives that support some and not others.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveCapabilities {
    /// Media profiles the drive can read.
    pub read_profiles: Vec<String>,
    /// Media profiles the drive can write.
    pub write_profiles: Vec<String>,
    /// Whether a simulated write is available.
    pub supports_test_write: bool,
    /// Whether buffer-underrun protection is available.
    pub supports_buffer_underrun_protection: bool,
    /// Whether CD-TEXT can be written.
    pub supports_cd_text: bool,
    /// Whether raw disc-at-once is available.
    pub supports_raw_dao: bool,
    /// Whether subchannel R-W can be written.
    pub supports_subchannel_rw: bool,
    /// Maximum write speed per profile.
    pub max_write_speed_by_profile: BTreeMap<String, String>,
    /// What each engine reported, kept so a disagreement is visible.
    pub engine_evidence: BTreeMap<String, String>,
}

/// What is actually in the drive.
// As above: these describe independent observed properties of a medium.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediumInfo {
    /// Media profile, e.g. `CD-R`.
    pub profile: String,
    /// Whether the medium is blank.
    pub blank: bool,
    /// Whether it can be written at all.
    pub writable: bool,
    /// Whether it can be erased and rewritten.
    pub rewritable: bool,
    /// Total capacity in blocks.
    pub capacity_blocks: u64,
    /// Blocks still free.
    pub free_blocks: u64,
    /// Block size.
    pub block_size: u32,
    /// Manufacturer identifier, when the drive reports one.
    pub manufacturer_id: Option<String>,
    /// Sessions already present.
    pub sessions: u32,
    /// Whether the medium can be erased.
    pub erasable: bool,
    /// Anything the drive flagged.
    pub status_warnings: Vec<String>,
}

impl MediumInfo {
    /// Free capacity in bytes.
    #[must_use]
    pub const fn free_bytes(&self) -> u64 {
        self.free_blocks.saturating_mul(self.block_size as u64)
    }
}

/// One file the engine will write, already resolved and verified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedInput {
    /// Local path in the worker's staging area.
    pub staged_path: PathBuf,
    /// Digest the file must have.
    ///
    /// Present so the worker can re-verify immediately before writing. Bytes
    /// staged an hour ago may not be the bytes on disk now.
    pub sha256: Sha256Digest,
    /// Length in bytes.
    pub length_bytes: u64,
}

/// What kind of write to perform.
///
/// A closed set. There is no "other" variant carrying arbitrary options,
/// because that is how a command-line escape hatch gets reintroduced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    /// A single data track written disc-at-once.
    DataDiscAtOnce,
    /// A single data track written track-at-once.
    DataTrackAtOnce,
    /// A full TOC written disc-at-once, for audio and mixed mode.
    TocDiscAtOnce,
}

/// A validated description of one write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BurnPlan {
    /// The attempt this plan belongs to.
    pub attempt_id: tangible_domain::BurnAttemptId,
    /// Target drive.
    pub drive: DriveRef,
    /// Files to write, in order.
    pub inputs: Vec<PlannedInput>,
    /// How to write them.
    pub mode: WriteMode,
    /// Media profiles this plan is valid for.
    pub accepted_profiles: Vec<String>,
    /// Requested speed, or none for the drive's choice.
    ///
    /// A label rather than a number: engines express speed differently, and a
    /// validated label cannot become an arbitrary argument.
    pub speed: Option<String>,
    /// Whether to close the disc.
    pub finalize: bool,
    /// Whether to eject afterwards.
    pub eject_on_success: bool,
    /// Total bytes the plan will write.
    pub total_bytes: u64,
}

impl BurnPlan {
    /// Whether a medium is large enough.
    ///
    /// Overburning is not offered, so a plan that does not fit simply does not
    /// proceed.
    #[must_use]
    pub const fn fits_on(&self, medium: &MediumInfo) -> bool {
        self.total_bytes <= medium.free_bytes()
    }
}

/// A reason a plan cannot proceed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum PreflightFailure {
    /// No medium is present.
    NoMedium,
    /// The medium cannot be written.
    MediumNotWritable {
        /// The profile observed.
        profile: String,
    },
    /// The medium already holds data and the plan does not erase it.
    MediumNotBlank {
        /// Sessions already present.
        sessions: u32,
    },
    /// The medium is not one this plan accepts.
    ProfileNotAccepted {
        /// Profile in the drive.
        found: String,
        /// Profiles the plan accepts.
        accepted: Vec<String>,
    },
    /// The plan is larger than the medium.
    ///
    /// Reported rather than worked around: overburning is not offered.
    InsufficientCapacity {
        /// Bytes required.
        required_bytes: u64,
        /// Bytes available.
        available_bytes: u64,
    },
    /// The drive cannot write this profile.
    DriveCannotWriteProfile {
        /// The profile needed.
        profile: String,
    },
    /// A staged input no longer matches its recorded digest.
    ///
    /// The check that makes "hash verified before write" real rather than
    /// aspirational.
    InputDigestMismatch {
        /// The input.
        path: String,
        /// Digest recorded in the plan.
        expected: String,
        /// Digest the bytes produce now.
        actual: String,
    },
    /// A staged input is missing.
    InputMissing {
        /// The input.
        path: String,
    },
}

/// The outcome of preflight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreflightReport {
    /// Blocking reasons. Empty means the write may proceed.
    pub failures: Vec<PreflightFailure>,
    /// Non-blocking observations the operator should see.
    pub warnings: Vec<String>,
    /// What was in the drive when preflight ran.
    pub medium: Option<MediumInfo>,
}

impl PreflightReport {
    /// Whether the write may proceed.
    #[must_use]
    pub const fn passed(&self) -> bool {
        self.failures.is_empty()
    }
}

/// The outcome of a write.
///
/// Note what this does *not* say: nothing here means the disc is correct. A
/// successful write is a claim about the engine exiting cleanly, and
/// verification is a separate operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteReport {
    /// Whether the engine reported success.
    pub engine_reported_success: bool,
    /// Bytes written.
    pub bytes_written: u64,
    /// Engine name.
    pub engine: String,
    /// Engine version.
    pub engine_version: String,
    /// Whether the disc was closed.
    pub finalized: bool,
    /// Engine-reported diagnostics, bounded.
    pub diagnostics: Vec<String>,
}

/// The outcome of reading the medium back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyReport {
    /// Whether every byte read back matched.
    pub matched: bool,
    /// Bytes compared.
    pub bytes_compared: u64,
    /// Which verification was performed, so the claim is precise.
    pub method: String,
    /// Where the first difference was, when there was one.
    pub first_mismatch_offset: Option<u64>,
    /// What verification did not cover.
    ///
    /// Always populated. A verification result that does not state its limits
    /// invites being read as a stronger guarantee than it is.
    pub limitations: Vec<String>,
}

/// A request to erase a rewritable medium.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlankRequest {
    /// Target drive.
    pub drive: DriveRef,
    /// Whether to erase fully rather than just the table of contents.
    pub full: bool,
    /// Explicit confirmation that data loss is intended.
    ///
    /// A separate field rather than an implied consequence, because this
    /// destroys whatever is in the drive.
    pub confirmed_destructive: bool,
}

/// The outcome of an erase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlankReport {
    /// Whether the medium was erased.
    pub erased: bool,
    /// How long it took, in seconds.
    pub duration_seconds: u64,
    /// Engine diagnostics.
    pub diagnostics: Vec<String>,
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn medium(free_blocks: u64) -> MediumInfo {
        MediumInfo {
            profile: "CD-R".to_owned(),
            blank: true,
            writable: true,
            rewritable: false,
            capacity_blocks: free_blocks,
            free_blocks,
            block_size: 2048,
            manufacturer_id: None,
            sessions: 0,
            erasable: false,
            status_warnings: vec![],
        }
    }

    fn plan(total_bytes: u64) -> BurnPlan {
        BurnPlan {
            attempt_id: tangible_domain::BurnAttemptId::generate(),
            drive: DriveRef {
                worker_id: WorkerId::generate(),
                drive_id: DriveId::generate(),
                device_alias: "/dev/disc-block".to_owned(),
            },
            inputs: vec![],
            mode: WriteMode::DataDiscAtOnce,
            accepted_profiles: vec!["CD-R".to_owned()],
            speed: None,
            finalize: true,
            eject_on_success: true,
            total_bytes,
        }
    }

    #[test]
    fn free_capacity_is_blocks_times_block_size() {
        assert_eq!(medium(100).free_bytes(), 100 * 2048);
    }

    #[test]
    fn a_plan_that_exactly_fills_the_medium_fits() {
        assert!(plan(100 * 2048).fits_on(&medium(100)));
    }

    #[test]
    fn a_plan_one_byte_too_large_does_not_fit() {
        // Overburning is not offered, so "nearly fits" is "does not fit".
        assert!(!plan(100 * 2048 + 1).fits_on(&medium(100)));
    }

    #[test]
    fn capacity_arithmetic_saturates_rather_than_wrapping() {
        let mut huge = medium(u64::MAX);
        huge.block_size = u32::MAX;
        // Must not panic or wrap into a small number that would let an
        // oversized plan appear to fit.
        let _ = huge.free_bytes();
        assert!(plan(u64::MAX).fits_on(&huge));
    }

    #[test]
    fn preflight_passes_only_with_no_failures() {
        let clean = PreflightReport {
            failures: vec![],
            warnings: vec!["nonstandard capacity".to_owned()],
            medium: None,
        };
        assert!(clean.passed(), "warnings alone must not block a write");

        let blocked = PreflightReport {
            failures: vec![PreflightFailure::NoMedium],
            warnings: vec![],
            medium: None,
        };
        assert!(!blocked.passed());
    }

    #[test]
    fn reports_round_trip_through_serde() {
        // They are persisted on the burn attempt row.
        let report = PreflightReport {
            failures: vec![PreflightFailure::InsufficientCapacity {
                required_bytes: 10,
                available_bytes: 5,
            }],
            warnings: vec![],
            medium: Some(medium(1)),
        };
        let json = serde_json::to_string(&report).expect("serialize");
        assert_eq!(
            serde_json::from_str::<PreflightReport>(&json).expect("deserialize"),
            report
        );
    }

    #[test]
    fn a_write_mode_is_a_closed_set() {
        // No variant carries free-form options, which is what keeps a
        // command-line escape hatch from reappearing.
        let json = serde_json::to_string(&WriteMode::TocDiscAtOnce).expect("serialize");
        assert_eq!(json, "\"toc_disc_at_once\"");
        assert!(serde_json::from_str::<WriteMode>("\"raw_passthrough\"").is_err());
    }
}
