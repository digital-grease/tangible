// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Configured directories an operator may import from.
//!
//! The API never accepts a host path. It accepts the identifier of a root an
//! administrator configured, plus a relative path beneath it, and this turns
//! that pair into a real path or refuses.
//!
//! Refusing is the interesting half. A watched folder is the one place where a
//! path an untrusted actor influenced (a filename in a download directory),
//! meets the filesystem, so every way out of the root is closed explicitly:
//!
//! - the relative path must be relative, and must not name a parent;
//! - the resolved path must still be inside the root after the operating
//!   system has resolved every link;
//! - symlinks are refused rather than followed, because a link in a watched
//!   folder is exactly how a file outside it gets imported.
//!
//! The check is done against the canonical paths rather than by string
//! comparison. A prefix test on unresolved text is defeated by any link in the
//! middle of the path, which is the mistake this type exists to make
//! impossible.
//!
//! A root is read-only to Tangible unless the host's operator marked it
//! removable (`TANGIBLE_REMOVABLE_ROOTS`, ADR 0017), and then only
//! [`WatchRoots::remove_matching`] deletes in it: the files of one download
//! whose bytes the library holds, found by the same rules an import uses.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Component, Path, PathBuf};

use sha2::{Digest as _, Sha256};
use tokio::fs;
use tokio::io::AsyncReadExt as _;

/// Why a watched path could not be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WatchRootError {
    /// No root is configured under that identifier.
    ///
    /// Names the identifier, which the caller supplied, and never the paths
    /// that are configured: a caller guessing identifiers should not be able
    /// to map the host's filesystem.
    #[error("no watched root is configured as {path_id}")]
    UnknownRoot {
        /// What was asked for.
        path_id: String,
    },

    /// The relative path is not one this will resolve.
    #[error("that path is not usable: {reason}")]
    UnsafePath {
        /// Why, in terms the caller can act on.
        reason: &'static str,
    },

    /// Nothing is there.
    #[error("no such file beneath the watched root")]
    NotFound,

    /// The path exists but is not a regular file.
    #[error("that path is not a regular file")]
    NotAFile,

    /// The filesystem could not be read.
    #[error("the watched root could not be read")]
    Unreadable,

    /// A directory with nothing in it to import.
    #[error("that directory holds no files to import")]
    EmptyDirectory,

    /// Deleting was asked for in a root not marked removable.
    #[error("the watched root {path_id} is not one Tangible may delete in")]
    NotRemovable {
        /// What was asked for.
        path_id: String,
    },

    /// A directory past the bounds a disc's files fit in.
    #[error(
        "that directory holds more than {MAX_DIRECTORY_FILES} files or is nested deeper than {MAX_DIRECTORY_DEPTH} levels"
    )]
    DirectoryTooLarge,
}

/// Most files one directory import may hold. A 99-track CUE/BIN set with one
/// file per track, its sheet and some notes fits several times over.
pub const MAX_DIRECTORY_FILES: usize = 256;

/// Deepest a directory import may go below the directory named.
pub const MAX_DIRECTORY_DEPTH: usize = 4;

/// What a watched path names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchedSource {
    /// One regular file.
    File(PathBuf),
    /// A directory: every file in it, as a path relative to the directory
    /// with `/` separators, and where it is on disk.
    ///
    /// The files of one disc kept together, a CUE or TOC and the files it
    /// names, which become one artifact.
    Directory {
        /// The files, sorted by relative path.
        files: Vec<(String, PathBuf)>,
    },
}

/// The roots an administrator configured.
#[derive(Debug, Clone, Default)]
pub struct WatchRoots {
    roots: BTreeMap<String, PathBuf>,
    removable: BTreeSet<String>,
}

/// What deleting a download's files came to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Removed {
    /// Files deleted: their bytes were the library's.
    pub files_removed: usize,
    /// Their bytes.
    pub bytes_removed: u64,
    /// Files left: their bytes were not the library's.
    pub files_kept: usize,
}

impl WatchRoots {
    /// Roots from identifier and path pairs.
    #[must_use]
    pub fn new(roots: BTreeMap<String, PathBuf>) -> Self {
        Self {
            roots,
            removable: BTreeSet::new(),
        }
    }

