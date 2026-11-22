// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Taking the drive up again when it is plugged back in.
//!
//! Docker gives a container its devices by number when it starts it. A USB
//! drive that drops off its bus and comes back is a new device to the
//! kernel, under a new number while this worker still holds the old one
//! (`sr1` for a drive that was `sr0`), so the container's node names a
//! device that is no longer there ([`crate::connection::is_gone`]). Docker
//! looks the drive's name up again each time it starts the container, so a
//! restart gives the worker its drive back, and a worker can have one made
//! by stopping: the Compose files restart it (`restart: unless-stopped`).
//!
//! It stops only when the restart will find the drive, because Docker cannot
//! start a container whose device is missing, and then leaves it stopped: it
//! does not try again when the device comes back. So:
//!
//! - the drive is known again by what the kernel's device tree said of it
//!   when the worker started ([`DriveMarks`]): its SCSI vendor and model
//!   and, on USB, the USB device's vendor, product and serial number, which
//!   are what its `/dev/disk/by-id` name is made of and which no new number
//!   changes. Exactly one drive the tree lists must match: two alike cannot
//!   be told apart;
//! - it counts as back once the tree has listed it for a while, by when
//!   udev has given it its name ([`settling_time`]);
//! - the name Docker was given must find it ([`Given`]): a `/dev/disk/by-id`
//!   name does, and a kernel name such as `/dev/sr0` only if the drive came
//!   back as that. Any other, or none known, is left to a person.
//!
//! A drive that keeps dropping, as one short of power does, would have the
//! worker restart over and over, so each restart made for it in the last
//! hour doubles the wait before the next ([`RestartLog`]).
//!
//! Read-only, like [`crate::connection`], and from the same tree: a
//! container sees the host's `/sys`.

use std::io::Read as _;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::connection::{number_is_gone, split_device_number};

/// The exit status of a worker that stops so that its container is
/// restarted with its drive: `EX_TEMPFAIL`, try again, which every restart
/// policy but `no` acts on.
pub const RESTART_FOR_DRIVE: u8 = 75;

/// How long a drive that came back is waited on before restarting for it,
/// the first time in an hour. udev names a drive well within a second of
/// the kernel listing it (0.3 seconds for the first real drive), so this is
/// ample.
pub const FIRST_WAIT: Duration = Duration::from_secs(10);

/// The longest wait, however often the drive has dropped.
pub const LONGEST_WAIT: Duration = Duration::from_mins(15);

/// How far back the restarts made for a drive count towards the wait.
pub const RESTARTS_COUNTED_FOR: time::Duration = time::Duration::HOUR;

/// How long a drive that came back is waited on before a restart, after
/// `recent` restarts for it in the last hour: [`FIRST_WAIT`], doubled for
/// each, and at most [`LONGEST_WAIT`].
#[must_use]
pub fn settling_time(recent: usize) -> Duration {
    let doublings = u32::try_from(recent).unwrap_or(u32::MAX).min(16);
    FIRST_WAIT
        .saturating_mul(1_u32 << doublings)
        .min(LONGEST_WAIT)
}

/// Whether this process restarts itself to take its drive up once it is
/// back, however the kernel numbers it: what a person is told to do about a
/// drive that is gone depends on it ([`crate::connection::gone_detail`]).
/// One worker runs in a process, and the runner says this when it starts
/// following its drive.
static RESTARTS_ITSELF: AtomicBool = AtomicBool::new(false);

/// Whether this process restarts itself to take its drive up once it is
/// back.
#[must_use]
pub fn restarts_itself() -> bool {
    RESTARTS_ITSELF.load(Ordering::Relaxed)
}

/// Say whether this process restarts itself to take its drive up.
pub fn set_restarts_itself(restarts: bool) {
    RESTARTS_ITSELF.store(restarts, Ordering::Relaxed);
}

/// What the kernel's device tree says a drive is: enough to know it again
/// under another number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveMarks {
    /// Its SCSI vendor, as the drive answers an inquiry: `Slimtype`.
    pub vendor: String,
    /// Its SCSI model: `DVD A DS8A8SH`.
    pub model: String,
    /// The USB device it hangs off, for a drive on USB.
    pub usb: Option<UsbMarks>,
}

