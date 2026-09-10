// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Facts about compact discs.
//!
//! Small, dull, and shared on purpose. A track mode's sector size decides
//! where every byte of a disc goes, and two copies of that table that disagree
//! would be two components silently describing different discs. The descriptor
//! parser and the burn planner both read it from here.

/// Bytes in a sector, by the mode a descriptor names.
///
/// These are the raw sizes the mode occupies in a file, not the user-data
/// sizes: `MODE2/2336` carries 2336 bytes per sector of which fewer are user
/// data, and it is the 2336 that positions the next sector.
///
/// `None` for anything unrecognised, which is the honest answer and the one
/// that makes a layout refuse rather than guess.
#[must_use]
pub fn sector_bytes(mode: &str) -> Option<u32> {
    // Written out one per line rather than derived from the digits in the
    // name. The digits agree with the size today; a table that computed them
    // would agree with a typo tomorrow.
    #[allow(clippy::match_same_arms)]
    match mode.to_ascii_uppercase().as_str() {
        "AUDIO" => Some(2352),
        "CDG" => Some(2448),
        "MODE1/2048" => Some(2048),
        "MODE1/2352" => Some(2352),
        "MODE2/2048" => Some(2048),
        "MODE2/2324" => Some(2324),
        "MODE2/2336" => Some(2336),
        "MODE2/2352" => Some(2352),
        "CDI/2336" => Some(2336),
        "CDI/2352" => Some(2352),
        _ => None,
    }
}

/// Whether a mode carries audio rather than data.
///
/// The distinction decides what verification can claim: data sectors carry
/// error correction and can be compared byte for byte, and audio cannot be,
/// because without the drive's read offset a comparison fails on correct hardware.
#[must_use]
pub fn is_audio(mode: &str) -> bool {
    matches!(mode.to_ascii_uppercase().as_str(), "AUDIO" | "CDG")
}

/// Whether a media profile names a CD.
///
/// A prefix test on the MMC profile names, which is enough because every CD
/// profile begins with the two letters and no other family does: `CD-R`,
/// `CD-RW`, `CD-ROM` against `DVD-R`, `BD-R` and the rest.
#[must_use]
pub fn is_cd_profile(profile: &str) -> bool {
    profile.trim().to_ascii_uppercase().starts_with("CD")
}

/// Frames in one second, and therefore sectors: a frame is one block.
///
/// The constant every timecode in every CD descriptor is built from.
pub const FRAMES_PER_SECOND: u64 = 75;

/// Seconds in one minute of a timecode.
pub const SECONDS_PER_MINUTE: u64 = 60;

/// Most tracks a CD can hold.
pub const MAX_TRACKS: u32 = 99;

/// Sectors on a Red Book 74 minute disc.
///
/// Not a limit, a threshold: longer discs are ordinary and write fine on most
/// media, and are worth mentioning because the ones that do not write are
/// always the long ones.
pub const REDBOOK_SECTORS: u64 = 333_000;

/// Sectors of image this project will write to a CD.
///
/// The practical maximum for 80 minute media: 79:59:74 is the last addressable
/// position, and the 150 sector lead-in before it carries no image data.
/// Layouts are image-relative, so this is the number they are compared to.
///
/// Overburning past it is not offered. It depends on the drive, the media and
/// luck, and the failure mode is a ruined disc at the end of a long write.
pub const MAX_SECTORS: u64 = 359_849;

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn every_known_mode_has_a_sector_size() {
        for mode in [
            "AUDIO",
            "CDG",
            "MODE1/2048",
            "MODE1/2352",
            "MODE2/2048",
            "MODE2/2324",
            "MODE2/2336",
            "MODE2/2352",
            "CDI/2336",
            "CDI/2352",
        ] {
            assert!(sector_bytes(mode).is_some(), "{mode} has no size");
        }
    }

    #[test]
    fn a_mode_is_recognised_whatever_case_it_is_written_in() {
        assert_eq!(sector_bytes("mode2/2352"), Some(2352));
        assert!(is_audio("audio"));
    }

    #[test]
    fn an_unknown_mode_has_no_size_rather_than_a_default() {
        assert_eq!(sector_bytes("MODE9/9999"), None);
        assert_eq!(sector_bytes(""), None);
        assert!(!is_audio("MODE1/2048"));
    }

    #[test]
    fn cd_profiles_are_told_from_the_rest() {
        assert!(is_cd_profile("CD-R"));
        assert!(is_cd_profile("CD-RW"));
        assert!(is_cd_profile("cd-rom"));
        assert!(!is_cd_profile("DVD-R"));
        assert!(!is_cd_profile("BD-R"));
        assert!(!is_cd_profile(""));
    }

    #[test]
    fn the_capacity_constants_are_the_right_way_round() {
        const { assert!(REDBOOK_SECTORS < MAX_SECTORS) };
        // 79:59:74 is the last addressable position on 80 minute media. The
        // first 150 sectors are the lead-in and carry no image data, and the
        // layouts this is compared against are image-relative, so they come
        // off. Spelled out rather than trusted: the first version of this
        // constant was wrong by exactly that 150.
        assert_eq!(MAX_SECTORS, (79 * 60 + 59) * 75 + 74 - 150);
        assert_eq!(FRAMES_PER_SECOND * SECONDS_PER_MINUTE, 4500);
    }
}
