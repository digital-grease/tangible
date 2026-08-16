// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Staging areas.
//!
//! Bytes arriving from a source land in a scratch area before they become
//! canonical. Staging is separate from the library root so that nothing
//! half-received is ever reachable through the object tree, and so that a
//! staging area on a different filesystem cannot make the final publish
//! non-atomic.
//!
//! Promotion is copy-plus-verify by construction: a staged file is streamed
//! into the object store, which computes the digest from the bytes it actually
//! reads. Nothing is moved by rename across a filesystem boundary and nothing
//! is trusted to have arrived intact.
//!
//! Everything in a staging area is untrusted. A staged tree may have come from
//! an archive built by someone hostile, so paths are resolved through
//! [`LogicalPath`] and symlinks are refused rather than followed; following
//! one is how an extraction escapes its own directory.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use tangible_domain::LogicalPath;
use tokio::fs;

use crate::filesystem::{FilesystemStore, IngestLimits, Ingested, StorageError};

/// Which kind of work a staging area belongs to.
///
/// Kept apart so a sweep can treat them differently: an abandoned import is
/// diagnostic evidence worth keeping briefly, while an abandoned
/// materialization is pure scratch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StagingKind {
    /// Bytes being acquired for an import.
    Imports,
    /// Output being produced by a derivation.
    Derivatives,
    /// An artifact being reconstituted for a burn or a validator.
    Materializations,
}

impl StagingKind {
    /// Directory name beneath the staging root.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Imports => "imports",
            Self::Derivatives => "derivatives",
            Self::Materializations => "materializations",
        }
    }

    /// Every kind, for sweeps.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[Self::Imports, Self::Derivatives, Self::Materializations]
    }
}

/// Why a staging operation failed.
#[derive(Debug, thiserror::Error)]
pub enum StagingError {
    /// A path escaped, or tried to escape, its staging area.
    #[error("path {path} does not resolve inside its staging area")]
    Escape {
        /// The offending path.
        path: String,
    },

    /// A symlink was found in a staged tree.
    ///
    /// Refused rather than followed. A symlink in an extracted archive is the
    /// standard way to make a write land outside the directory it was
    /// supposed to be confined to.
    #[error("staged path {path} is a symlink, which is not accepted")]
    Symlink {
        /// The offending path.
        path: String,
    },

    /// The staging area does not exist.
    #[error("staging area {id} does not exist")]
    NoSuchArea {
        /// The area asked for.
        id: String,
    },

    /// An underlying object-store failure.
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// An I/O failure.
    #[error("{operation} failed for {path}")]
    Io {
        /// What was attempted.
        operation: &'static str,
        /// Path involved.
        path: PathBuf,
        /// Cause.
        #[source]
        source: std::io::Error,
    },
}

/// Owns the staging root and hands out isolated areas.
#[derive(Debug, Clone)]
pub struct StagingManager {
    root: PathBuf,
}

impl StagingManager {
    /// Open or create the staging root.
    ///
    /// # Errors
    ///
    /// [`StagingError::Io`] if the layout cannot be created.
    pub async fn open(root: impl Into<PathBuf>) -> Result<Self, StagingError> {
        let root = root.into();
        for kind in StagingKind::all() {
            let dir = root.join(kind.as_str());
            fs::create_dir_all(&dir)
                .await
                .map_err(|source| StagingError::Io {
                    operation: "creating a staging directory",
                    path: dir,
                    source,
                })?;
        }
        Ok(Self { root })
    }

    /// The staging root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Create an isolated area for one job.
    ///
    /// The identifier is used as a directory name, so it is checked rather
    /// than trusted: a job id containing a separator would otherwise place the
    /// area somewhere unintended.
    ///
    /// # Errors
    ///
    /// [`StagingError::Escape`] for an unusable identifier, or
    /// [`StagingError::Io`] if the directory cannot be created.
    pub async fn create(&self, kind: StagingKind, id: &str) -> Result<StagingArea, StagingError> {
        if id.is_empty()
            || id.contains('/')
            || id.contains('\\')
            || id.contains('\0')
            || id == "."
            || id == ".."
        {
            return Err(StagingError::Escape {
                path: id.to_owned(),
            });
        }

        let path = self.root.join(kind.as_str()).join(id);
        fs::create_dir_all(&path)
            .await
            .map_err(|source| StagingError::Io {
                operation: "creating a staging area",
                path: path.clone(),
                source,
            })?;

        Ok(StagingArea {
            path,
            id: id.to_owned(),
            kind,
        })
    }

