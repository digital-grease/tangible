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
use tangible_domain::cd;
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

/// One track of a CD layout, as the plan carries it.
///
/// The quantities mean what they mean in the manifest this was built from:
/// `start_lba` is the image-relative disc position of the first sector present
/// in the file, `sector_count` is how many sectors of the track that file
/// holds, and `pregap_sectors` is the whole gap before INDEX 01 whether the
/// burner generates it or the file carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedTrack {
    /// Track number, one-based.
    pub number: u32,
    /// Session the track belongs to.
    pub session: u32,
    /// Mode as the descriptor wrote it, e.g. `AUDIO` or `MODE2/2352`.
    pub mode: String,
    /// Which of the plan's inputs holds the bytes, by index.
    pub input: usize,
    /// Byte offset within that input.
    pub file_offset_bytes: u64,
    /// Image-relative disc position of the first sector present.
    pub start_lba: u64,
    /// Sectors of the track the input holds.
    pub sector_count: u64,
    /// The whole gap before INDEX 01.
    pub pregap_sectors: u64,
    /// Index points, as LBAs relative to the first sector present.
    ///
    /// Carried because a writer has to tell a gap the burner generates from
    /// one the file holds, and the presence of an INDEX 00 is what says which
    /// this is. The combined [`PlannedTrack::pregap_sectors`] cannot answer
    /// that on its own.
    #[serde(default)]
    pub indexes: Vec<PlannedIndex>,
    /// International Standard Recording Code to record on the track.
    #[serde(default)]
    pub isrc: Option<String>,
    /// How the input stores audio samples, when the descriptor said.
    ///
    /// An audio track without one is refused before media is asked for: the
    /// two answers produce a correct disc and a disc of static, and nothing in
    /// the bytes says which is which.
    #[serde(default)]
    pub sample_byte_order: Option<cd::SampleByteOrder>,
    /// Subcode flags to record on the track.
    #[serde(default)]
    pub flags: Vec<cd::TrackFlag>,
}

/// One index point within a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedIndex {
    /// Index number. 0 is the pregap, 1 is the track proper.
    pub number: u32,
    /// LBA relative to the track's first present sector.
    pub relative_lba: u64,
}

impl PlannedTrack {
    /// Bytes this track occupies in its input, when the mode is known.
    #[must_use]
    pub fn byte_length(&self) -> Option<u64> {
        cd::sector_bytes(&self.mode).map(|bytes| self.sector_count.saturating_mul(u64::from(bytes)))
    }

    /// Whether the track carries audio.
    #[must_use]
    pub fn is_audio(&self) -> bool {
        cd::is_audio(&self.mode)
    }

    /// Sectors of gap the file itself carries, before INDEX 01.
    ///
    /// An INDEX 00 says the gap is in the file. Without one there is nothing
    /// in the file before the track proper, whatever gap the disc may have.
    #[must_use]
    pub fn in_file_pregap(&self) -> u64 {
        if !self.indexes.iter().any(|index| index.number == 0) {
            return 0;
        }
        self.indexes
            .iter()
            .find(|index| index.number == 1)
            .map_or(0, |index| index.relative_lba)
    }

    /// Sectors of gap the writer has to generate, which are in no file.
    #[must_use]
    pub fn generated_pregap(&self) -> u64 {
        self.pregap_sectors.saturating_sub(self.in_file_pregap())
    }

    /// Sectors from INDEX 01 to the end of the track: the track proper.
    ///
    /// What the four-second minimum is measured against. A gap the file
    /// carries is inside [`PlannedTrack::sector_count`] and is not part of it.
    #[must_use]
    pub fn sectors_after_index_one(&self) -> u64 {
        self.sector_count.saturating_sub(self.in_file_pregap())
    }
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
    /// Tracks, for a disc described as tracks rather than as one image.
    ///
    /// Empty for a block image, which is the shape an ISO has: one input, one
    /// track, nothing to say about it that the input does not already say.
    #[serde(default)]
    pub tracks: Vec<PlannedTrack>,
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
    /// Media catalogue number to record on the disc, when there is one.
    #[serde(default)]
    pub catalog: Option<String>,
}

