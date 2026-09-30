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

/// Whether a string is a media catalogue number.
///
/// Thirteen digits, which is a UPC/EAN. Checked rather than assumed because
/// the value arrives from a descriptor somebody else wrote, and a manifest is
/// read by other tools: one that carries a `catalog` field at all should be
/// one they can trust the shape of.
///
/// The check digit is not verified. Real discs carry catalogue numbers that
/// fail it, and refusing those would lose a true record of what was on the
/// disc in exchange for a validation nobody asked for.
#[must_use]
pub fn is_catalog_number(value: &str) -> bool {
    value.len() == CATALOG_DIGITS && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// Whether a string is an International Standard Recording Code.
///
/// Twelve characters, `CCOOOYYSSSSS`: two letters of country, three
/// alphanumerics of registrant, two digits of year and five of designation.
/// Compared without regard to case, which is how the standard defines it, and
/// stored as written.
#[must_use]
pub fn is_isrc(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != ISRC_LENGTH {
        return false;
    }
    let alphabetic = |index: usize| bytes.get(index).is_some_and(u8::is_ascii_alphabetic);
    let alphanumeric = |index: usize| bytes.get(index).is_some_and(u8::is_ascii_alphanumeric);
    let digit = |index: usize| bytes.get(index).is_some_and(u8::is_ascii_digit);

    (0..2).all(alphabetic) && (2..5).all(alphanumeric) && (5..12).all(digit)
}

/// The order of the two bytes in each 16-bit audio sample of a file.
///
/// A fact about how a file stores audio, not about the disc: the disc has one
/// sample order and a file may hold either. It is recorded because nothing in
/// the bytes says which, and a writer that assumes the wrong one burns every
/// audio track as full-volume static. cdrdao reads raw audio as big-endian
/// unless told otherwise, while almost every rip stores it little-endian, so
/// the wrong assumption is also the default one.
///
/// Meaningless for data sectors, which are bytes rather than samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleByteOrder {
    /// Least significant byte first. What a CUE sheet calls `BINARY`, and
    /// what nearly every rip is.
    LittleEndian,
    /// Most significant byte first. What a CUE sheet calls `MOTOROLA`.
    BigEndian,
}

/// A subcode flag a track can carry.
///
/// A closed set: these four are the Q-channel control bits a descriptor can
/// state, spelled as CUE sheets spell them. They change how a disc plays or
/// may be copied, so a disc burned without them is a different disc, and one
/// that differs invisibly until a player applies the wrong equalisation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum TrackFlag {
    /// Digital copy permitted.
    #[serde(rename = "DCP")]
    DigitalCopyPermitted,
    /// Four-channel audio.
    #[serde(rename = "4CH")]
    FourChannel,
    /// Pre-emphasis applied when mastering, which a player has to undo.
    #[serde(rename = "PRE")]
    PreEmphasis,
    /// Serial copy management system.
    #[serde(rename = "SCMS")]
    SerialCopyManagement,
}

impl TrackFlag {
    /// The flag a descriptor names, or `None` for one outside the set.
    ///
    /// Case-insensitive, as descriptors are.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_uppercase().as_str() {
            "DCP" => Some(Self::DigitalCopyPermitted),
            "4CH" => Some(Self::FourChannel),
            "PRE" => Some(Self::PreEmphasis),
            "SCMS" => Some(Self::SerialCopyManagement),
            _ => None,
        }
    }

    /// The flag as a descriptor spells it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DigitalCopyPermitted => "DCP",
            Self::FourChannel => "4CH",
            Self::PreEmphasis => "PRE",
            Self::SerialCopyManagement => "SCMS",
        }
    }
}

/// Shortest track a CD may hold, in sectors: four seconds.
///
/// Measured from INDEX 01, so a pregap does not count towards it. A writer
/// refuses a shorter track rather than burn a disc that breaks the format,
/// which makes this a thing to check before anyone is asked for media.
pub const MIN_TRACK_SECTORS: u64 = 4 * FRAMES_PER_SECOND;

/// Digits in a media catalogue number.
pub const CATALOG_DIGITS: usize = 13;

/// Characters in an ISRC.
pub const ISRC_LENGTH: usize = 12;

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
    fn a_catalogue_number_is_thirteen_digits() {
        assert!(is_catalog_number("1234567890123"));
        assert!(!is_catalog_number("123456789012"), "twelve is not thirteen");
        assert!(!is_catalog_number("12345678901234"));
        assert!(!is_catalog_number("123456789012X"));
        assert!(!is_catalog_number(""));
        // Leading zeroes are ordinary, and a numeric parse would eat them.
        assert!(is_catalog_number("0000000000000"));
    }

    #[test]
    fn an_isrc_is_two_letters_three_alphanumerics_and_seven_digits() {
        assert!(is_isrc("USRC17607839"));
        assert!(is_isrc("usrc17607839"), "case does not change the code");
        assert!(is_isrc("GBAYE0601498"));
        assert!(!is_isrc("USRC1760783"), "eleven characters");
        assert!(!is_isrc("U1RC17607839"), "the country is letters");
        assert!(!is_isrc("USRC1760783X"), "the designation is digits");
        assert!(!is_isrc("US-RC1-76-07839"), "hyphens are not stored");
        assert!(!is_isrc(""));
    }

    #[test]
    fn a_flag_is_read_as_a_descriptor_writes_it() {
        assert_eq!(
            TrackFlag::parse("DCP"),
            Some(TrackFlag::DigitalCopyPermitted)
        );
        assert_eq!(TrackFlag::parse("4ch"), Some(TrackFlag::FourChannel));
        assert_eq!(TrackFlag::parse("pre"), Some(TrackFlag::PreEmphasis));
        assert_eq!(
            TrackFlag::parse("SCMS"),
            Some(TrackFlag::SerialCopyManagement)
        );
        // Some sheets write DATA for a data track. It is not a flag anyone
        // sets; the mode already says it.
        assert_eq!(TrackFlag::parse("DATA"), None);
        assert_eq!(TrackFlag::parse(""), None);
    }

    #[test]
    fn a_flag_serializes_as_it_is_spelled() {
        // The manifest persists the text, so the spelling is the contract.
        for flag in [
            TrackFlag::DigitalCopyPermitted,
            TrackFlag::FourChannel,
            TrackFlag::PreEmphasis,
            TrackFlag::SerialCopyManagement,
        ] {
            let json = serde_json::to_string(&flag).expect("serialize");
            assert_eq!(json, format!("\"{}\"", flag.as_str()));
            assert_eq!(TrackFlag::parse(flag.as_str()), Some(flag));
        }
        assert!(serde_json::from_str::<TrackFlag>("\"DATA\"").is_err());
    }

    #[test]
    fn a_byte_order_serializes_as_words() {
        assert_eq!(
            serde_json::to_string(&SampleByteOrder::LittleEndian).expect("serialize"),
            "\"little_endian\""
        );
        assert_eq!(
            serde_json::to_string(&SampleByteOrder::BigEndian).expect("serialize"),
            "\"big_endian\""
        );
    }

    #[test]
    fn the_shortest_track_is_four_seconds() {
        assert_eq!(MIN_TRACK_SECTORS, 300);
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