/// The USB device a drive hangs off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbMarks {
    /// Its vendor id.
    pub vendor_id: String,
    /// Its product id.
    pub product_id: String,
    /// Its serial number, when it has one.
    pub serial: Option<String>,
}

impl DriveMarks {
    /// Of the block device whose directory in the device tree is `block`,
    /// such as `/sys/devices/…/1:0:0:0/block/sr1`. None when the tree does
    /// not say what it is.
    fn of_block(block: &Path) -> Option<Self> {
        let scsi = std::fs::canonicalize(block.join("device")).ok()?;
        // The nearest USB device above the SCSI device is the drive's own
        // bridge; a hub further up has an id too, and is not the drive.
        let usb = scsi
            .ancestors()
            .skip(1)
            .find(|dir| dir.join("idVendor").is_file())
            .and_then(UsbMarks::of);
        Some(Self {
            vendor: attribute(&scsi, "vendor")?,
            model: attribute(&scsi, "model")?,
            usb,
        })
    }
}

impl UsbMarks {
    fn of(dir: &Path) -> Option<Self> {
        Some(Self {
            vendor_id: attribute(dir, "idVendor")?,
            product_id: attribute(dir, "idProduct")?,
            serial: attribute(dir, "serial"),
        })
    }
}

/// A device-tree attribute, trimmed, as the kernel writes them: at most a
/// page. None when it cannot be read or is empty.
fn attribute(dir: &Path, name: &str) -> Option<String> {
    let mut text = String::new();
    std::fs::File::open(dir.join(name))
        .ok()?
        .take(4096)
        .read_to_string(&mut text)
        .ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// A drive the device tree lists.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Listed {
    /// Its major and minor numbers.
    number: (u64, u64),
    /// Its kernel name, which is its node's name on the host: `sr1`.
    name: String,
    /// What the tree says it is.
    marks: DriveMarks,
}

/// Every block device numbered under `major` that the tree at `sys_root`
/// lists and says what it is. Bounded, as the tree is not ours.
fn listed(sys_root: &Path, major: u64) -> Vec<Listed> {
    let Ok(entries) = std::fs::read_dir(sys_root.join("dev/block")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .take(4096)
        .filter_map(|entry| {
            let number = entry.file_name().to_str().and_then(parse_number)?;
            if number.0 != major {
                return None;
            }
            let block = std::fs::canonicalize(entry.path()).ok()?;
            let name = block.file_name()?.to_str()?.to_owned();
            let marks = DriveMarks::of_block(&block)?;
            Some(Listed {
                number,
                name,
                marks,
            })
        })
        .collect()
}

/// `11:1` as its numbers.
fn parse_number(text: &str) -> Option<(u64, u64)> {
    let (major, minor) = text.split_once(':')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// The drive's name on the host, as Docker was given it
/// (`TANGIBLE_HOST_BLOCK_DEVICE`), which decides whether a restart finds
/// the drive once it is back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Given {
    /// Not said, as by a Compose file from before the worker was told.
    Unknown,
    /// A `/dev/disk/by-id` name, which udev makes from what the drive is,
    /// and so gives back to the drive however the kernel numbers it.
    ById(String),
    /// A kernel name, such as `/dev/sr0`, which finds the drive only if it
    /// comes back as that. Held without `/dev/`: `sr0`.
    Kernel(String),
    /// Any other name, such as a `/dev/disk/by-path` name or `/dev/cdrom`,
    /// which may name another drive, or none, once this one is back.
    Other(String),
}

impl Given {
    /// What `source`, the device mapping's source, is.
    #[must_use]
    pub fn parse(source: Option<&str>) -> Self {
        let Some(source) = source.map(str::trim).filter(|source| !source.is_empty()) else {
            return Self::Unknown;
        };
        if source
            .strip_prefix("/dev/disk/by-id/")
            .is_some_and(|name| !name.is_empty() && !name.contains('/'))
        {
            return Self::ById(source.to_owned());
        }
        if let Some(name) = source.strip_prefix("/dev/")
            && is_kernel_name(name)
        {
            return Self::Kernel(name.to_owned());
        }
        Self::Other(source.to_owned())
    }

    /// Whether a restart finds the drive once it is back as `name`, its
    /// kernel name.
    #[must_use]
    pub fn finds(&self, name: &str) -> bool {
        match self {
            Self::ById(_) => true,
            Self::Kernel(kernel) => kernel == name,
            Self::Unknown | Self::Other(_) => false,
        }
    }

    /// Whether a restart finds the drive however the kernel numbers it.
    #[must_use]
    pub const fn finds_any(&self) -> bool {
        matches!(self, Self::ById(_))
    }
}

/// An optical drive's kernel name: `sr` and a number.
fn is_kernel_name(name: &str) -> bool {
    name.strip_prefix("sr")
        .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
}

/// What a [`Follower`] sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sighting {
    /// The drive is where the container was given it.
    Here,
    /// It is gone, and no drive like it is listed, or more than one is.
    Gone,
    /// Back as `name`, its kernel name, and not yet for long enough.
    Settling {
        /// `sr1`.
        name: String,
        /// How long until it is.
        left: Duration,
    },
    /// Back as `name` for long enough: a restart gives it to this worker.
    Back {
        /// `sr1`.
        name: String,
    },
    /// Back as `name`, where the name Docker was given would not find it.
    OutOfReach {
        /// `sr1`.
        name: String,
    },
}