impl BurnPlan {
    /// Whether a medium is large enough.
    ///
    /// Overburning is not offered, so a plan that does not fit simply does not
    /// proceed.
    ///
    /// Bytes, which is the right unit for a block image and the wrong one for
    /// a track layout: a raw 2352 byte sector on a disc whose profile reports
    /// 2048 byte blocks would make a full CD look oversized by a sixth. Use
    /// [`BurnPlan::capacity_failures`], which asks the right question for the
    /// shape it is given.
    #[must_use]
    pub const fn fits_on(&self, medium: &MediumInfo) -> bool {
        self.total_bytes <= medium.free_bytes()
    }

    /// Whether this plan describes a disc as tracks.
    #[must_use]
    pub fn is_track_layout(&self) -> bool {
        !self.tracks.is_empty()
    }

    /// Sectors the layout occupies, generated pregaps included.
    ///
    /// The gap a PREGAP command asks for is not in any file and still takes up
    /// the disc, so a capacity check that counted only file bytes would
    /// under-count a mixed-mode disc by two seconds per audio track.
    ///
    /// Only the generated part is added. A gap the file carries is already in
    /// the track's sectors, and adding the whole of `pregap_sectors` counted it
    /// twice. cdrdao's own `toc-size` is what showed that: for a track whose
    /// file carries its gap it reports the sectors in the file and no more.
    #[must_use]
    pub fn required_sectors(&self) -> u64 {
        self.tracks
            .iter()
            .map(|track| track.sector_count.saturating_add(track.generated_pregap()))
            .fold(0, u64::saturating_add)
    }

    /// Checks that hold whatever is in the drive.
    ///
    /// Run before an operator is asked for a disc. A layout that could never
    /// be written should not spend half an hour waiting for media first, and
    /// none of these answers change when a disc appears.
    #[must_use]
    pub fn check_layout(&self) -> Vec<PreflightFailure> {
        let mut failures = Vec::new();
        if !self.is_track_layout() {
            return failures;
        }

        if self.tracks.len() > cd::MAX_TRACKS as usize {
            failures.push(PreflightFailure::TooManyTracks {
                count: self.tracks.len(),
            });
        }

        let sessions = self
            .tracks
            .iter()
            .map(|track| track.session)
            .max()
            .unwrap_or(1);
        if sessions > 1 {
            failures.push(PreflightFailure::MultisessionUnsupported { sessions });
        }

        let required_sectors = self.required_sectors();
        if required_sectors > cd::MAX_SECTORS {
            failures.push(PreflightFailure::ExceedsCdCapacity {
                required_sectors,
                available_sectors: cd::MAX_SECTORS,
            });
        }

        for track in &self.tracks {
            // Measured from INDEX 01, as the format measures it. A writer
            // refuses a shorter track, so asking for a disc first would only
            // waste somebody's time.
            if track.sectors_after_index_one() < cd::MIN_TRACK_SECTORS {
                failures.push(PreflightFailure::TrackTooShort {
                    track: track.number,
                    sectors: track.sectors_after_index_one(),
                });
            }
            if track.is_audio() && track.sample_byte_order.is_none() {
                failures.push(PreflightFailure::AudioByteOrderUnknown {
                    track: track.number,
                });
            }
            let Some(byte_length) = track.byte_length() else {
                failures.push(PreflightFailure::UnknownTrackMode {
                    track: track.number,
                    mode: track.mode.clone(),
                });
                continue;
            };
            let Some(input) = self.inputs.get(track.input) else {
                failures.push(PreflightFailure::TrackInputMissing {
                    track: track.number,
                });
                continue;
            };
            // The offset and the length are both from the descriptor and the
            // length is from the file. A disagreement means the disc being
            // described is not the disc being carried.
            let needs_bytes = track.file_offset_bytes.saturating_add(byte_length);
            if needs_bytes > input.length_bytes {
                failures.push(PreflightFailure::TrackOutsideInput {
                    track: track.number,
                    needs_bytes,
                    input_bytes: input.length_bytes,
                });
            }
        }

        failures
    }

