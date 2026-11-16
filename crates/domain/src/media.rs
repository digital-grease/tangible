// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The discs a drive writes, from the media profiles it lists.
//!
//! A drive names the MMC profiles it supports, which xorriso prints as
//! `DVD+R/DL`, `DVD-R sequential recording`, `BD-RE` and so on. Those that
//! record are what it writes, and the largest nominal size among them is the
//! most one disc in it can hold. A drive's list is evidence, not a promise:
//! preflight still reads the disc actually in the drive before anything is
//! written, and that disc's own capacity is what a burn is held to.
//!
//! The sizes are nominal, so they answer only "could this possibly fit": a
//! release larger than the largest disc a person's drives write is worth a
//! warning before it is downloaded.

use crate::cd;

/// The kinds of disc a size is judged against, smallest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiscSize {
    /// A CD of 80 minutes.
    Cd,
    /// A single-layer DVD.
    Dvd,
    /// A dual-layer DVD.
    DvdDualLayer,
    /// A dual-layer Blu-ray disc.
    BluRayDualLayer,
}

/// Bytes in a DVD's or Blu-ray disc's sector.
const SECTOR_BYTES: u64 = 2048;

/// Bytes a CD image keeps for each sector as a BIN: the whole 2,352, which
/// is the most any image of a CD takes (an ISO of a data CD keeps 2,048).
const CD_RAW_SECTOR_BYTES: u64 = 2352;

impl DiscSize {
    /// The most an image of one such disc can be, in bytes.
    ///
    /// A CD at [`cd::MAX_SECTORS`], kept whole; a DVD-R's 2,298,496 sectors
    /// (a DVD+R has 2,295,104); a DVD+R DL's 4,173,824 (a DVD-R DL has
    /// 4,171,712); and a 50 GB Blu-ray disc's 24,438,784. Blu-ray drives
    /// list one profile for every layer count, and every one that writes
    /// writes two layers, so two is what a Blu-ray profile counts as. Three
    /// and four layer discs (BDXL) are not counted: the profile does not say
    /// whether a drive writes them.
    #[must_use]
    pub const fn bytes(self) -> u64 {
        match self {
            Self::Cd => cd::MAX_SECTORS * CD_RAW_SECTOR_BYTES,
            Self::Dvd => 2_298_496 * SECTOR_BYTES,
            Self::DvdDualLayer => 4_173_824 * SECTOR_BYTES,
            Self::BluRayDualLayer => 24_438_784 * SECTOR_BYTES,
        }
    }

    /// Its name on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cd => "cd",
            Self::Dvd => "dvd",
            Self::DvdDualLayer => "dvd_dual_layer",
            Self::BluRayDualLayer => "bluray_dual_layer",
        }
    }

    /// Whether `bytes` could be one such disc.
    #[must_use]
    pub const fn holds(self, bytes: u64) -> bool {
        bytes <= self.bytes()
    }
}

/// The profile's name, upper-cased, and its words.
fn words(profile: &str) -> (String, Vec<String>) {
    let name = profile.trim().to_ascii_uppercase();
    let words = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect();
    (name, words)
}