impl Sighting {
    /// Which kind it is, to say each change once.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Here => "here",
            Self::Gone => "gone",
            Self::Settling { .. } => "settling",
            Self::Back { .. } => "back",
            Self::OutOfReach { .. } => "out_of_reach",
        }
    }
}

/// Why a worker's drive cannot be followed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NotFollowed {
    /// The worker's device is not a block device, or is not there.
    #[error("{device} is not a block device")]
    NotABlockDevice {
        /// The worker's device.
        device: String,
    },
    /// The device tree does not say what the drive is.
    #[error("the kernel's device tree does not say what the drive at {device} is")]
    NotDescribed {
        /// The worker's device.
        device: String,
    },
    /// Another drive looks the same, so the two could not be told apart.
    #[error(
        "another drive on this machine looks the same as the one at {device}, so the two could \
         not be told apart when one comes back"
    )]
    LookAlike {
        /// The worker's device.
        device: String,
    },
}

/// Watches for the worker's drive to come back under another number.
#[derive(Debug)]
pub struct Follower {
    sys_root: PathBuf,
    /// The container's node's numbers: the drive's when the worker started.
    rdev: u64,
    /// What the tree said of the drive then.
    marks: DriveMarks,
    /// How Docker was told the drive's name.
    given: Given,
    /// How long it must be back before a restart.
    wait: Duration,
    /// The drive back under another number, and since when.
    back: Option<((u64, u64), Instant)>,
}

impl Follower {
    /// Follow the drive at `device`, the container's node, by what the tree
    /// at `sys_root` says of it now, after `recent_restarts` restarts made
    /// for it in the last hour.
    ///
    /// # Errors
    ///
    /// [`NotFollowed`] when the drive cannot be known again: its node is not
    /// a block device, the tree does not say what it is, or another drive
    /// looks the same.
    pub fn start(
        device: &Path,
        given: Given,
        sys_root: &Path,
        recent_restarts: usize,
    ) -> Result<Self, NotFollowed> {
        let named = device.display().to_string();
        let metadata = std::fs::metadata(device)
            .ok()
            .filter(|metadata| metadata.file_type().is_block_device())
            .ok_or_else(|| NotFollowed::NotABlockDevice {
                device: named.clone(),
            })?;
        Self::numbered(metadata.rdev(), &named, given, sys_root, recent_restarts)
    }

    /// [`Self::start`], for the drive numbered `rdev`, whose node is
    /// `device`.
    pub(crate) fn numbered(
        rdev: u64,
        device: &str,
        given: Given,
        sys_root: &Path,
        recent_restarts: usize,
    ) -> Result<Self, NotFollowed> {
        let (major, minor) = split_device_number(rdev);
        let marks = std::fs::canonicalize(sys_root.join(format!("dev/block/{major}:{minor}")))
            .ok()
            .and_then(|block| DriveMarks::of_block(&block))
            .ok_or_else(|| NotFollowed::NotDescribed {
                device: device.to_owned(),
            })?;
        let alike = listed(sys_root, major)
            .iter()
            .filter(|drive| drive.marks == marks)
            .count();
        if alike > 1 {
            return Err(NotFollowed::LookAlike {
                device: device.to_owned(),
            });
        }
        Ok(Self {
            sys_root: sys_root.to_path_buf(),
            rdev,
            marks,
            given,
            wait: settling_time(recent_restarts),
            back: None,
        })
    }