    /// The same roots, with `ids` marked removable.
    ///
    /// # Errors
    ///
    /// The first id that names no configured root: a removable root that was
    /// meant and is not there would leave its downloads for ever, and one
    /// mistyped is better refused at start.
    pub fn with_removable<I, S>(mut self, ids: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for id in ids {
            let id = id.as_ref().trim();
            if id.is_empty() {
                continue;
            }
            if !self.roots.contains_key(id) {
                return Err(id.to_owned());
            }
            self.removable.insert(id.to_owned());
        }
        Ok(self)
    }

    /// Whether Tangible may delete in the root `path_id`.
    #[must_use]
    pub fn is_removable(&self, path_id: &str) -> bool {
        self.removable.contains(path_id)
    }

    /// The identifiers of the roots Tangible may delete in.
    #[must_use]
    pub fn removable_ids(&self) -> Vec<String> {
        self.removable.iter().cloned().collect()
    }

    /// Where the roots Tangible may delete in are, for the server's own
    /// checks (diagnostics asks whether each is writable). Never for the API.
    #[must_use]
    pub fn removable_paths(&self) -> Vec<PathBuf> {
        self.removable
            .iter()
            .filter_map(|id| self.roots.get(id).cloned())
            .collect()
    }

    /// Parse the `id=path,id=path` form used by configuration.
    ///
    /// Entries that name nothing are dropped rather than failing the whole
    /// setting: an operator with a trailing comma should get the roots they
    /// meant, not a server that will not start.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        let roots = value
            .split(',')
            .filter_map(|entry| {
                let (id, path) = entry.split_once('=')?;
                let id = id.trim();
                let path = path.trim();
                if id.is_empty() || path.is_empty() {
                    return None;
                }
                Some((id.to_owned(), PathBuf::from(path)))
            })
            .collect();
        Self::new(roots)
    }

    /// Whether any root is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// The configured identifiers, for showing an operator what to choose.
    ///
    /// Identifiers only. The paths are the host's business and are never
    /// exposed through the API.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.roots.keys().cloned().collect()
    }

    /// Whether a root exists under this identifier.
    #[must_use]
    pub fn contains(&self, path_id: &str) -> bool {
        self.roots.contains_key(path_id)
    }

    /// Resolve a relative path beneath a configured root.
    ///
    /// # Errors
    ///
    /// [`WatchRootError::UnknownRoot`] for an identifier that is not
    /// configured, [`WatchRootError::UnsafePath`] for anything that could
    /// leave the root, [`WatchRootError::NotFound`] or
    /// [`WatchRootError::NotAFile`] for a path that does not name a readable
    /// regular file.
    pub async fn resolve(&self, path_id: &str, relative: &str) -> Result<PathBuf, WatchRootError> {
        let root = self
            .roots
            .get(path_id)
            .ok_or_else(|| WatchRootError::UnknownRoot {
                path_id: path_id.to_owned(),
            })?;

        check_relative(relative)?;

        // The root itself is canonicalized too. Comparing against the
        // configured text would break the moment a root is reached through a
        // symlinked mount, which is ordinary on a NAS.
        let root = fs::canonicalize(root)
            .await
            .map_err(|_| WatchRootError::Unreadable)?;

        let candidate = root.join(relative);
        let resolved = match fs::canonicalize(&candidate).await {
            Ok(resolved) => resolved,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(WatchRootError::NotFound);
            }
            Err(_) => return Err(WatchRootError::Unreadable),
        };

        // After the operating system has resolved everything. A link pointing
        // out of the root fails here, which is the point.
        if !resolved.starts_with(&root) {
            return Err(WatchRootError::UnsafePath {
                reason: "it resolves outside the watched root",
            });
        }

        // Symlinks are refused rather than followed even when they stay
        // inside the root: a link is a second name for bytes the operator may
        // not have meant to offer, and refusing keeps the rule simple enough
        // to state.
        let metadata = fs::symlink_metadata(&candidate)
            .await
            .map_err(|_| WatchRootError::Unreadable)?;
        if metadata.file_type().is_symlink() {
            return Err(WatchRootError::UnsafePath {
                reason: "symbolic links are not followed",
            });
        }
        if !metadata.file_type().is_file() {
            return Err(WatchRootError::NotAFile);
        }

        Ok(resolved)
    }

    /// Resolve a relative path that may name a file or a directory.
    ///
    /// A directory is walked, and every file in it is resolved by
    /// [`Self::resolve`] in turn, so each one gets exactly the checks a file
    /// named on its own gets. Two more rules apply to a directory:
    ///
    /// - a symbolic link anywhere in it refuses the whole import, rather than
    ///   being followed or quietly left out, so the operator learns why;
    /// - names beginning with `.` are left out, which is where operating
    ///   systems put their own bookkeeping (`.DS_Store`, `._disc.bin`), none
    ///   of it part of a disc.
    ///
    /// # Errors
    ///
    /// As [`Self::resolve`], plus [`WatchRootError::EmptyDirectory`] and
    /// [`WatchRootError::DirectoryTooLarge`].
    pub async fn resolve_source(
        &self,
        path_id: &str,
        relative: &str,
    ) -> Result<WatchedSource, WatchRootError> {
        match self.resolve(path_id, relative).await {
            Ok(file) => return Ok(WatchedSource::File(file)),
            Err(WatchRootError::NotAFile) => {}
            Err(error) => return Err(error),
        }

        // Not a file, and not a link (resolve refused links before saying
        // not-a-file), so a directory or something stranger.
        let root = self
            .roots
            .get(path_id)
            .ok_or_else(|| WatchRootError::UnknownRoot {
                path_id: path_id.to_owned(),
            })?;
        let directory = root.join(relative);
        let metadata = fs::symlink_metadata(&directory)
            .await
            .map_err(|_| WatchRootError::Unreadable)?;
        if !metadata.is_dir() {
            return Err(WatchRootError::NotAFile);
        }

        let mut files = Vec::new();
        let mut pending = vec![(String::new(), 0_usize)];
        while let Some((below, depth)) = pending.pop() {
            let here = if below.is_empty() {
                directory.clone()
            } else {
                directory.join(&below)
            };
            let mut entries = fs::read_dir(&here)
                .await
                .map_err(|_| WatchRootError::Unreadable)?;
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|_| WatchRootError::Unreadable)?
            {
                let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
                    return Err(WatchRootError::UnsafePath {
                        reason: "a name in the directory is not UTF-8",
                    });
                };
                if name.starts_with('.') {
                    continue;
                }
                let inside = if below.is_empty() {
                    name
                } else {
                    format!("{below}/{name}")
                };
                let kind = entry
                    .file_type()
                    .await
                    .map_err(|_| WatchRootError::Unreadable)?;
                if kind.is_symlink() {
                    return Err(WatchRootError::UnsafePath {
                        reason: "the directory contains a symbolic link, and links are not followed",
                    });
                }
                if kind.is_dir() {
                    if depth + 1 > MAX_DIRECTORY_DEPTH {
                        return Err(WatchRootError::DirectoryTooLarge);
                    }
                    pending.push((inside, depth + 1));
                    continue;
                }
                let resolved = self
                    .resolve(path_id, &format!("{relative}/{inside}"))
                    .await?;
                files.push((inside, resolved));
                if files.len() > MAX_DIRECTORY_FILES {
                    return Err(WatchRootError::DirectoryTooLarge);
                }
            }
        }
        if files.is_empty() {
            return Err(WatchRootError::EmptyDirectory);
        }
        files.sort();
        Ok(WatchedSource::Directory { files })
    }
}