    /// Capacity checks against the medium actually in the drive.
    ///
    /// Sectors for a track layout and bytes for a block image, because those
    /// are the units each is written in.
    #[must_use]
    pub fn capacity_failures(&self, medium: &MediumInfo) -> Vec<PreflightFailure> {
        let mut failures = Vec::new();
        if !self.is_track_layout() {
            if !self.fits_on(medium) {
                failures.push(PreflightFailure::InsufficientCapacity {
                    required_bytes: self.total_bytes,
                    available_bytes: medium.free_bytes(),
                });
            }
            return failures;
        }

        if !cd::is_cd_profile(&medium.profile) {
            failures.push(PreflightFailure::TrackLayoutNeedsACd {
                found: medium.profile.clone(),
            });
        }

        // A CD's block count is its sector count whatever user-data size the
        // profile reports per block, so this comparison is in the same units
        // on both sides.
        let required_sectors = self.required_sectors();
        if required_sectors > medium.free_blocks {
            failures.push(PreflightFailure::InsufficientCapacity {
                required_bytes: self.total_bytes,
                available_bytes: medium.free_bytes(),
            });
        }

        failures
    }

    /// Observations worth putting in front of an operator before a burn.
    ///
    /// Not failures. Each of these describes a disc that will be written and
    /// will not be quite the disc the descriptor came from, which is a thing
    /// to know beforehand rather than to discover afterwards.
    #[must_use]
    pub fn layout_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if !self.is_track_layout() {
            return warnings;
        }

        if self.tracks.iter().any(PlannedTrack::is_audio) {
            // Said here because preflight is the last moment before
            // media is consumed, and it is the operator's decision to make.
            warnings.push(
                "audio tracks are verified by length and error reporting rather than by \
                 comparing bytes: without a per-drive offset a byte comparison fails on \
                 correct hardware"
                    .to_owned(),
            );
        }

        // A CUE/BIN pair cannot carry subchannel data, so anything that lived
        // there is not on the disc this writes.
        warnings.push(
            "a disc written from a track descriptor carries no subchannel data, so CD+G \
             graphics and subchannel-based protections are not reproduced"
                .to_owned(),
        );

        let required_sectors = self.required_sectors();
        if required_sectors > cd::REDBOOK_SECTORS {
            warnings.push(format!(
                "the layout needs {required_sectors} sectors, more than the {} of a 74 minute \
                 disc; it needs 80 minute media",
                cd::REDBOOK_SECTORS
            ));
        }