    /// This follower, waiting `wait` for a drive that is back. For tests,
    /// which cannot wait ten seconds.
    #[cfg(test)]
    pub(crate) const fn waiting(mut self, wait: Duration) -> Self {
        self.wait = wait;
        self
    }

    /// How long the drive must be back before a restart.
    #[must_use]
    pub const fn wait(&self) -> Duration {
        self.wait
    }

    /// How Docker was told the drive's name.
    #[must_use]
    pub const fn given(&self) -> &Given {
        &self.given
    }

    /// What the tree said of the drive when the worker started.
    #[must_use]
    pub const fn marks(&self) -> &DriveMarks {
        &self.marks
    }

    /// Look at the tree again, at `now`.
    pub fn look(&mut self, now: Instant) -> Sighting {
        if !number_is_gone(self.rdev, &self.sys_root) {
            self.back = None;
            return Sighting::Here;
        }
        let (major, _) = split_device_number(self.rdev);
        let mut alike = listed(&self.sys_root, major)
            .into_iter()
            .filter(|drive| drive.marks == self.marks);
        let (Some(drive), None) = (alike.next(), alike.next()) else {
            self.back = None;
            return Sighting::Gone;
        };
        if !self.given.finds(&drive.name) {
            self.back = None;
            return Sighting::OutOfReach { name: drive.name };
        }
        // Timed from when it came back under this number: a drive that drops
        // again while it is waited on starts the wait again.
        let since = match self.back {
            Some((number, since)) if number == drive.number => since,
            _ => {
                self.back = Some((drive.number, now));
                now
            }
        };
        let been = now.saturating_duration_since(since);
        if been >= self.wait {
            Sighting::Back { name: drive.name }
        } else {
            Sighting::Settling {
                name: drive.name,
                left: self.wait.saturating_sub(been),
            }
        }
    }
}

/// The restarts a worker made for its drive, on its state volume, where the
/// next process reads them.
#[derive(Debug, Clone)]
pub struct RestartLog {
    path: PathBuf,
}

impl RestartLog {
    /// A log kept at `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The restarts made within [`RESTARTS_COUNTED_FOR`] of `now`. A log that
    /// cannot be read counts none, which can only shorten a wait.
    #[must_use]
    pub fn recent(&self, now: OffsetDateTime) -> Vec<OffsetDateTime> {
        let mut text = String::new();
        let read = std::fs::File::open(&self.path)
            .and_then(|file| file.take(64 * 1024).read_to_string(&mut text));
        if read.is_err() {
            return Vec::new();
        }
        let Ok(times) = serde_json::from_str::<Vec<String>>(&text) else {
            return Vec::new();
        };
        times
            .iter()
            .filter_map(|time| OffsetDateTime::parse(time, &Rfc3339).ok())
            .filter(|time| *time <= now && now - *time < RESTARTS_COUNTED_FOR)
            .collect()
    }

    /// Add a restart made at `now`, keeping only those still counted.
    /// Temporary file then rename, so a crash leaves the old log or the new
    /// one.
    ///
    /// # Errors
    ///
    /// The I/O error when the log cannot be written.
    pub fn record(&self, now: OffsetDateTime) -> std::io::Result<()> {
        let mut times = self.recent(now);
        times.push(now);
        let times: Vec<String> = times
            .iter()
            .filter_map(|time| time.format(&Rfc3339).ok())
            .collect();
        let json = serde_json::to_string(&times).map_err(std::io::Error::other)?;
        let temporary = self.path.with_extension("part");
        std::fs::write(&temporary, json)?;
        std::fs::rename(&temporary, &self.path)
    }
}

/// A device tree for tests, laid out as the kernel lays one out.
#[cfg(test)]
#[allow(clippy::expect_used)]
pub(crate) mod test_tree {
    use std::path::Path;

    /// A device tree holding drives, as the kernel lays one out for a USB
    /// drive: the block device under its SCSI device, under the USB
    /// interface, under the USB device that carries the ids.
    pub(crate) struct Tree {
        dir: tempfile::TempDir,
    }

