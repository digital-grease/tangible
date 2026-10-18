// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! What a storage directory looks like from here: there, writable, and how
//! much room is left.
//!
//! Diagnostics only. Nothing here decides whether an import or a burn may
//! proceed; those check what they need as they go.

use std::path::{Path, PathBuf};

/// A directory, as diagnostics saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryCheck {
    /// Whether the path exists and is a directory.
    pub present: bool,
    /// Whether a file could be created and removed in it. `None` when it was
    /// not tried because the directory is missing.
    pub writable: Option<bool>,
    /// Bytes an unprivileged process may still write, and the filesystem's
    /// size. `None` when the filesystem could not be asked.
    pub space: Option<Space>,
}

/// Room on the filesystem holding a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    /// Bytes available to an unprivileged process.
    pub free_bytes: u64,
    /// The filesystem's size in bytes.
    pub total_bytes: u64,
}

/// Look at `directory`. Writability is tried by creating and removing a
/// uniquely named empty file in `probe_in`, a subdirectory where stray
/// temporary files are expected, or in `directory` itself when it is `None`.
pub async fn check_directory(directory: &Path, probe_in: Option<&Path>) -> DirectoryCheck {
    let present = tokio::fs::metadata(directory)
        .await
        .is_ok_and(|metadata| metadata.is_dir());
    if !present {
        return DirectoryCheck {
            present,
            writable: None,
            space: None,
        };
    }
    let probe_dir = probe_in.unwrap_or(directory);
    let writable = Some(try_write(probe_dir).await);
    let owned = directory.to_path_buf();
    let space = tokio::task::spawn_blocking(move || space_of(&owned))
        .await
        .ok()
        .flatten();
    DirectoryCheck {
        present,
        writable,
        space,
    }
}

/// Whether an empty file can be created in `directory` and removed again.
async fn try_write(directory: &Path) -> bool {
    let probe: PathBuf = directory.join(format!(
        ".tangible-diagnostics-{}",
        uuid::Uuid::now_v7().simple()
    ));
    let created = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .await
        .is_ok();
    if created && let Err(error) = tokio::fs::remove_file(&probe).await {
        tracing::warn!(error = %error, "diagnostics could not remove its probe file");
    }
    created
}

/// The filesystem's free and total bytes.
fn space_of(directory: &Path) -> Option<Space> {
    let stat = rustix::fs::statvfs(directory).ok()?;
    let unit = if stat.f_frsize > 0 {
        stat.f_frsize
    } else {
        stat.f_bsize
    };
    Some(Space {
        free_bytes: stat.f_bavail.saturating_mul(unit),
        total_bytes: stat.f_blocks.saturating_mul(unit),
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_writable_directory_is_present_writable_and_has_room() {
        let dir = tempfile::tempdir().expect("tempdir");
        let check = check_directory(dir.path(), None).await;
        assert!(check.present);
        assert_eq!(check.writable, Some(true));
        let space = check
            .space
            .expect("statvfs answers for a temporary directory");
        assert!(space.total_bytes >= space.free_bytes);
        assert!(space.total_bytes > 0);
        // The probe leaves nothing behind.
        let mut entries = std::fs::read_dir(dir.path()).expect("read_dir");
        assert!(entries.next().is_none());
    }

    #[tokio::test]
    async fn a_missing_directory_is_not_probed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let check = check_directory(&dir.path().join("absent"), None).await;
        assert_eq!(
            check,
            DirectoryCheck {
                present: false,
                writable: None,
                space: None,
            }
        );
    }

    #[tokio::test]
    async fn a_file_is_not_a_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").expect("write");
        assert!(!check_directory(&file, None).await.present);
    }

    #[tokio::test]
    async fn the_probe_goes_where_it_is_told() {
        let dir = tempfile::tempdir().expect("tempdir");
        let check = check_directory(dir.path(), Some(&dir.path().join("missing"))).await;
        assert!(check.present);
        assert_eq!(check.writable, Some(false));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_read_only_directory_is_not_writable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).expect("mkdir");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).expect("chmod");
        let check = check_directory(&locked, None).await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        // Root writes through permissions, so this only means something
        // when the tests are not run as root.
        // The directory is owned by whoever runs the tests.
        let owner =
            std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(dir.path()).expect("metadata"));
        if owner != 0 {
            assert_eq!(check.writable, Some(false));
        }
    }
}
