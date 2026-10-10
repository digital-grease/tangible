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
    fn something_that_is_not_a_block_device_is_unknown() {
        let file = tempfile::NamedTempFile::new().expect("file");
        assert_eq!(detect(file.path()), DriveConnection::Unknown);
        assert_eq!(
            detect(Path::new("/nonexistent/sr0")),
            DriveConnection::Unknown
        );
    }
}