    impl Tree {
        pub(crate) fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(dir.path().join("dev/block")).expect("dev/block");
            // A machine's tree always lists other block devices, without
            // which nothing is said to be gone.
            let loop0 = dir.path().join("devices/virtual/block/loop0");
            std::fs::create_dir_all(&loop0).expect("loop0");
            std::os::unix::fs::symlink(&loop0, dir.path().join("dev/block/7:0")).expect("7:0");
            Self { dir }
        }

        pub(crate) fn root(&self) -> &Path {
            self.dir.path()
        }

        /// A drive at USB port `port`, as `sr{minor}`, with this serial.
        pub(crate) fn plug(&self, port: &str, minor: u64, serial: Option<&str>) {
            let usb = self
                .root()
                .join(format!("devices/pci0000:00/usb7/7-1/{port}"));
            let scsi = usb.join(format!(
                "{port}:1.0/host{minor}/target{minor}:0:0/{minor}:0:0:0"
            ));
            let block = scsi.join(format!("block/sr{minor}"));
            std::fs::create_dir_all(&block).expect("block");
            std::os::unix::fs::symlink(&scsi, block.join("device")).expect("device link");
            std::fs::write(scsi.join("vendor"), "Slimtype\n").expect("vendor");
            std::fs::write(scsi.join("model"), "DVD A DS8A8SH   \n").expect("model");
            std::fs::write(usb.join("idVendor"), "1132\n").expect("idVendor");
            std::fs::write(usb.join("idProduct"), "4511\n").expect("idProduct");
            if let Some(serial) = serial {
                std::fs::write(usb.join("serial"), format!("{serial}\n")).expect("serial");
            }
            // A hub above it carries ids of its own, and is not the drive.
            let hub = self.root().join("devices/pci0000:00/usb7/7-1");
            std::fs::write(hub.join("idVendor"), "214b\n").expect("hub idVendor");
            std::fs::write(hub.join("idProduct"), "7260\n").expect("hub idProduct");
            std::os::unix::fs::symlink(&block, self.root().join(format!("dev/block/11:{minor}")))
                .expect("number link");
        }

