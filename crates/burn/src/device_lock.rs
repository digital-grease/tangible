// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Exclusive claim on the drive a worker writes with.
//!
//! Two processes writing one drive is not a race that produces a wrong
//! number. It produces a physically ruined disc, and a drive that may need a
//! power cycle before it will accept another.
//!
//! The database already stops two *attempts* sharing a drive, and that is a
//! different guarantee than it looks: two workers configured with the same
//! device node are two drive records for one piece of hardware, and nothing
//! in the schema can see that they are the same. The remaining check has to
//! happen where the hardware is, which is here.
//!
//! Two layers, because they answer different questions:
//!
//! * a set of the drives this process has claimed, so a worker configured
//!   twice against one drive is told exactly that rather than being told
//!   somebody else has it;
//! * an advisory lock on the device node, which is what excludes another
//!   process: a second container with the same device passed through, or a
//!   worker an operator started by hand. This is the convention systemd
//!   documents for block devices, so udev also leaves the node alone while
//!   the lock is held.
//!
//! Advisory means it excludes programs that take it. Another Tangible worker
//! will; `dd` will not. That is the right trade: the failure being prevented
//! is a misconfigured deployment, not a hostile one.

use std::collections::HashSet;
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

/// The drives this process has claimed, by resolved path.
static HELD: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// The claimed set, recovering rather than propagating a poisoned lock.
///
/// A panic while the set was borrowed leaves a set of paths, not a half
/// applied invariant, so there is nothing to protect a caller from. Taking a
/// worker down over it would be worse than the fault that poisoned it.
fn held() -> MutexGuard<'static, HashSet<PathBuf>> {
    HELD.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Why a drive could not be claimed.
#[derive(Debug, thiserror::Error)]
pub enum DeviceLockError {
    /// This worker already holds it.
    ///
    /// Two drives configured with one device node, which is a configuration
    /// error rather than contention.
    #[error("this worker already holds {path}")]
    AlreadyHeld {
        /// The drive as configured.
        path: PathBuf,
    },

    /// Another process holds it.
    #[error("another process holds {path}")]
    HeldElsewhere {
        /// The drive as configured.
        path: PathBuf,
    },

    /// It could not be opened at all.
    ///
    /// A device node that is not there is the usual cause: an alias naming a
    /// drive the container was not given.
    #[error("{path} could not be claimed")]
    Unavailable {
        /// The drive as configured.
        path: PathBuf,
        /// Cause.
        #[source]
        source: std::io::Error,
    },
}

/// An exclusive claim on one drive, released when this is dropped.
///
/// Held for as long as the worker runs rather than for the length of a burn.
/// A second worker on one drive is a mistake in a deployment, and the moment
/// to discover it is when that worker starts, not when it has a disc in the
/// laser.
#[derive(Debug)]
pub struct DeviceLock {
    /// The alias as configured, which is what an operator recognises.
    path: PathBuf,
    /// What the alias pointed at when the claim was made.
    resolved: PathBuf,
    /// Holds the advisory lock. The kernel releases it when this closes,
    /// including when the process dies without unwinding, which is what makes
    /// a crashed worker's drive claimable again.
    _file: File,
}

impl DeviceLock {
    /// Claim a drive.
    ///
    /// # Errors
    ///
    /// [`DeviceLockError::AlreadyHeld`] if this process has already claimed
    /// it, [`DeviceLockError::HeldElsewhere`] if another process has, or
    /// [`DeviceLockError::Unavailable`] if it cannot be opened.
    pub fn acquire(path: impl Into<PathBuf>) -> Result<Self, DeviceLockError> {
        let path = path.into();

        // Resolved before anything is compared, because an alias is a symlink
        // an operator points at a device node: /dev/disc-block and /dev/sr0
        // are one drive and must collide.
        let resolved =
            std::fs::canonicalize(&path).map_err(|source| DeviceLockError::Unavailable {
                path: path.clone(),
                source,
            })?;

        if !held().insert(resolved.clone()) {
            return Err(DeviceLockError::AlreadyHeld { path });
        }

        match lock_exclusively(&resolved, &path) {
            Ok(file) => Ok(Self {
                path,
                resolved,
                _file: file,
            }),
            Err(error) => {
                // Registered before locking so two threads racing here cannot
                // both reach the lock, which means an unregister on every way
                // out of it.
                held().remove(&resolved);
                Err(error)
            }
        }
    }

    /// The drive as configured.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the alias still names the drive that was claimed.
    ///
    /// Worth asking again before a write. An alias that now resolves
    /// somewhere else means the drive was swapped or re-enumerated, and the
    /// exclusivity this worker believes it has is over a device it is no
    /// longer the one writing to.
    #[must_use]
    pub fn identity_intact(&self) -> bool {
        std::fs::canonicalize(&self.path).is_ok_and(|now| now == self.resolved)
    }
}

impl Drop for DeviceLock {
    fn drop(&mut self) {
        held().remove(&self.resolved);
    }
}