impl WatchRoots {
    /// Delete the download at `relative` beneath the removable root
    /// `path_id`: each regular file in it whose SHA-256 (lowercase hex) and
    /// length are in `library`, then the folders that leaves empty, the one
    /// named among them and those above it, up to but never the root (a
    /// client may name a job's one file inside a folder of its own).
    /// Everything else stays and is counted.
    ///
    /// The download is found exactly as an import finds it
    /// ([`Self::resolve_source`]): no links, nothing outside the root, the
    /// same bounds. Each file is hashed just before it is deleted. Names
    /// beginning with `.` are neither imported nor deleted.
    ///
    /// # Errors
    ///
    /// [`WatchRootError::NotRemovable`] for a root not marked removable; as
    /// [`Self::resolve_source`] otherwise ([`WatchRootError::NotFound`] when
    /// nothing is there); [`WatchRootError::Unreadable`] for a file that
    /// cannot be read or deleted.
    pub async fn remove_matching(
        &self,
        path_id: &str,
        relative: &str,
        library: &HashSet<(String, u64)>,
    ) -> Result<Removed, WatchRootError> {
        if !self.contains(path_id) {
            return Err(WatchRootError::UnknownRoot {
                path_id: path_id.to_owned(),
            });
        }
        if !self.is_removable(path_id) {
            return Err(WatchRootError::NotRemovable {
                path_id: path_id.to_owned(),
            });
        }
        let source = self.resolve_source(path_id, relative).await?;
        let root = self.canonical_root(path_id).await?;
        let (files, directory) = match source {
            WatchedSource::File(path) => (vec![(String::new(), path)], None),
            WatchedSource::Directory { files } => {
                let directory = self.resolve_directory(path_id, relative).await?;
                (files, Some(directory))
            }
        };
        let mut removed = Removed::default();
        for (_, path) in &files {
            // Looked at again: the walk was a moment ago.
            let metadata = fs::symlink_metadata(path)
                .await
                .map_err(|_| WatchRootError::Unreadable)?;
            if !metadata.file_type().is_file() {
                removed.files_kept += 1;
                continue;
            }
            let (digest, length) = hash_file(path)
                .await
                .map_err(|_| WatchRootError::Unreadable)?;
            if library.contains(&(digest, length)) {
                fs::remove_file(path)
                    .await
                    .map_err(|_| WatchRootError::Unreadable)?;
                removed.files_removed += 1;
                removed.bytes_removed += length;
            } else {
                removed.files_kept += 1;
            }
        }
        if let Some(directory) = directory {
            // Deepest first, so a folder emptied by its children goes too.
            // One that is not empty stays: removing a directory never
            // removes what is in it.
            let mut folders: BTreeSet<PathBuf> = BTreeSet::new();
            for (inside, _) in &files {
                let mut parent = Path::new(inside).parent();
                while let Some(folder) = parent.filter(|p| !p.as_os_str().is_empty()) {
                    folders.insert(directory.join(folder));
                    parent = folder.parent();
                }
            }
            let mut folders: Vec<PathBuf> = folders.into_iter().collect();
            folders.sort_by_key(|folder| std::cmp::Reverse(folder.components().count()));
            for folder in folders {
                let _ = fs::remove_dir(&folder).await;
            }
            prune_up(&root, &directory).await;
        } else if removed.files_removed == 1
            && let Some(parent) = files.first().and_then(|(_, path)| path.parent())
        {
            prune_up(&root, parent).await;
        }
        Ok(removed)
    }

