// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Room on the worker's own disk for what a rip writes before sending it.
//!
//! A rip that runs out of room fails only when the disk is full, which for a
//! redumper dump or a dual-layer DVD can be an hour in. So the worker works
//! out beforehand what the disc in the drive will need and refuses a rip it
//! has no room for, with nothing read.
//!
//! What each way of reading writes:
//!
//! - cdrdao: the table of contents and one data file of 2352 bytes a
//!   sector.
//! - redumper: its raw dump runs from the start of the lead-in, LBA -45150,
//!   to past the end of the disc, at 3036 bytes a sector (2352 to `.scram`,
//!   588 to `.state`, 96 to `.subcode`), then `split` writes the tracks
//!   beside it at 2352 a sector. Checked against the bench dump of
//!   2026-10-22: 48,758 sectors gave a `.scram` of 114.7 MB, a `.state` of
//!   28.7 MB and a `.subcode` of 4.7 MB, exactly. A full 80-minute CD needs
//!   about 2.1 GB.
//! - a DVD: its image, sector for sector, and as much again for a decrypted
//!   copy kept beside it.

use std::path::Path;

/// Bytes a raw CD sector takes.
const RAW_SECTOR_BYTES: u64 = 2352;

/// Bytes redumper's dump files take for each sector: `.scram`, `.state` and
/// `.subcode`.
const REDUMPER_DUMP_BYTES_PER_SECTOR: u64 = 2352 + 588 + 96;

/// Sectors redumper's dump files hold before the disc's first: from LBA
/// -45150 to 0.
const REDUMPER_LEAD_IN_SECTORS: u64 = 45_150;

/// Sectors allowed past the disc's end, for the lead-out redumper reads:
/// the 90 seconds a lead-out is at least.
const REDUMPER_LEAD_OUT_SECTORS: u64 = 90 * 75;

/// Room kept free beyond what a rip writes, for the worker's own state and
/// the logs and tables the tools write beside their output.
pub const HEADROOM_BYTES: u64 = 64 * 1024 * 1024;

/// How a rip will read the disc in the drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    /// A CD, with cdrdao.
    CdWithCdrdao,
    /// A CD, with redumper.
    CdWithRedumper,
    /// A DVD or other image, kept `copies` times over: two when a decrypted
    /// copy may be kept beside the image.
    Image {
        /// How many disc-sized trees or images it may write.
        copies: u64,
    },
}

/// The bytes a rip may write for a disc of `blocks` blocks of `block_size`
/// bytes, headroom included.
#[must_use]
pub fn rip_needs(reading: Reading, blocks: u64, block_size: u32) -> u64 {
    let written = match reading {
        Reading::CdWithCdrdao => blocks.saturating_mul(RAW_SECTOR_BYTES),
        Reading::CdWithRedumper => REDUMPER_LEAD_IN_SECTORS
            .saturating_add(blocks)
            .saturating_add(REDUMPER_LEAD_OUT_SECTORS)
            .saturating_mul(REDUMPER_DUMP_BYTES_PER_SECTOR)
            .saturating_add(blocks.saturating_mul(RAW_SECTOR_BYTES)),
        Reading::Image { copies } => blocks
            .saturating_mul(u64::from(block_size))
            .saturating_mul(copies),
    };
    written.saturating_add(HEADROOM_BYTES)
}

/// The bytes free to this process on the filesystem holding `path`, or on
/// that of its nearest ancestor that exists. `None` when it cannot be told,
/// and a rip then goes ahead unchecked, as before.
#[cfg(unix)]
#[must_use]
pub fn free_bytes(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|ancestor| ancestor.exists())?;
    let stat = rustix::fs::statvfs(existing).ok()?;
    let unit = if stat.f_frsize > 0 {
        stat.f_frsize
    } else {
        stat.f_bsize
    };
    Some(stat.f_bavail.saturating_mul(unit))
}

/// Free space cannot be told here. Workers run in Linux containers, so this
/// exists to keep the crate building rather than to be used.
#[cfg(not(unix))]
#[must_use]
pub fn free_bytes(path: &Path) -> Option<u64> {
    let _ = path;
    None
}

/// Bytes as a person reads them, to one decimal place.
#[must_use]
pub fn describe(bytes: u64) -> String {
    const GB: u64 = 1_000_000_000;
    const MB: u64 = 1_000_000;
    // One decimal place of gigabytes or megabytes; exactness is not the
    // point of a sentence saying a disk is too full.
    #[allow(clippy::cast_precision_loss)]
    let (value, unit) = if bytes >= GB {
        (bytes as f64 / GB as f64, "GB")
    } else {
        (bytes as f64 / MB as f64, "MB")
    };
    format!("{value:.1} {unit}")
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn redumper_needs_what_the_bench_dump_took() {
        // The bench disc, 3608 sectors: 148.1 MB of dump files and 8.5 MB of
        // tracks, as measured on 2026-10-22, plus the lead-out allowance.
        let dump = (45_150 + 3608) * REDUMPER_DUMP_BYTES_PER_SECTOR;
        assert_eq!(dump, 114_678_816 + 28_669_704 + 4_680_768);
        let needed = rip_needs(Reading::CdWithRedumper, 3608, 2048);
        assert_eq!(
            needed,
            dump + 6750 * REDUMPER_DUMP_BYTES_PER_SECTOR + 3608 * 2352 + HEADROOM_BYTES
        );
        // A full 80-minute disc: about 2.1 GB.
        let full = rip_needs(Reading::CdWithRedumper, 360_000, 2048);
        assert!((2_100_000_000..2_200_000_000).contains(&full), "{full}");
    }

    #[test]
    fn cdrdao_and_images_need_the_disc_once_or_as_many_times_as_kept() {
        assert_eq!(
            rip_needs(Reading::CdWithCdrdao, 360_000, 2048),
            360_000 * 2352 + HEADROOM_BYTES
        );
        let dvd = 2_295_104;
        assert_eq!(
            rip_needs(Reading::Image { copies: 1 }, dvd, 2048),
            dvd * 2048 + HEADROOM_BYTES
        );
        assert_eq!(
            rip_needs(Reading::Image { copies: 2 }, dvd, 2048),
            2 * dvd * 2048 + HEADROOM_BYTES
        );
        assert_eq!(
            rip_needs(Reading::Image { copies: 2 }, u64::MAX, 2048),
            u64::MAX,
            "saturates"
        );
    }

    #[cfg(unix)]
    #[test]
    fn free_space_is_measured_from_the_nearest_directory_that_exists() {
        let dir = tempfile::tempdir().unwrap();
        let here = free_bytes(dir.path()).expect("statvfs answers for a temporary directory");
        assert!(here > 0);
        assert!(free_bytes(&dir.path().join("rips/not-yet")).is_some());
    }

    #[test]
    fn sizes_read_as_people_say_them() {
        assert_eq!(describe(2_134_567_890), "2.1 GB");
        assert_eq!(describe(345_600_000), "345.6 MB");
        assert_eq!(describe(0), "0.0 MB");
    }
}