        warnings
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
    /// A track names a mode nothing here knows the sector size of.
    ///
    /// Without a sector size there is no way to say where the track's bytes
    /// are, and writing it would be writing an offset nobody computed.
    UnknownTrackMode {
        /// The track.
        track: u32,
        /// The mode as the descriptor wrote it.
        mode: String,
    },
    /// A track's bytes are not inside the file that is supposed to hold them.
    ///
    /// A topology and its components disagreeing, which is how a burn comes to
    /// write whatever happened to be at that offset.
    TrackOutsideInput {
        /// The track.
        track: u32,
        /// Where the track ends, in bytes into the file.
        needs_bytes: u64,
        /// How long the file actually is.
        input_bytes: u64,
    },
    /// A track refers to an input the plan does not carry.
    TrackInputMissing {
        /// The track.
        track: u32,
    },
    /// More tracks than a CD can hold.
    TooManyTracks {
        /// How many were planned.
        count: usize,
    },
    /// The layout spans more than one session.
    ///
    /// Multisession writing is a capability this project has not built. The
    /// disc would be written as one session, which is a different disc from
    /// the one the manifest describes.
    MultisessionUnsupported {
        /// How many sessions the layout spans.
        sessions: u32,
    },
    /// The layout is longer than a CD.
    ///
    /// Separate from [`PreflightFailure::InsufficientCapacity`], which is
    /// about the disc that happens to be in the drive. This one is about every
    /// disc there is.
    ExceedsCdCapacity {
        /// Sectors the layout needs.
        required_sectors: u64,
        /// Sectors the largest CD this project will write holds.
        available_sectors: u64,
    },
    /// The engine cannot write this shape of disc.
    ///
    /// The check that stops a mixed-mode CD being handed to an engine that
    /// would flatten it into a single data track.
    WriteModeUnsupported {
        /// What the plan needs.
        mode: WriteMode,
        /// The engine that cannot do it.
        engine: String,
    },
    /// A track layout on a medium that is not a CD.
    TrackLayoutNeedsACd {
        /// The profile in the drive.
        found: String,
    },
    /// A track shorter than the four seconds a CD track must last.
    TrackTooShort {
        /// The track.
        track: u32,
        /// Sectors from INDEX 01 to its end.
        sectors: u64,
    },
    /// An audio track whose file does not say how it stores samples.
    ///
    /// Refused rather than assumed: one answer is a disc and the other is
    /// static, and the bytes do not say which.
    AudioByteOrderUnknown {
        /// The track.
        track: u32,
    },
    /// The table of contents the engine writes from could not be made, or the
    /// writing tool objected to it.
    ///
    /// Specific to an engine that writes from such a document, which is why it
    /// is not among the checks every layout gets.
    TableOfContentsRefused {
        /// Why, in the words of whichever refused it. Not `reason`, which is
        /// the tag this enum is serialized under.
        cause: String,
    },
    /// The writing tool read the table of contents differently from the plan.
    ///
    /// The last check before media is consumed that the tool will write what
    /// was meant, made by asking the tool itself rather than by trusting the
    /// document it was handed.
    TableOfContentsDisagrees {
        /// What disagreed.
        detail: String,
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
    /// What was checked on each track, for a disc described as tracks.
    ///
    /// Empty for a block image, which is compared as one extent. For a track
    /// layout this is the result: which tracks were compared byte for byte,
    /// which could only be checked for length and readability, and how each
    /// came out.
    #[serde(default)]
    pub tracks: Vec<TrackVerification>,
}

impl VerifyReport {
    /// Whether everything that was checked passed, but some of it was only
    /// checked for length and readability rather than compared.
    ///
    /// Recorded as a `partial` result rather than a pass: a record never claims
    /// more than was checked, and a length check is not a comparison.
    #[must_use]
    pub fn is_partial(&self) -> bool {
        self.matched
            && self
                .tracks
                .iter()
                .any(|track| track.check == TrackCheck::LengthAndReadable)
    }
}

/// How one track was checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackCheck {
    /// Every byte read back and compared with what was written.
    ByteCompare,
    /// Read back without an uncorrected error, and found to be the length the
    /// layout says, to the sector. Not compared: audio, because without the
    /// drive's read offset a comparison fails on correct hardware, and raw
    /// data tracks, until raw sector reads have been proven on a drive.
    LengthAndReadable,
}

/// How one track's check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackOutcome {
    /// It passed.
    Match,
    /// It was read and was wrong: different bytes, or a different length or
    /// position from the layout's.
    Mismatch,
    /// It could not be read back cleanly.
    Unreadable,
}