    /// A root's real path.
    async fn canonical_root(&self, path_id: &str) -> Result<PathBuf, WatchRootError> {
        let root = self
            .roots
            .get(path_id)
            .ok_or_else(|| WatchRootError::UnknownRoot {
                path_id: path_id.to_owned(),
            })?;
        fs::canonicalize(root)
            .await
            .map_err(|_| WatchRootError::Unreadable)
    }

    /// The real path of a directory beneath a root, checked to be inside it
    /// and not itself a link, and never the root itself.
    async fn resolve_directory(
        &self,
        path_id: &str,
        relative: &str,
    ) -> Result<PathBuf, WatchRootError> {
        check_relative(relative)?;
        let root = self.canonical_root(path_id).await?;
        let candidate = root.join(relative);
        let metadata = fs::symlink_metadata(&candidate)
            .await
            .map_err(|_| WatchRootError::Unreadable)?;
        if metadata.file_type().is_symlink() {
            return Err(WatchRootError::UnsafePath {
                reason: "symbolic links are not followed",
            });
        }
        let resolved = fs::canonicalize(&candidate)
            .await
            .map_err(|_| WatchRootError::Unreadable)?;
        if resolved == root || !resolved.starts_with(&root) {
            return Err(WatchRootError::UnsafePath {
                reason: "it resolves outside the watched root",
            });
        }
        Ok(resolved)
    }
}