        /// The drive numbered `minor` gone from the tree, as when it drops.
        pub(crate) fn unplug(&self, minor: u64) {
            std::fs::remove_file(self.root().join(format!("dev/block/11:{minor}")))
                .expect("unlink");
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use test_tree::Tree;

    const SERIAL: &str = "413539303333313541444431";

    #[test]
    fn a_drive_is_known_by_its_scsi_names_and_its_usb_bridge() {
        let tree = Tree::new();
        tree.plug("7-1.3.1", 1, Some(SERIAL));
        let drives = listed(tree.root(), 11);
        assert_eq!(drives.len(), 1);
        assert_eq!(drives[0].name, "sr1");
        assert_eq!(drives[0].number, (11, 1));
        assert_eq!(
            drives[0].marks,
            DriveMarks {
                vendor: "Slimtype".to_owned(),
                model: "DVD A DS8A8SH".to_owned(),
                usb: Some(UsbMarks {
                    vendor_id: "1132".to_owned(),
                    product_id: "4511".to_owned(),
                    serial: Some(SERIAL.to_owned()),
                }),
            },
            "the bridge's ids, not the hub's"
        );
        assert!(
            listed(tree.root(), 8).is_empty(),
            "another major is not a drive"
        );
    }

    #[test]
    fn the_name_docker_was_given_decides_whether_a_restart_finds_the_drive() {
        let by_id = Given::parse(Some(
            "/dev/disk/by-id/usb-Slimtype_DVD_A_DS8A8SH_413539303333313541444431-0:0",
        ));
        assert!(matches!(by_id, Given::ById(_)));
        assert!(by_id.finds("sr1") && by_id.finds("sr0") && by_id.finds_any());

        let kernel = Given::parse(Some("/dev/sr0"));
        assert_eq!(kernel, Given::Kernel("sr0".to_owned()));
        assert!(kernel.finds("sr0"));
        assert!(
            !kernel.finds("sr1"),
            "sr0 no longer names a drive back as sr1"
        );
        assert!(!kernel.finds_any());

        for other in [
            "/dev/cdrom",
            "/dev/disk/by-path/pci-0000:c6:00.4-usb-0:1.3.1:1.0-scsi-0:0:0:0",
            "/dev/srx",
            "/dev/disk/by-id/",
        ] {
            let given = Given::parse(Some(other));
            assert!(matches!(given, Given::Other(_)), "{other}: {given:?}");
            assert!(!given.finds("sr0"), "{other}");
        }
        for unknown in [None, Some(""), Some("  ")] {
            assert_eq!(Given::parse(unknown), Given::Unknown);
            assert!(!Given::Unknown.finds("sr0"));
        }
    }

    #[test]
    fn the_wait_doubles_with_each_restart_in_the_hour_up_to_a_limit() {
        assert_eq!(settling_time(0), Duration::from_secs(10));
        assert_eq!(settling_time(1), Duration::from_secs(20));
        assert_eq!(settling_time(3), Duration::from_secs(80));
        assert_eq!(
            settling_time(7),
            LONGEST_WAIT,
            "1280 seconds, held to 15 minutes"
        );
        assert_eq!(settling_time(usize::MAX), LONGEST_WAIT);
    }

    #[test]
    fn the_restarts_made_in_the_last_hour_are_kept_and_counted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = RestartLog::new(dir.path().join("drive-restarts.json"));
        let now = OffsetDateTime::now_utc();
        assert!(log.recent(now).is_empty(), "no log counts none");

        log.record(now - time::Duration::minutes(90))
            .expect("record");
        log.record(now - time::Duration::minutes(30))
            .expect("record");
        log.record(now).expect("record");
        assert_eq!(
            log.recent(now).len(),
            2,
            "the one 90 minutes ago no longer counts"
        );
        let kept: Vec<String> = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("drive-restarts.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(kept.len(), 2, "and is not kept: {kept:?}");
        assert!(!dir.path().join("drive-restarts.part").exists());

        std::fs::write(dir.path().join("drive-restarts.json"), "not json").unwrap();
        assert!(
            log.recent(now).is_empty(),
            "a log that cannot be read counts none"
        );
    }

    /// A follower of the drive numbered 11:`minor`, as the worker starts
    /// one: a block device node needs privilege to make, so by its numbers.
    fn follower(tree: &Tree, minor: u64, given: Given, recent: usize) -> Follower {
        Follower::numbered((11 << 8) | minor, "/dev/sr0", given, tree.root(), recent)
            .expect("followed")
    }

    fn by_id() -> Given {
        Given::parse(Some(
            "/dev/disk/by-id/usb-Slimtype_DVD_A_DS8A8SH_413539303333313541444431-0:0",
        ))
    }

    #[test]
    fn a_drive_back_under_another_number_is_restarted_for_once_it_has_stayed() {
        let tree = Tree::new();
        tree.plug("7-1.4.1.1", 0, Some(SERIAL));
        let mut follower = follower(&tree, 0, by_id(), 0);
        let start = Instant::now();
        assert_eq!(follower.look(start), Sighting::Here);

        // It drops: 11:0 is gone and nothing like it is listed.
        tree.unplug(0);
        assert_eq!(follower.look(start), Sighting::Gone);

        // It comes back as sr1, on another port even, which the by-id name
        // follows.
        tree.plug("7-1.3.1", 1, Some(SERIAL));
        let at = start + Duration::from_secs(2);
        assert_eq!(
            follower.look(at),
            Sighting::Settling {
                name: "sr1".to_owned(),
                left: Duration::from_secs(10)
            }
        );
        assert_eq!(
            follower.look(at + Duration::from_secs(4)),
            Sighting::Settling {
                name: "sr1".to_owned(),
                left: Duration::from_secs(6)
            }
        );
        assert_eq!(
            follower.look(at + Duration::from_secs(10)),
            Sighting::Back {
                name: "sr1".to_owned()
            }
        );
    }

    #[test]
    fn a_drive_that_drops_again_while_it_is_waited_on_is_waited_on_afresh() {
        let tree = Tree::new();
        tree.plug("7-1.3.1", 0, Some(SERIAL));
        let mut follower = follower(&tree, 0, by_id(), 2);
        assert_eq!(
            follower.wait(),
            Duration::from_secs(40),
            "two restarts this hour"
        );
        let start = Instant::now();
        tree.unplug(0);
        tree.plug("7-1.3.1", 1, Some(SERIAL));
        assert!(matches!(follower.look(start), Sighting::Settling { .. }));

        // Gone again, then back as sr2: the wait starts over.
        tree.unplug(1);
        assert_eq!(
            follower.look(start + Duration::from_secs(30)),
            Sighting::Gone
        );
        tree.plug("7-1.3.1", 2, Some(SERIAL));
        let again = start + Duration::from_secs(35);
        assert!(matches!(follower.look(again), Sighting::Settling { .. }));
        assert!(matches!(
            follower.look(again + Duration::from_secs(39)),
            Sighting::Settling { .. }
        ));
        assert_eq!(
            follower.look(again + Duration::from_secs(40)),
            Sighting::Back {
                name: "sr2".to_owned()
            }
        );
    }

    #[test]
    fn another_drive_is_not_taken_for_this_one() {
        let tree = Tree::new();
        tree.plug("7-1.3.1", 0, Some(SERIAL));
        let mut follower = follower(&tree, 0, by_id(), 0);
        tree.unplug(0);
        // The same model, with another serial number: another drive.
        tree.plug("7-1.2", 1, Some("121220160204"));
        assert_eq!(follower.look(Instant::now()), Sighting::Gone);
    }

    #[test]
    fn a_drive_that_cannot_be_known_again_is_not_followed() {
        let tree = Tree::new();
        // A node that is not a block device.
        let file = tempfile::NamedTempFile::new().expect("file");
        assert!(matches!(
            Follower::start(file.path(), by_id(), tree.root(), 0),
            Err(NotFollowed::NotABlockDevice { .. })
        ));
        // A drive the tree says nothing of.
        assert!(matches!(
            Follower::numbered(0x0b00, "/dev/sr0", by_id(), tree.root(), 0),
            Err(NotFollowed::NotDescribed { .. })
        ));
        // Two drives alike, neither with a serial number.
        tree.plug("7-1.3.1", 0, None);
        tree.plug("7-1.2", 1, None);
        let words = Follower::numbered(0x0b00, "/dev/sr0", by_id(), tree.root(), 0)
            .expect_err("two alike")
            .to_string();
        assert!(words.contains("looks the same"), "{words}");
    }

    #[test]
    fn two_drives_alike_coming_back_are_not_told_apart() {
        let tree = Tree::new();
        tree.plug("7-1.3.1", 0, None);
        let mut follower = follower(&tree, 0, by_id(), 0);
        tree.unplug(0);
        tree.plug("7-1.3.1", 1, None);
        assert!(
            matches!(follower.look(Instant::now()), Sighting::Settling { .. }),
            "one alike is taken for it, as its by-id name would be"
        );
        tree.plug("7-1.2", 2, None);
        assert_eq!(
            follower.look(Instant::now()),
            Sighting::Gone,
            "two alike are not"
        );
    }

    #[test]
    fn a_drive_back_where_the_name_docker_was_given_would_not_find_it_is_left_to_a_person() {
        let tree = Tree::new();
        tree.plug("7-1.3.1", 0, Some(SERIAL));
        let mut by_number = follower(&tree, 0, Given::Kernel("sr0".to_owned()), 0);
        let mut unknown = follower(&tree, 0, Given::Unknown, 0);
        tree.unplug(0);
        tree.plug("7-1.3.1", 1, Some(SERIAL));
        let now = Instant::now() + Duration::from_mins(5);
        for follower in [&mut by_number, &mut unknown] {
            assert_eq!(
                follower.look(now),
                Sighting::OutOfReach {
                    name: "sr1".to_owned()
                },
                "{:?}",
                follower.given()
            );
        }
    }

    #[test]
    fn a_tree_that_cannot_be_read_says_the_drive_is_here() {
        // As connection::is_gone: nothing is said to be gone that cannot be
        // told.
        let tree = Tree::new();
        tree.plug("7-1.3.1", 0, Some(SERIAL));
        let mut follower = follower(&tree, 0, by_id(), 0);
        std::fs::remove_dir_all(tree.root().join("dev")).expect("remove");
        assert_eq!(follower.look(Instant::now()), Sighting::Here);
    }
}