/// The check on one track.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackVerification {
    /// Track number.
    pub number: u32,
    /// How it was checked.
    pub check: TrackCheck,
    /// How that came out.
    pub outcome: TrackOutcome,
    /// Sectors the layout says the track proper holds.
    pub sectors_expected: u64,
    /// Sectors found on the disc.
    pub sectors_read: u64,
    /// The digest of what was written, for a byte comparison.
    #[serde(default)]
    pub expected_sha256: Option<String>,
    /// The digest of what was read back, for a byte comparison.
    #[serde(default)]
    pub observed_sha256: Option<String>,
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
            tracks: Vec::new(),
            catalog: None,
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

    // --- track layouts ---------------------------------------------------------

    /// A plan for `sizes` inputs and the tracks laid over them.
    fn track_plan(sizes: &[u64], tracks: Vec<PlannedTrack>) -> BurnPlan {
        let mut built = plan(sizes.iter().copied().fold(0, u64::saturating_add));
        built.inputs = sizes
            .iter()
            .map(|length| PlannedInput {
                staged_path: PathBuf::from("/staged/track.bin"),
                sha256: Sha256Digest::from_bytes([0; 32]),
                length_bytes: *length,
            })
            .collect();
        built.mode = WriteMode::TocDiscAtOnce;
        built.tracks = tracks;
        built
    }

    fn track(number: u32, mode: &str, sector_count: u64) -> PlannedTrack {
        PlannedTrack {
            number,
            session: 1,
            mode: mode.to_owned(),
            input: 0,
            file_offset_bytes: 0,
            start_lba: 0,
            sector_count,
            pregap_sectors: 0,
            indexes: vec![PlannedIndex {
                number: 1,
                relative_lba: 0,
            }],
            isrc: None,
            sample_byte_order: Some(cd::SampleByteOrder::LittleEndian),
            flags: Vec::new(),
        }
    }

    #[test]
    fn a_block_image_has_no_layout_to_object_to() {
        assert!(plan(100).check_layout().is_empty());
        assert!(plan(100).layout_warnings().is_empty());
        assert_eq!(plan(100).required_sectors(), 0);
    }

    #[test]
    fn a_coherent_layout_passes() {
        let built = track_plan(&[2352 * 300], vec![track(1, "AUDIO", 300)]);

        assert!(
            built.check_layout().is_empty(),
            "{:?}",
            built.check_layout()
        );
        assert_eq!(built.required_sectors(), 300);
    }

    #[test]
    fn a_track_that_runs_past_its_file_is_refused() {
        // A topology and its components disagreeing. Writing it would put
        // whatever happened to be at that offset onto a disc.
        let built = track_plan(&[2352 * 150], vec![track(1, "AUDIO", 300)]);

        assert!(matches!(
            built.check_layout().as_slice(),
            [PreflightFailure::TrackOutsideInput { track: 1, .. }]
        ));
    }

    #[test]
    fn a_mode_with_no_known_sector_size_is_refused() {
        let built = track_plan(&[2352 * 300], vec![track(1, "MODE9/9999", 300)]);

        assert!(matches!(
            built.check_layout().as_slice(),
            [PreflightFailure::UnknownTrackMode { track: 1, .. }]
        ));
    }

    #[test]
    fn a_track_naming_an_input_the_plan_does_not_have_is_refused() {
        let mut orphan = track(1, "AUDIO", 300);
        orphan.input = 7;
        let built = track_plan(&[2352 * 300], vec![orphan]);

        assert!(matches!(
            built.check_layout().as_slice(),
            [PreflightFailure::TrackInputMissing { track: 1 }]
        ));
    }

    #[test]
    fn more_tracks_than_a_cd_holds_is_refused() {
        let tracks = (1..=100).map(|n| track(n, "AUDIO", 1)).collect();
        let built = track_plan(&[2352 * 1000], tracks);

        assert!(
            built
                .check_layout()
                .iter()
                .any(|failure| matches!(failure, PreflightFailure::TooManyTracks { count: 100 }))
        );
    }

    #[test]
    fn a_layout_spanning_sessions_is_refused_rather_than_flattened() {
        // Written as one session it would be a different disc from the one the
        // manifest describes, and it would look like it worked.
        let mut second = track(2, "AUDIO", 10);
        second.session = 2;
        let built = track_plan(&[2352 * 100], vec![track(1, "AUDIO", 10), second]);

        assert!(built.check_layout().iter().any(|failure| matches!(
            failure,
            PreflightFailure::MultisessionUnsupported { sessions: 2 }
        )));
    }

    #[test]
    fn a_layout_longer_than_any_cd_is_refused_before_a_disc_is_asked_for() {
        let built = track_plan(
            &[2352 * (cd::MAX_SECTORS + 10)],
            vec![track(1, "AUDIO", cd::MAX_SECTORS + 10)],
        );

        assert!(
            built
                .check_layout()
                .iter()
                .any(|failure| matches!(failure, PreflightFailure::ExceedsCdCapacity { .. }))
        );
    }

    #[test]
    fn a_generated_pregap_takes_up_the_disc_without_taking_up_a_file() {
        let mut second = track(2, "AUDIO", 10);
        second.pregap_sectors = 150;
        let built = track_plan(&[2352 * 100], vec![track(1, "AUDIO", 10), second]);

        assert_eq!(built.required_sectors(), 170);
    }

    #[test]
    fn a_gap_the_file_carries_is_not_counted_twice() {
        // The track's sectors already include a gap its file carries. Adding
        // the whole pregap on top claimed two seconds per track that the disc
        // does not use; cdrdao's toc-size, run on the same layout, is what
        // said so.
        let mut audio = track(2, "AUDIO", 3000);
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
        ];
        let built = track_plan(&[2352 * 4000], vec![track(1, "MODE1/2352", 1000), audio]);

        assert_eq!(built.required_sectors(), 4000);
    }

    #[test]
    fn a_track_shorter_than_four_seconds_is_refused_before_a_disc_is_asked_for() {
        // A writer refuses it, so a disc would only be asked for and wasted.
        let built = track_plan(&[2352 * 299], vec![track(1, "AUDIO", 299)]);

        assert!(built.check_layout().iter().any(|failure| matches!(
            failure,
            PreflightFailure::TrackTooShort {
                track: 1,
                sectors: 299
            }
        )));
    }

    #[test]
    fn four_seconds_are_counted_from_index_one() {
        // The format measures the track proper. Three hundred sectors in the
        // file of which a hundred and fifty are its gap is two seconds of
        // track, and too short.
        let mut audio = track(1, "AUDIO", 300);
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
        ];
        let built = track_plan(&[2352 * 300], vec![audio]);

        assert!(built.check_layout().iter().any(|failure| matches!(
            failure,
            PreflightFailure::TrackTooShort {
                track: 1,
                sectors: 150
            }
        )));
    }

    #[test]
    fn an_audio_track_whose_byte_order_nobody_recorded_is_refused() {
        // One answer is a disc and the other is static.
        let mut audio = track(1, "AUDIO", 300);
        audio.sample_byte_order = None;
        let built = track_plan(&[2352 * 300], vec![audio]);

        assert!(matches!(
            built.check_layout().as_slice(),
            [PreflightFailure::AudioByteOrderUnknown { track: 1 }]
        ));
    }

    #[test]
    fn a_data_track_needs_no_byte_order() {
        // Data sectors are bytes, not samples.
        let mut data = track(1, "MODE1/2352", 300);
        data.sample_byte_order = None;
        let built = track_plan(&[2352 * 300], vec![data]);

        assert!(
            built.check_layout().is_empty(),
            "{:?}",
            built.check_layout()
        );
    }

    #[test]
    fn a_full_cd_of_raw_sectors_is_not_called_oversized() {
        // The regression this check exists for. A raw 2352 byte sector on a
        // medium that reports 2048 byte blocks makes a full disc look a sixth
        // too big, and a byte comparison would refuse to write a disc that
        // fits perfectly well.
        let sectors = 300_000;
        let built = track_plan(&[2352 * sectors], vec![track(1, "AUDIO", sectors)]);

        assert!(
            !built.fits_on(&medium(sectors)),
            "the byte comparison is wrong"
        );
        assert!(
            built.capacity_failures(&medium(sectors)).is_empty(),
            "the sector comparison is right"
        );
    }

    #[test]
    fn a_layout_that_does_not_fit_the_disc_in_the_drive_is_still_refused() {
        let built = track_plan(&[2352 * 1000], vec![track(1, "AUDIO", 1000)]);

        assert!(
            built.capacity_failures(&medium(500)).iter().any(|failure| {
                matches!(failure, PreflightFailure::InsufficientCapacity { .. })
            })
        );
    }

    #[test]
    fn a_track_layout_will_not_go_on_a_dvd() {
        let built = track_plan(&[2352 * 10], vec![track(1, "AUDIO", 10)]);
        let mut dvd = medium(1_000_000);
        dvd.profile = "DVD-R".to_owned();

        assert!(
            built
                .capacity_failures(&dvd)
                .iter()
                .any(|failure| { matches!(failure, PreflightFailure::TrackLayoutNeedsACd { .. }) })
        );
    }

    #[test]
    fn the_operator_is_told_what_the_disc_will_not_be() {
        let built = track_plan(&[2352 * 100], vec![track(1, "AUDIO", 100)]);

        let warnings = built.layout_warnings();
        assert!(
            warnings.iter().any(|warning| warning.contains("audio")),
            "audio verification is not a byte comparison: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("subchannel")),
            "a descriptor carries no subchannel data: {warnings:?}"
        );
    }

    #[test]
    fn a_disc_past_seventy_four_minutes_says_it_needs_longer_media() {
        let sectors = cd::REDBOOK_SECTORS + 1;
        let built = track_plan(&[2352 * sectors], vec![track(1, "AUDIO", sectors)]);

        assert!(
            built
                .layout_warnings()
                .iter()
                .any(|warning| warning.contains("80 minute")),
            "{:?}",
            built.layout_warnings()
        );
    }
}