/// Remove `folder` and each folder above it that is left empty, stopping at
/// the first that is not, and never removing `root` or anything outside it.
async fn prune_up(root: &Path, folder: &Path) {
    let mut here = Some(folder);
    while let Some(folder) = here {
        if folder == root || !folder.starts_with(root) {
            return;
        }
        if fs::remove_dir(folder).await.is_err() {
            return;
        }
        here = folder.parent();
    }
}

/// A file's SHA-256, lowercase hex, and its length, read in 1 MiB pieces.
async fn hash_file(path: &Path) -> std::io::Result<(String, u64)> {
    let mut file = fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 20];
    let mut length = 0_u64;
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        length += read as u64;
    }
    Ok((hex::encode(hasher.finalize()), length))
}

/// Reject a relative path before it touches the filesystem.
///
/// Cheap, total, and independent of what is on disk: a path that names a
/// parent or starts at the root cannot be made safe by any amount of
/// canonicalization afterwards.
fn check_relative(relative: &str) -> Result<(), WatchRootError> {
    if relative.is_empty() {
        return Err(WatchRootError::UnsafePath {
            reason: "it is empty",
        });
    }
    if relative.contains('\0') {
        return Err(WatchRootError::UnsafePath {
            reason: "it contains a NUL byte",
        });
    }
    if relative.contains('\\') {
        return Err(WatchRootError::UnsafePath {
            reason: "backslashes are not path separators here",
        });
    }

    let path = Path::new(relative);
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {
                return Err(WatchRootError::UnsafePath {
                    reason: "it contains a . component",
                });
            }
            Component::ParentDir => {
                return Err(WatchRootError::UnsafePath {
                    reason: "it contains a .. component",
                });
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(WatchRootError::UnsafePath {
                    reason: "it must be relative",
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn roots(dir: &Path) -> WatchRoots {
        let mut map = BTreeMap::new();
        map.insert("incoming".to_owned(), dir.to_path_buf());
        WatchRoots::new(map)
    }

    #[test]
    fn configuration_parses_the_documented_form() {
        let roots = WatchRoots::parse("incoming=/srv/incoming,archive=/srv/archive");
        assert_eq!(
            roots.ids(),
            vec!["archive".to_owned(), "incoming".to_owned()]
        );
        assert!(roots.contains("incoming"));
        assert!(!roots.contains("elsewhere"));
    }

    #[test]
    fn a_stray_comma_does_not_cost_an_operator_their_roots() {
        let roots = WatchRoots::parse("incoming=/srv/incoming,");
        assert_eq!(roots.ids(), vec!["incoming".to_owned()]);
    }

    #[test]
    fn nothing_configured_is_an_empty_set_rather_than_a_failure() {
        assert!(WatchRoots::parse("").is_empty());
        assert!(WatchRoots::default().is_empty());
    }

    #[test]
    fn traversal_is_refused_before_the_filesystem_is_touched() {
        // The check has to be total: a path naming a parent cannot be made
        // safe by canonicalizing afterwards.
        for bad in [
            "../etc/passwd",
            "a/../../b",
            "/etc/passwd",
            "./a",
            "a\\b",
            "",
        ] {
            assert!(check_relative(bad).is_err(), "{bad:?} should be refused");
        }
        assert!(check_relative("Example Disc/disc.iso").is_ok());
    }

    #[tokio::test]
    async fn an_unknown_root_names_only_what_the_caller_asked_for() {
        // A caller guessing identifiers must not be able to map the host.
        let dir = tempfile::TempDir::new().expect("temp dir");
        let error = roots(dir.path())
            .resolve("elsewhere", "x.iso")
            .await
            .expect_err("must refuse");
        assert_eq!(
            error,
            WatchRootError::UnknownRoot {
                path_id: "elsewhere".to_owned()
            }
        );
        assert!(
            !error
                .to_string()
                .contains(&dir.path().display().to_string())
        );
    }

    #[tokio::test]
    async fn a_file_in_the_root_resolves() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        std::fs::write(dir.path().join("disc.iso"), b"x").expect("write");
        let resolved = roots(dir.path())
            .resolve("incoming", "disc.iso")
            .await
            .expect("resolve");
        assert!(resolved.ends_with("disc.iso"));
    }

    #[tokio::test]
    async fn a_file_in_a_subdirectory_resolves() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        std::fs::create_dir(dir.path().join("Example Disc")).expect("mkdir");
        std::fs::write(dir.path().join("Example Disc/disc.iso"), b"x").expect("write");
        assert!(
            roots(dir.path())
                .resolve("incoming", "Example Disc/disc.iso")
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_missing_file_is_reported_as_missing() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        assert_eq!(
            roots(dir.path())
                .resolve("incoming", "absent.iso")
                .await
                .expect_err("must refuse"),
            WatchRootError::NotFound
        );
    }

    #[tokio::test]
    async fn a_directory_is_not_a_file() {
        // Directory-tree imports are a later epic. Until then, saying so is
        // better than importing the first thing found inside.
        let dir = tempfile::TempDir::new().expect("temp dir");
        std::fs::create_dir(dir.path().join("BDMV")).expect("mkdir");
        assert_eq!(
            roots(dir.path())
                .resolve("incoming", "BDMV")
                .await
                .expect_err("must refuse"),
            WatchRootError::NotAFile
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn a_symlink_out_of_the_root_is_refused() {
        // The case the whole type exists for: a link in a watched folder is
        // how a file outside it gets imported.
        let dir = tempfile::TempDir::new().expect("temp dir");
        let outside = tempfile::TempDir::new().expect("temp dir");
        std::fs::write(outside.path().join("secret"), b"x").expect("write");
        std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("link.iso"))
            .expect("symlink");

        let error = roots(dir.path())
            .resolve("incoming", "link.iso")
            .await
            .expect_err("must refuse");
        assert!(
            matches!(error, WatchRootError::UnsafePath { .. }),
            "{error:?}"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn a_symlink_inside_the_root_is_refused_too() {
        // Refused even when it stays inside: a link is a second name for
        // bytes the operator may not have meant to offer, and the simple rule
        // is the one that stays correct.
        let dir = tempfile::TempDir::new().expect("temp dir");
        std::fs::write(dir.path().join("real.iso"), b"x").expect("write");
        std::os::unix::fs::symlink(dir.path().join("real.iso"), dir.path().join("link.iso"))
            .expect("symlink");

        assert!(
            roots(dir.path())
                .resolve("incoming", "link.iso")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn a_path_through_a_symlinked_directory_is_refused() {
        // The mistake a string prefix check makes: the text stays inside the
        // root and the bytes do not.
        let dir = tempfile::TempDir::new().expect("temp dir");
        let outside = tempfile::TempDir::new().expect("temp dir");
        std::fs::write(outside.path().join("secret.iso"), b"x").expect("write");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("elsewhere")).expect("symlink");

        assert!(
            roots(dir.path())
                .resolve("incoming", "elsewhere/secret.iso")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_file_is_still_a_file() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        std::fs::write(dir.path().join("disc.iso"), b"x").unwrap();
        assert!(matches!(
            roots(dir.path())
                .resolve_source("incoming", "disc.iso")
                .await,
            Ok(WatchedSource::File(_))
        ));
    }

    #[tokio::test]
    async fn a_directory_is_every_file_in_it_with_its_bookkeeping_left_out() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let disc = dir.path().join("Example Disc");
        std::fs::create_dir_all(disc.join("tracks")).unwrap();
        std::fs::write(disc.join("disc.cue"), b"cue").unwrap();
        std::fs::write(disc.join("tracks/track01.bin"), b"bin").unwrap();
        std::fs::write(disc.join(".DS_Store"), b"junk").unwrap();
        std::fs::write(disc.join("._disc.cue"), b"junk").unwrap();

        let Ok(WatchedSource::Directory { files }) = roots(dir.path())
            .resolve_source("incoming", "Example Disc")
            .await
        else {
            panic!("a directory");
        };
        let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, vec!["disc.cue", "tracks/track01.bin"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_link_anywhere_in_a_directory_refuses_the_whole_import() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let outside = tempfile::TempDir::new().expect("temp dir");
        std::fs::write(outside.path().join("secret"), b"no").unwrap();
        let disc = dir.path().join("disc");
        std::fs::create_dir(&disc).unwrap();
        std::fs::write(disc.join("disc.cue"), b"cue").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), disc.join("disc.bin")).unwrap();

        assert!(matches!(
            roots(dir.path()).resolve_source("incoming", "disc").await,
            Err(WatchRootError::UnsafePath { .. })
        ));
    }

    /// What the library holds of `contents`: their digests and lengths.
    fn library(contents: &[&[u8]]) -> HashSet<(String, u64)> {
        contents
            .iter()
            .map(|bytes| {
                (
                    hex::encode(Sha256::digest(bytes)),
                    u64::try_from(bytes.len()).unwrap(),
                )
            })
            .collect()
    }

    fn removable(dir: &Path) -> WatchRoots {
        roots(dir).with_removable(["incoming"]).unwrap()
    }

    #[test]
    fn only_a_configured_root_is_marked_removable() {
        let roots = WatchRoots::parse("incoming=/srv/incoming,archive=/srv/archive");
        let marked = roots.clone().with_removable([" incoming ", ""]).unwrap();
        assert!(marked.is_removable("incoming"));
        assert!(!marked.is_removable("archive"));
        assert_eq!(marked.removable_ids(), ["incoming"]);
        assert_eq!(
            roots.with_removable(["incoming", "nzbs"]).unwrap_err(),
            "nzbs"
        );
    }

    #[tokio::test]
    async fn a_download_loses_only_the_files_the_library_holds_and_the_folders_left_empty() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let job = dir.path().join("Disc (CUE)");
        std::fs::create_dir_all(job.join("extras/deeper")).unwrap();
        std::fs::write(job.join("disc.cue"), b"cue").unwrap();
        std::fs::write(job.join("disc.bin"), b"bin bytes").unwrap();
        std::fs::write(job.join("extras/deeper/track.bin"), b"track").unwrap();
        std::fs::write(job.join("extras/notes.nfo"), b"not imported").unwrap();
        std::fs::write(job.join(".DS_Store"), b"bookkeeping").unwrap();
        let held = library(&[b"cue", b"bin bytes", b"track"]);

        let removed = removable(dir.path())
            .remove_matching("incoming", "Disc (CUE)", &held)
            .await
            .unwrap();
        assert_eq!(
            removed,
            Removed {
                files_removed: 3,
                bytes_removed: 17,
                files_kept: 1,
            }
        );
        assert!(!job.join("disc.bin").exists());
        assert!(!job.join("extras/deeper").exists(), "emptied, so removed");
        assert!(job.join("extras/notes.nfo").exists(), "not the library's");
        assert!(job.join(".DS_Store").exists(), "never looked at");
        assert!(dir.path().exists(), "the root is never removed");

        // A job's one file in a folder of its own, named as the file: the
        // folder goes with it.
        let single = dir.path().join("single");
        std::fs::create_dir(&single).unwrap();
        std::fs::write(single.join("single.iso"), b"one").unwrap();
        let removed = removable(dir.path())
            .remove_matching("incoming", "single/single.iso", &library(&[b"one"]))
            .await
            .unwrap();
        assert_eq!(removed.files_removed, 1);
        assert!(!single.exists());

        // Everything the library's: the folder goes too, the root stays.
        let whole = dir.path().join("whole");
        std::fs::create_dir(&whole).unwrap();
        std::fs::write(whole.join("disc.iso"), b"iso").unwrap();
        let removed = removable(dir.path())
            .remove_matching("incoming", "whole", &library(&[b"iso"]))
            .await
            .unwrap();
        assert_eq!(removed.files_removed, 1);
        assert!(!whole.exists());
        assert!(dir.path().exists());
    }

    #[tokio::test]
    async fn a_file_changed_since_import_is_kept() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        std::fs::write(dir.path().join("disc.iso"), b"changed").unwrap();
        let removed = removable(dir.path())
            .remove_matching("incoming", "disc.iso", &library(&[b"original"]))
            .await
            .unwrap();
        assert_eq!(removed.files_removed, 0);
        assert_eq!(removed.files_kept, 1);
        assert!(dir.path().join("disc.iso").exists());

        // The same bytes but another length cannot happen; another length
        // recorded for the same digest is still not a match.
        let mut wrong_length = library(&[b"changed"]);
        wrong_length = wrong_length
            .into_iter()
            .map(|(digest, length)| (digest, length + 1))
            .collect();
        let removed = removable(dir.path())
            .remove_matching("incoming", "disc.iso", &wrong_length)
            .await
            .unwrap();
        assert_eq!(removed.files_removed, 0);
    }

    #[tokio::test]
    async fn nothing_is_deleted_outside_a_removable_root_or_through_a_link() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let outside = tempfile::TempDir::new().expect("temp dir");
        std::fs::write(dir.path().join("disc.iso"), b"iso").unwrap();
        std::fs::write(outside.path().join("other.iso"), b"iso").unwrap();
        let held = library(&[b"iso"]);

        // A root not marked removable.
        assert!(matches!(
            roots(dir.path())
                .remove_matching("incoming", "disc.iso", &held)
                .await,
            Err(WatchRootError::NotRemovable { .. })
        ));
        assert!(matches!(
            removable(dir.path())
                .remove_matching("elsewhere", "disc.iso", &held)
                .await,
            Err(WatchRootError::UnknownRoot { .. })
        ));
        assert!(dir.path().join("disc.iso").exists());

        // The root itself, and ways out of it.
        for relative in ["", ".", "..", "../other.iso", "/etc"] {
            assert!(
                removable(dir.path())
                    .remove_matching("incoming", relative, &held)
                    .await
                    .is_err(),
                "{relative:?}"
            );
        }

        // A link to a file outside, alone or in a folder.
        std::os::unix::fs::symlink(
            outside.path().join("other.iso"),
            dir.path().join("link.iso"),
        )
        .unwrap();
        let folder = dir.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("disc.iso"), b"iso").unwrap();
        std::os::unix::fs::symlink(outside.path().join("other.iso"), folder.join("other.iso"))
            .unwrap();
        for relative in ["link.iso", "folder"] {
            assert!(matches!(
                removable(dir.path())
                    .remove_matching("incoming", relative, &held)
                    .await,
                Err(WatchRootError::UnsafePath { .. })
            ));
        }
        // A folder that is itself a link.
        std::os::unix::fs::symlink(outside.path(), dir.path().join("linked")).unwrap();
        assert!(matches!(
            removable(dir.path())
                .remove_matching("incoming", "linked", &held)
                .await,
            Err(WatchRootError::UnsafePath { .. })
        ));
        assert!(outside.path().join("other.iso").exists());
        assert!(folder.join("disc.iso").exists(), "refused whole");

        // Nothing there.
        assert!(matches!(
            removable(dir.path())
                .remove_matching("incoming", "gone.iso", &held)
                .await,
            Err(WatchRootError::NotFound)
        ));
    }

    #[tokio::test]
    async fn an_empty_or_too_deep_directory_is_refused() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        std::fs::create_dir(dir.path().join("empty")).unwrap();
        std::fs::write(dir.path().join("empty/.hidden"), b"x").unwrap();
        assert_eq!(
            roots(dir.path()).resolve_source("incoming", "empty").await,
            Err(WatchRootError::EmptyDirectory)
        );

        let deep = dir.path().join("deep/a/b/c/d/e");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("f.bin"), b"x").unwrap();
        assert_eq!(
            roots(dir.path()).resolve_source("incoming", "deep").await,
            Err(WatchRootError::DirectoryTooLarge)
        );
    }

    #[tokio::test]
    async fn too_many_files_are_refused() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let many = dir.path().join("many");
        std::fs::create_dir(&many).unwrap();
        for n in 0..=MAX_DIRECTORY_FILES {
            std::fs::write(many.join(format!("{n}.bin")), b"x").unwrap();
        }
        assert_eq!(
            roots(dir.path()).resolve_source("incoming", "many").await,
            Err(WatchRootError::DirectoryTooLarge)
        );
    }
}