    /// Reopen an existing area, e.g. after a restart.
    ///
    /// # Errors
    ///
    /// [`StagingError::NoSuchArea`] if it is gone.
    pub async fn reopen(&self, kind: StagingKind, id: &str) -> Result<StagingArea, StagingError> {
        let path = self.root.join(kind.as_str()).join(id);
        match fs::metadata(&path).await {
            Ok(metadata) if metadata.is_dir() => Ok(StagingArea {
                path,
                id: id.to_owned(),
                kind,
            }),
            _ => Err(StagingError::NoSuchArea { id: id.to_owned() }),
        }
    }

    /// Identifiers of every existing area of one kind.
    ///
    /// Recovery walks this after a restart to find work that was interrupted.
    ///
    /// # Errors
    ///
    /// [`StagingError::Io`] if the directory cannot be read.
    pub async fn list(&self, kind: StagingKind) -> Result<Vec<String>, StagingError> {
        let dir = self.root.join(kind.as_str());
        let mut entries = fs::read_dir(&dir)
            .await
            .map_err(|source| StagingError::Io {
                operation: "listing staging areas",
                path: dir.clone(),
                source,
            })?;

        let mut found = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|source| StagingError::Io {
                operation: "walking the staging directory",
                path: dir.clone(),
                source,
            })?
        {
            if entry.path().is_dir()
                && let Some(name) = entry.file_name().to_str()
            {
                found.push(name.to_owned());
            }
        }
        found.sort();
        Ok(found)
    }

    /// Delete an area and everything in it.
    ///
    /// # Errors
    ///
    /// [`StagingError::Io`] if removal fails for a reason other than absence.
    pub async fn discard(&self, kind: StagingKind, id: &str) -> Result<bool, StagingError> {
        let path = self.root.join(kind.as_str()).join(id);
        match fs::remove_dir_all(&path).await {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(source) => Err(StagingError::Io {
                operation: "discarding a staging area",
                path,
                source,
            }),
        }
    }
}

/// One job's isolated scratch directory.
#[derive(Debug, Clone)]
pub struct StagingArea {
    path: PathBuf,
    id: String,
    kind: StagingKind,
}

impl StagingArea {
    /// Absolute path of the area.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The job identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Which kind of area this is.
    #[must_use]
    pub const fn kind(&self) -> StagingKind {
        self.kind
    }

    /// Resolve a logical path to a real one inside this area.
    ///
    /// [`LogicalPath`] already guarantees the path is relative and free of
    /// traversal, so the join cannot escape lexically. This additionally
    /// refuses a path whose parents or target are symlinks, because a symlink
    /// planted by an extracted archive escapes at the filesystem level rather
    /// than in the string.
    ///
    /// # Errors
    ///
    /// [`StagingError::Symlink`] if any component is a symlink, or
    /// [`StagingError::Escape`] if resolution somehow leaves the area.
    pub async fn resolve(&self, logical: &LogicalPath) -> Result<PathBuf, StagingError> {
        let mut current = self.path.clone();
        for segment in logical.segments() {
            current.push(segment);
            // symlink_metadata does not follow, which is the whole point:
            // metadata() would report the target and hide the link.
            if let Ok(metadata) = fs::symlink_metadata(&current).await
                && metadata.file_type().is_symlink()
            {
                return Err(StagingError::Symlink {
                    path: logical.to_string(),
                });
            }
        }

        if !current.starts_with(&self.path) {
            return Err(StagingError::Escape {
                path: logical.to_string(),
            });
        }
        Ok(current)
    }

