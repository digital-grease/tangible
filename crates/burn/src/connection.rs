// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! How the worker's drive is attached: USB or SATA.
//!
//! Burn reports name it, because a USB bridge and a SATA port fail in
//! different ways. The kernel's device tree says which: the drive's entry
//! under `/sys/dev/block` links to a path through the bus it hangs off, such
//! as `…/usb2/2-1/2-1:1.0/host6/…` or `…/ata1/host0/…`.
//!
//! The drive is found by its device numbers, not its name. Inside the burn
//! container the drive is always `/dev/sr0`, whichever node it is on the
//! host, and `/sys` is the host's, so looking up `sr0` by name could describe
//! a different drive. The block device's major and minor numbers are the
//! same on both sides.
//!
//! Read-only, and best effort: anything unexpected is `Unknown`, which a
//! report leaves for the person to fill in.

use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
use std::path::Path;

use tangible_domain::DriveConnection;

/// Where the kernel's device tree is mounted.
pub const SYS_ROOT: &str = "/sys";

/// How the block device at `device` is attached.
#[must_use]
pub fn detect(device: &Path) -> DriveConnection {
    detect_in(device, Path::new(SYS_ROOT))
}

/// [`detect`], against a device tree mounted at `sys_root`.
#[must_use]
pub fn detect_in(device: &Path, sys_root: &Path) -> DriveConnection {
    let Ok(metadata) = std::fs::metadata(device) else {
        return DriveConnection::Unknown;
    };
    if !metadata.file_type().is_block_device() {
        return DriveConnection::Unknown;
    }
    from_device_number(metadata.rdev(), sys_root)
}

/// How the block device numbered `rdev` is attached, by following its link
/// under `sys_root/dev/block`.
#[must_use]
pub fn from_device_number(rdev: u64, sys_root: &Path) -> DriveConnection {
    let (major, minor) = split_device_number(rdev);
    let link = sys_root.join("dev/block").join(format!("{major}:{minor}"));
    match std::fs::canonicalize(link) {
        Ok(target) => classify(&target),
        Err(_) => DriveConnection::Unknown,
    }
}

/// Whether the block device at `device` is gone from the machine: its node
/// is still here, but the kernel lists no device with its numbers. That is
/// a drive disconnected, or reconnected as another device (the next `srN`,
/// say), since the container started: Docker gives a container its devices,
/// by number, when it starts, so the container's node stays the old one.
/// False whenever it cannot be told, as without a device tree to read.
#[must_use]
pub fn is_gone(device: &Path) -> bool {
    is_gone_in(device, Path::new(SYS_ROOT))
}

/// [`is_gone`], against a device tree mounted at `sys_root`.
#[must_use]
pub fn is_gone_in(device: &Path, sys_root: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(device) else {
        return false;
    };
    metadata.file_type().is_block_device() && number_is_gone(metadata.rdev(), sys_root)
}

/// Whether the kernel lists no block device numbered `rdev`: the device
/// tree is there and lists devices, and none with these numbers.
#[must_use]
pub fn number_is_gone(rdev: u64, sys_root: &Path) -> bool {
    let listed = sys_root.join("dev/block");
    let Ok(mut entries) = std::fs::read_dir(&listed) else {
        return false;
    };
    if entries.next().is_none() {
        return false;
    }
    let (major, minor) = split_device_number(rdev);
    std::fs::symlink_metadata(listed.join(format!("{major}:{minor}"))).is_err()
}

/// What a person is told when the drive at `device` is gone: what happened,
/// and what brings it back, which depends on whether this worker restarts
/// itself to take its drive up ([`crate::replug::restarts_itself`]).
#[must_use]
pub fn gone_detail(device: &str) -> String {
    gone_detail_saying(device, crate::replug::restarts_itself())
}

/// [`gone_detail`], for a worker that does, or does not, restart itself to
/// take its drive up once it is back.
#[must_use]
pub fn gone_detail_saying(device: &str, restarts_itself: bool) -> String {
    let remedy = if restarts_itself {
        "Once the drive is plugged back in, this worker restarts itself to take it up"
    } else {
        "Restart the worker (docker compose restart, naming its service) so it finds the drive \
         again"
    };
    format!(
        "the drive at {device} is gone from this machine: it was disconnected, or reconnected \
         as another device, since this worker started, and a container is given its drive when \
         it starts. {remedy}"
    )
}

/// What brings a gone drive back, for the worker's log.
#[must_use]
pub fn gone_remedy() -> &'static str {
    if crate::replug::restarts_itself() {
        "this worker restarts itself to take it up once it is back"
    } else {
        "restart the worker to find it again"
    }
}

/// A Linux `dev_t` as its major and minor numbers, as glibc's `major()` and
/// `minor()` split it.
#[must_use]
pub fn split_device_number(rdev: u64) -> (u64, u64) {
    let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff);
    let minor = (rdev & 0xff) | ((rdev >> 12) & !0xff);
    (major, minor)
}