/// Whether a profile names an optical disc: a CD, DVD, Blu-ray disc or HD
/// DVD, rather than the generic profiles (`Removable disk`) drives also list.
#[must_use]
pub fn is_optical(profile: &str) -> bool {
    let (name, _) = words(profile);
    ["CD", "DVD", "BD", "HD DVD"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// Whether a profile is one a drive records: an optical profile other than
/// a pressed one (`CD-ROM`, `DVD-ROM`, `BD-ROM`, `HD DVD-ROM`).
#[must_use]
pub fn is_recordable(profile: &str) -> bool {
    let (_, words) = words(profile);
    is_optical(profile) && !words.iter().any(|word| word == "ROM")
}

/// How large a disc of a recordable profile can be. `None` for a profile
/// that does not record, or an HD DVD's, which Tangible does not write.
#[must_use]
pub fn size_of(profile: &str) -> Option<DiscSize> {
    if !is_recordable(profile) {
        return None;
    }
    let (name, words) = words(profile);
    let dual_layer = words.iter().any(|word| word == "DL");
    if name.starts_with("CD") {
        Some(DiscSize::Cd)
    } else if name.starts_with("DVD") {
        Some(if dual_layer {
            DiscSize::DvdDualLayer
        } else {
            DiscSize::Dvd
        })
    } else if name.starts_with("BD") {
        Some(DiscSize::BluRayDualLayer)
    } else {
        None
    }
}

/// The largest disc any of `profiles` writes.
pub fn largest<'a>(profiles: impl IntoIterator<Item = &'a str>) -> Option<DiscSize> {
    profiles
        .into_iter()
        .filter_map(size_of)
        .max_by_key(|size| size.bytes())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The Slimtype DS8A8SH's list, as xorriso printed it on 2026-11-15
    /// (fixtures/tool-output/xorriso/profiles-dvd-writer.txt).
    const SLIMTYPE: [&str; 14] = [
        "DVD+R/DL",
        "DVD+R",
        "DVD+RW",
        "DVD-R/DL layer jump recording",
        "DVD-R/DL sequential recording",
        "DVD-RW sequential recording",
        "DVD-RW restricted overwrite",
        "DVD-RAM",
        "DVD-R sequential recording",
        "DVD-ROM",
        "CD-RW",
        "CD-R",
        "CD-ROM",
        "Removable disk",
    ];

    #[test]
    fn a_dvd_writer_writes_a_dual_layer_dvd_at_most() {
        assert_eq!(largest(SLIMTYPE), Some(DiscSize::DvdDualLayer));
        let recordable: Vec<_> = SLIMTYPE.into_iter().filter(|p| is_recordable(p)).collect();
        assert_eq!(recordable.len(), 11, "{recordable:?}");
        for pressed in ["DVD-ROM", "CD-ROM", "Removable disk"] {
            assert!(!is_recordable(pressed), "{pressed}");
        }
        assert!(!is_optical("Removable disk"));
        assert!(is_optical("CD-ROM"));
    }

    #[test]
    fn each_family_is_sized() {
        assert_eq!(size_of("CD-R"), Some(DiscSize::Cd));
        assert_eq!(size_of("CD-RW"), Some(DiscSize::Cd));
        assert_eq!(size_of("DVD-R sequential recording"), Some(DiscSize::Dvd));
        assert_eq!(size_of("DVD-RAM"), Some(DiscSize::Dvd));
        assert_eq!(size_of("DVD+RW"), Some(DiscSize::Dvd));
        assert_eq!(size_of("DVD+R/DL"), Some(DiscSize::DvdDualLayer));
        assert_eq!(size_of("dvd+r-dl"), Some(DiscSize::DvdDualLayer));
        assert_eq!(
            size_of("DVD-R/DL layer jump recording"),
            Some(DiscSize::DvdDualLayer)
        );
        assert_eq!(
            size_of("BD-R sequential recording"),
            Some(DiscSize::BluRayDualLayer)
        );
        assert_eq!(size_of("BD-RE"), Some(DiscSize::BluRayDualLayer));
        for none in [
            "BD-ROM",
            "DVD-ROM",
            "HD DVD-R",
            "Removable disk",
            "",
            "tape",
        ] {
            assert_eq!(size_of(none), None, "{none}");
        }
    }

    #[test]
    fn the_largest_is_the_largest_whatever_the_order() {
        assert_eq!(largest(["CD-R"]), Some(DiscSize::Cd));
        assert_eq!(
            largest(["BD-RE", "CD-R", "DVD+R"]),
            Some(DiscSize::BluRayDualLayer)
        );
        assert_eq!(largest(["DVD-ROM", "CD-ROM"]), None);
        assert_eq!(largest(std::iter::empty()), None);
    }

    #[test]
    fn the_sizes_are_the_nominal_ones() {
        assert_eq!(DiscSize::Cd.bytes(), 846_364_848);
        assert_eq!(DiscSize::Dvd.bytes(), 4_707_319_808);
        assert_eq!(DiscSize::DvdDualLayer.bytes(), 8_547_991_552);
        assert_eq!(DiscSize::BluRayDualLayer.bytes(), 50_050_629_632);
        assert!(DiscSize::Dvd.holds(4_707_319_808));
        assert!(!DiscSize::Dvd.holds(4_707_319_809));
        assert_eq!(DiscSize::DvdDualLayer.as_str(), "dvd_dual_layer");
    }
}