/// Open the node and take the advisory lock, or say who has it.
fn lock_exclusively(resolved: &Path, path: &Path) -> Result<File, DeviceLockError> {
    let file = open_for_lock(resolved).map_err(|source| DeviceLockError::Unavailable {
        path: path.to_path_buf(),
        source,
    })?;

    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(DeviceLockError::HeldElsewhere {
            path: path.to_path_buf(),
        }),
        Err(TryLockError::Error(source)) => Err(DeviceLockError::Unavailable {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Open a device node without waiting on the medium in it.
///
/// O_NONBLOCK is the whole reason this is not a plain open: an optical drive
/// with an empty tray refuses an ordinary one, and a worker that cannot claim
/// an empty drive is a worker that cannot wait for somebody to put a disc in
/// it. Read-only and not exclusive, so the claim does not stand in the way of
/// the engine opening the same drive to write.
#[cfg(unix)]
fn open_for_lock(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    File::options()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

/// Open a device node.
///
/// Optical drives are reached through a Linux container, so this exists to
/// keep the crate building rather than to be used.
#[cfg(not(unix))]
fn open_for_lock(path: &Path) -> std::io::Result<File> {
    File::options().read(true).open(path)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A file standing in for a device node.
    ///
    /// The lock works on whatever the alias names, which is deliberate: the
    /// engine tests write to a file target rather than to a disc, and two of
    /// those must exclude each other exactly as two drives would.
    fn node(dir: &tempfile::TempDir, name: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, b"").expect("create the node");
        path
    }

    #[test]
    fn a_drive_can_be_claimed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = node(&dir, "sr0");

        let lock = DeviceLock::acquire(&path).expect("claim");

        assert_eq!(lock.path(), path);
        assert!(lock.identity_intact());
    }

    #[test]
    fn a_second_claim_by_this_worker_is_refused() {
        // Two drives configured with one device node. The operator needs to
        // hear that it is their own worker holding it, not a mystery process.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = node(&dir, "sr0");
        let _first = DeviceLock::acquire(&path).expect("claim");

        let second = DeviceLock::acquire(&path);

        assert!(matches!(second, Err(DeviceLockError::AlreadyHeld { .. })));
    }

    #[test]
    fn releasing_a_drive_lets_it_be_claimed_again() {
        // A worker that stops must not leave the drive unusable until the
        // machine is rebooted.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = node(&dir, "sr0");

        drop(DeviceLock::acquire(&path).expect("claim"));

        DeviceLock::acquire(&path).expect("claim it again");
    }

    #[test]
    fn a_drive_that_is_not_there_is_refused_rather_than_ignored() {
        // The alias naming a device the container was not given. Carrying on
        // unlocked would mean the one configuration mistake that matters
        // silently disabling the protection against it.
        let dir = tempfile::tempdir().expect("tempdir");

        let result = DeviceLock::acquire(dir.path().join("absent"));

        assert!(matches!(result, Err(DeviceLockError::Unavailable { .. })));
    }

    #[test]
    fn the_claim_excludes_another_open_of_the_node() {
        // The half that stops a second *process*. A separate open in this
        // process is what one would get: the lock belongs to the open file
        // description, not to the process holding it.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = node(&dir, "sr0");
        let lock = DeviceLock::acquire(&path).expect("claim");

        let elsewhere = File::options().read(true).open(&path).expect("open");
        assert!(matches!(
            elsewhere.try_lock(),
            Err(TryLockError::WouldBlock)
        ));

        drop(lock);
        let after = File::options().read(true).open(&path).expect("open");
        assert!(after.try_lock().is_ok(), "the lock outlived the claim");
    }

    #[cfg(unix)]
    #[test]
    fn two_aliases_for_one_drive_are_one_claim() {
        // /dev/disc-block is a symlink an operator points at /dev/sr0.
        // Comparing the text of the two would call them different drives.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = node(&dir, "sr0");
        let alias = dir.path().join("disc-block");
        std::os::unix::fs::symlink(&path, &alias).expect("symlink");
        let _first = DeviceLock::acquire(&path).expect("claim");

        let second = DeviceLock::acquire(&alias);

        assert!(matches!(second, Err(DeviceLockError::AlreadyHeld { .. })));
    }

    #[cfg(unix)]
    #[test]
    fn an_alias_pointed_at_a_different_drive_is_no_longer_intact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = node(&dir, "sr0");
        let second = node(&dir, "sr1");
        let alias = dir.path().join("disc-block");
        std::os::unix::fs::symlink(&first, &alias).expect("symlink");
        let lock = DeviceLock::acquire(&alias).expect("claim");
        assert!(lock.identity_intact());

        std::fs::remove_file(&alias).expect("unlink");
        std::os::unix::fs::symlink(&second, &alias).expect("re-point");

        assert!(
            !lock.identity_intact(),
            "a swapped drive read as the same one"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_vanished_drive_is_not_intact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = node(&dir, "sr0");
        let lock = DeviceLock::acquire(&path).expect("claim");

        std::fs::remove_file(&path).expect("unlink");

        assert!(!lock.identity_intact());
    }
}