/// The bus a device-tree path runs through. USB is checked first: a USB
/// enclosure can carry its own SATA bridge, and the USB link is the one that
/// matters to a burn.
#[must_use]
pub fn classify(path: &Path) -> DriveConnection {
    let components = || {
        path.components()
            .filter_map(|component| component.as_os_str().to_str())
    };
    let numbered = |name: &str, prefix: &str| {
        name.strip_prefix(prefix)
            .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
    };
    if components().any(|name| numbered(name, "usb")) {
        DriveConnection::Usb
    } else if components().any(|name| numbered(name, "ata")) {
        DriveConnection::Sata
    } else {
        DriveConnection::Unknown
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_usb_drive_is_recognised() {
        // The first real drive, a Slimtype DS8A8SH, through a USB hub: the
        // path its /sys/dev/block/11:0 link resolved to.
        let path = Path::new(
            "/sys/devices/pci0000:00/0000:00:08.3/0000:c6:00.4/usb7/7-1/7-1.4/7-1.4.1/7-1.4.1.1/7-1.4.1.1:1.0/host0/target0:0:0/0:0:0:0/block/sr0",
        );
        assert_eq!(classify(path), DriveConnection::Usb);
    }

    #[test]
    fn a_sata_drive_is_recognised() {
        let path = Path::new(
            "/sys/devices/pci0000:00/0000:00:17.0/ata2/host1/target1:0:0/1:0:0:0/block/sr0",
        );
        assert_eq!(classify(path), DriveConnection::Sata);
    }

    #[test]
    fn anything_else_is_unknown() {
        for path in [
            "/sys/devices/virtual/block/loop0",
            "/sys/devices/platform/host0/target0:0:0/0:0:0:0/block/sr0",
            // Words that merely start like a bus are not one.
            "/sys/devices/usbfoo/atari/block/sr0",
        ] {
            assert_eq!(
                classify(Path::new(path)),
                DriveConnection::Unknown,
                "{path}"
            );
        }
    }

    #[test]
    fn device_numbers_split_as_glibc_does() {
        // sr0 is block 11:0; a large minor uses the high bits.
        assert_eq!(split_device_number(0x0b00), (11, 0));
        assert_eq!(split_device_number(0x0b01), (11, 1));
        let big = (0x123_u64 << 8) | 0x45 | (0x678_u64 << 20);
        assert_eq!(split_device_number(big), (0x123, 0x67845));
    }

    #[test]
    fn the_drive_is_found_by_number_through_the_device_tree() {
        let sys = tempfile::tempdir().expect("tempdir");
        let device = sys
            .path()
            .join("devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host6/block/sr0");
        std::fs::create_dir_all(&device).expect("device dir");
        std::fs::create_dir_all(sys.path().join("dev/block")).expect("dev/block");
        std::os::unix::fs::symlink(&device, sys.path().join("dev/block/11:0")).expect("link");
        assert_eq!(from_device_number(0x0b00, sys.path()), DriveConnection::Usb);
        assert_eq!(
            from_device_number(0x0b01, sys.path()),
            DriveConnection::Unknown,
            "a number with no entry"
        );
    }

    #[test]
    fn a_drive_is_gone_when_the_tree_lists_others_and_not_it() {
        let sys = tempfile::tempdir().expect("tempdir");
        let listed = sys.path().join("dev/block");
        // No tree at all, or an empty one: nothing can be told.
        assert!(!number_is_gone(0x0b00, sys.path()));
        std::fs::create_dir_all(&listed).expect("dev/block");
        assert!(!number_is_gone(0x0b00, sys.path()));
        // The drive came back as sr1 (11:1); the container still has 11:0.
        let device = sys.path().join("devices/usb7/7-1.3/host1/block/sr1");
        std::fs::create_dir_all(&device).expect("device dir");
        std::os::unix::fs::symlink(&device, listed.join("11:1")).expect("link");
        assert!(number_is_gone(0x0b00, sys.path()), "11:0 is gone");
        assert!(!number_is_gone(0x0b01, sys.path()), "11:1 is here");
        // A file, or nothing at all, is never said to be a gone drive.
        let file = tempfile::NamedTempFile::new().expect("file");
        assert!(!is_gone_in(file.path(), sys.path()));
        assert!(!is_gone_in(Path::new("/nonexistent/sr0"), sys.path()));
        assert!(gone_detail_saying("/dev/sr0", false).contains("Restart the worker"));
        let following = gone_detail_saying("/dev/sr0", true);
        assert!(following.contains("restarts itself"), "{following}");
        assert!(!following.contains("Restart the worker"), "{following}");
    }

    #[test]
    fn something_that_is_not_a_block_device_is_unknown() {
        let file = tempfile::NamedTempFile::new().expect("file");
        assert_eq!(detect(file.path()), DriveConnection::Unknown);
        assert_eq!(
            detect(Path::new("/nonexistent/sr0")),
            DriveConnection::Unknown
        );
    }
}