    /// Create the parent directories for a logical path.
    ///
    /// # Errors
    ///
    /// [`StagingError::Symlink`], [`StagingError::Escape`], or
    /// [`StagingError::Io`].
    pub async fn prepare_parent(&self, logical: &LogicalPath) -> Result<PathBuf, StagingError> {
        let target = self.resolve(logical).await?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|source| StagingError::Io {
                    operation: "creating a staged directory",
                    path: parent.to_path_buf(),
                    source,
                })?;
        }
        Ok(target)
    }

    /// Write bytes to a staged path, creating parents as needed.
    ///
    /// # Errors
    ///
    /// [`StagingError::Symlink`], [`StagingError::Escape`], or
    /// [`StagingError::Io`].
    pub async fn write(
        &self,
        logical: &LogicalPath,
        contents: &[u8],
    ) -> Result<PathBuf, StagingError> {
        let target = self.prepare_parent(logical).await?;
        fs::write(&target, contents)
            .await
            .map_err(|source| StagingError::Io {
                operation: "writing a staged file",
                path: target.clone(),
                source,
            })?;
        Ok(target)
    }

    /// Every file in the area, as logical paths, sorted.
    ///
    /// Symlinks are reported as an error rather than skipped: a staged tree
    /// containing one has not been safely extracted, and silently ignoring it
    /// would let the rest be promoted as if nothing were wrong.
    ///
    /// # Errors
    ///
    /// [`StagingError::Symlink`] if the tree contains one, or
    /// [`StagingError::Io`] on a read failure.
    pub async fn entries(&self) -> Result<Vec<LogicalPath>, StagingError> {
        let mut found = Vec::new();
        let mut stack = vec![self.path.clone()];

        while let Some(dir) = stack.pop() {
            let mut entries = fs::read_dir(&dir)
                .await
                .map_err(|source| StagingError::Io {
                    operation: "walking a staging area",
                    path: dir.clone(),
                    source,
                })?;
            while let Some(entry) =
                entries
                    .next_entry()
                    .await
                    .map_err(|source| StagingError::Io {
                        operation: "reading a staging entry",
                        path: dir.clone(),
                        source,
                    })?
            {
                let path = entry.path();
                let file_type = entry.file_type().await.map_err(|source| StagingError::Io {
                    operation: "inspecting a staging entry",
                    path: path.clone(),
                    source,
                })?;

                if file_type.is_symlink() {
                    return Err(StagingError::Symlink {
                        path: self.relative_string(&path),
                    });
                }
                if file_type.is_dir() {
                    stack.push(path);
                    continue;
                }
                let relative = self.relative_string(&path);
                match LogicalPath::parse(&relative) {
                    Ok(logical) => found.push(logical),
                    Err(_) => return Err(StagingError::Escape { path: relative }),
                }
            }
        }

        found.sort();
        Ok(found)
    }

    fn relative_string(&self, path: &Path) -> String {
        path.strip_prefix(&self.path)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// Stream a staged file into the object store.
    ///
    /// This is the copy-plus-verify step. The store computes the digest from
    /// the bytes it reads, so a staged file that was truncated or altered
    /// after arrival is filed under what it actually contains rather than what
    /// was expected, and the caller compares.
    ///
    /// The staged file is left in place; the caller discards the area once
    /// every component has been promoted and the manifest published.
    ///
    /// # Errors
    ///
    /// [`StagingError::Symlink`], [`StagingError::Escape`],
    /// [`StagingError::Io`], or [`StagingError::Storage`].
    pub async fn promote(
        &self,
        logical: &LogicalPath,
        store: &FilesystemStore,
        limits: IngestLimits,
    ) -> Result<Ingested, StagingError> {
        let source = self.resolve(logical).await?;
        let file = fs::File::open(&source)
            .await
            .map_err(|error| StagingError::Io {
                operation: "opening a staged file",
                path: source.clone(),
                source: error,
            })?;
        Ok(store.put_stream(file, limits).await?)
    }
}
