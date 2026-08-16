// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Filesystem content-addressed store.
//!
//! Implements the documented ingestion algorithm. Two storage rules shape
//! everything here:
//!
//! - **A partial file is never visible at a canonical object path.** Bytes go
//!   to a temporary file inside the objects tree, are fsynced, and are then
//!   atomically renamed into place. A crash can leave a stray temporary file,
//!   which is recoverable; it can never leave a truncated object under a
//!   digest that claims to describe it.
//! - **A supplied digest is never trusted.** The digest is computed while
//!   streaming and the object is stored under what was actually read, so a
//!   caller cannot cause bytes to be filed under someone else's name.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tangible_domain::Sha256Digest;
use tokio::fs;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::object_key::{ALGORITHM_DIR, INCOMING_DIR, OBJECTS_DIR, ObjectKey};

/// Marker file recording the on-disk layout version.
pub const VERSION_FILE: &str = "storage-version";

/// Layout version this build reads and writes.
pub const STORAGE_VERSION: &str = "1";

/// Buffer size for streaming copies.
///
/// Large enough that syscall overhead is negligible on multi-gigabyte images,
/// small enough that many concurrent imports do not exhaust memory. Images are
/// never loaded whole.
const COPY_BUFFER_BYTES: usize = 128 * 1024;

/// Failures from the storage layer.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The library root could not be prepared.
    #[error("could not prepare the library root at {path}")]
    Root {
        /// Path involved.
        path: PathBuf,
        /// Underlying cause.
        #[source]
        source: std::io::Error,
    },

    /// The on-disk layout was written by an incompatible version.
    #[error("library at {path} has storage version {found}, this build expects {STORAGE_VERSION}")]
    IncompatibleVersion {
        /// Library root.
        path: PathBuf,
        /// Version found on disk.
        found: String,
    },

    /// An I/O operation failed.
    #[error("{operation} failed for {path}")]
    Io {
        /// What was being attempted.
        operation: &'static str,
        /// Path involved.
        path: PathBuf,
        /// Underlying cause.
        #[source]
        source: std::io::Error,
    },

    /// The stream exceeded the configured limit.
    ///
    /// Reported rather than truncated: a partial image stored as if whole
    /// would be far worse than a failed import.
    #[error("stream exceeded the {limit} byte limit")]
    TooLarge {
        /// The limit that was hit.
        limit: u64,
    },

    /// A stored object's bytes no longer hash to its name.
    #[error("object {expected} is corrupt: its contents hash to {actual}")]
    Corrupt {
        /// Digest the object is filed under.
        expected: String,
        /// Digest its bytes actually produce.
        actual: String,
    },

    /// The requested object is not in the store.
    #[error("object {digest} is not present")]
    NotFound {
        /// Digest requested.
        digest: String,
    },
}

/// Limits applied while ingesting a stream.
#[derive(Debug, Clone, Copy)]
pub struct IngestLimits {
    /// Maximum bytes accepted. `None` means unlimited, which is only
    /// appropriate for a trusted local path.
    pub max_bytes: Option<u64>,
    /// Whether to fsync before renaming into place.
    ///
    /// On by default. Turning it off makes tests and throwaway imports much
    /// faster but risks an object that exists in the directory entry while its
    /// contents are still in the page cache after a power loss.
    pub fsync: bool,
}

impl Default for IngestLimits {
    fn default() -> Self {
        Self {
            max_bytes: None,
            fsync: true,
        }
    }
}

/// What happened when a stream was ingested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ingested {
    /// Digest computed from the bytes actually read.
    pub digest: Sha256Digest,
    /// Number of bytes read.
    pub size_bytes: u64,
    /// Whether an object with this digest already existed.
    ///
    /// `true` means the content deduplicated against an existing object and
    /// the temporary file was discarded.
    pub deduplicated: bool,
}

/// Metadata about a stored object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectStat {
    /// Digest the object is filed under.
    pub digest: Sha256Digest,
    /// Size on disk.
    pub size_bytes: u64,
}

/// A content-addressed store backed by a local filesystem.
#[derive(Debug, Clone)]
pub struct FilesystemStore {
    root: PathBuf,
}

impl FilesystemStore {
    /// Open or create a library root.
    ///
    /// Creates the layout if absent, and refuses a root written by an
    /// incompatible version rather than risking misinterpretation.
    ///
    /// # Errors
    ///
    /// [`StorageError::Root`] if the layout cannot be created, or
    /// [`StorageError::IncompatibleVersion`] if the marker disagrees.
    pub async fn open(root: impl Into<PathBuf>) -> Result<Self, StorageError> {
        let root = root.into();

        for directory in [
            root.join(OBJECTS_DIR).join(ALGORITHM_DIR),
            root.join(OBJECTS_DIR).join(INCOMING_DIR),
            root.join("manifests"),
            root.join("quarantine"),
        ] {
            fs::create_dir_all(&directory)
                .await
                .map_err(|source| StorageError::Root {
                    path: directory.clone(),
                    source,
                })?;
        }

        let version_path = root.join(VERSION_FILE);
        match fs::read_to_string(&version_path).await {
            Ok(found) => {
                let found = found.trim();
                if found != STORAGE_VERSION {
                    return Err(StorageError::IncompatibleVersion {
                        path: root,
                        found: found.to_owned(),
                    });
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                fs::write(&version_path, STORAGE_VERSION)
                    .await
                    .map_err(|source| StorageError::Root {
                        path: version_path,
                        source,
                    })?;
            }
            Err(source) => {
                return Err(StorageError::Root {
                    path: version_path,
                    source,
                });
            }
        }

        Ok(Self { root })
    }

    /// The library root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Absolute path of an object.
    #[must_use]
    pub fn object_path(&self, digest: &Sha256Digest) -> PathBuf {
        self.root.join(ObjectKey::new(*digest).relative_path())
    }

    /// Stream bytes into the store, computing the digest as they arrive.
    ///
    /// The caller does not say what the digest will be; it is discovered. That
    /// is what makes "never trust a client-provided digest" structural rather
    /// than a rule someone has to remember.
    ///
    /// # Errors
    ///
    /// [`StorageError::TooLarge`] if the limit is exceeded, or
    /// [`StorageError::Io`] on any filesystem failure.
    pub async fn put_stream<R>(
        &self,
        mut reader: R,
        limits: IngestLimits,
    ) -> Result<Ingested, StorageError>
    where
        R: AsyncRead + Unpin + Send,
    {
        // Temporary name inside the objects tree, so the later rename stays
        // within one filesystem and is therefore atomic.
        let incoming = self
            .root
            .join(OBJECTS_DIR)
            .join(INCOMING_DIR)
            .join(format!("{}.part", uuid::Uuid::now_v7()));

        let outcome = self
            .stream_to_incoming(&mut reader, &incoming, limits)
            .await;

        // Any failure must leave nothing behind. A stray .part file is
        // harmless but pointless, and on the size-limit path it could be huge.
        let (digest, size_bytes) = match outcome {
            Ok(pair) => pair,
            Err(error) => {
                let _ = fs::remove_file(&incoming).await;
                return Err(error);
            }
        };

        let destination = self.object_path(&digest);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|source| StorageError::Io {
                    operation: "creating the object directory",
                    path: parent.to_path_buf(),
                    source,
                })?;
        }

        // Deduplication: identical bytes are stored once. The existing object
        // is authoritative and is left untouched, so an in-flight reader of it
        // cannot be disturbed by a concurrent import of the same content.
        if fs::metadata(&destination).await.is_ok() {
            fs::remove_file(&incoming)
                .await
                .map_err(|source| StorageError::Io {
                    operation: "discarding a deduplicated temporary file",
                    path: incoming.clone(),
                    source,
                })?;
            return Ok(Ingested {
                digest,
                size_bytes,
                deduplicated: true,
            });
        }

        fs::rename(&incoming, &destination)
            .await
            .map_err(|source| StorageError::Io {
                operation: "publishing the object",
                path: destination.clone(),
                source,
            })?;

        tracing::debug!(digest = %digest, size_bytes, "stored object");

        Ok(Ingested {
            digest,
            size_bytes,
            deduplicated: false,
        })
    }

    /// Copy the stream to `incoming`, hashing and counting as it goes.
    async fn stream_to_incoming<R>(
        &self,
        reader: &mut R,
        incoming: &Path,
        limits: IngestLimits,
    ) -> Result<(Sha256Digest, u64), StorageError>
    where
        R: AsyncRead + Unpin + Send,
    {
        let mut file = fs::File::create(incoming)
            .await
            .map_err(|source| StorageError::Io {
                operation: "creating a temporary object file",
                path: incoming.to_path_buf(),
                source,
            })?;

        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
        let mut total: u64 = 0;

        loop {
            let read = reader
                .read(&mut buffer)
                .await
                .map_err(|source| StorageError::Io {
                    operation: "reading the source stream",
                    path: incoming.to_path_buf(),
                    source,
                })?;
            if read == 0 {
                break;
            }

            total += read as u64;
            if let Some(limit) = limits.max_bytes
                && total > limit
            {
                return Err(StorageError::TooLarge { limit });
            }

            let chunk = &buffer[..read];
            hasher.update(chunk);
            file.write_all(chunk)
                .await
                .map_err(|source| StorageError::Io {
                    operation: "writing a temporary object file",
                    path: incoming.to_path_buf(),
                    source,
                })?;
        }

        file.flush().await.map_err(|source| StorageError::Io {
            operation: "flushing a temporary object file",
            path: incoming.to_path_buf(),
            source,
        })?;

        if limits.fsync {
            // Without this the rename can be durable while the contents are
            // not, leaving an object that exists but is empty after a power
            // loss.
            file.sync_all().await.map_err(|source| StorageError::Io {
                operation: "syncing a temporary object file",
                path: incoming.to_path_buf(),
                source,
            })?;
        }

        let digest = Sha256Digest::from_bytes(hasher.finalize().into());
        Ok((digest, total))
    }

    /// Metadata for a stored object, or `None` if absent.
    ///
    /// # Errors
    ///
    /// [`StorageError::Io`] if the object exists but cannot be inspected.
    pub async fn stat(&self, digest: &Sha256Digest) -> Result<Option<ObjectStat>, StorageError> {
        let path = self.object_path(digest);
        match fs::metadata(&path).await {
            Ok(metadata) => Ok(Some(ObjectStat {
                digest: *digest,
                size_bytes: metadata.len(),
            })),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(source) => Err(StorageError::Io {
                operation: "reading object metadata",
                path,
                source,
            }),
        }
    }

    /// Read a byte range from an object.
    ///
    /// Ranges exist so a burn worker can stage a large image in pieces and a
    /// download can be resumed, without any caller reading a whole image into
    /// memory.
    ///
    /// # Errors
    ///
    /// [`StorageError::NotFound`] if the object is absent, or
    /// [`StorageError::Io`] on a read failure.
    pub async fn read_range(
        &self,
        digest: &Sha256Digest,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, StorageError> {
        let path = self.object_path(digest);
        let mut file = fs::File::open(&path).await.map_err(|source| {
            if source.kind() == ErrorKind::NotFound {
                StorageError::NotFound {
                    digest: digest.to_hex(),
                }
            } else {
                StorageError::Io {
                    operation: "opening an object",
                    path: path.clone(),
                    source,
                }
            }
        })?;

        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|source| StorageError::Io {
                operation: "seeking within an object",
                path: path.clone(),
                source,
            })?;

        let mut buffer = vec![0_u8; length];
        let mut filled = 0;
        while filled < length {
            let read =
                file.read(&mut buffer[filled..])
                    .await
                    .map_err(|source| StorageError::Io {
                        operation: "reading an object range",
                        path: path.clone(),
                        source,
                    })?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        buffer.truncate(filled);
        Ok(buffer)
    }

    /// Re-hash an object and confirm it still matches its name.
    ///
    /// This is the integrity scrub primitive: bit rot, a bad disk, or a
    /// careless edit all show up as a digest mismatch.
    ///
    /// # Errors
    ///
    /// [`StorageError::NotFound`], [`StorageError::Corrupt`] if the contents
    /// no longer hash to the key, or [`StorageError::Io`].
    pub async fn verify(&self, digest: &Sha256Digest) -> Result<ObjectStat, StorageError> {
        let path = self.object_path(digest);
        let mut file = fs::File::open(&path).await.map_err(|source| {
            if source.kind() == ErrorKind::NotFound {
                StorageError::NotFound {
                    digest: digest.to_hex(),
                }
            } else {
                StorageError::Io {
                    operation: "opening an object for verification",
                    path: path.clone(),
                    source,
                }
            }
        })?;

        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
        let mut total: u64 = 0;
        loop {
            let read = file
                .read(&mut buffer)
                .await
                .map_err(|source| StorageError::Io {
                    operation: "reading an object for verification",
                    path: path.clone(),
                    source,
                })?;
            if read == 0 {
                break;
            }
            total += read as u64;
            hasher.update(&buffer[..read]);
        }

        let actual = Sha256Digest::from_bytes(hasher.finalize().into());
        if actual == *digest {
            Ok(ObjectStat {
                digest: *digest,
                size_bytes: total,
            })
        } else {
            Err(StorageError::Corrupt {
                expected: digest.to_hex(),
                actual: actual.to_hex(),
            })
        }
    }

    /// Delete an object.
    ///
    /// The store does not know about references; refusing to delete something
    /// still in use is the database's job via its foreign keys. This is the
    /// sweep half of mark-and-sweep and must only be called with an object the
    /// planner has already established is unreferenced.
    ///
    /// Returns whether anything was removed.
    ///
    /// # Errors
    ///
    /// [`StorageError::Io`] if removal fails for a reason other than absence.
    pub async fn remove(&self, digest: &Sha256Digest) -> Result<bool, StorageError> {
        let path = self.object_path(digest);
        match fs::remove_file(&path).await {
            Ok(()) => {
                tracing::debug!(digest = %digest, "removed object");
                Ok(true)
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(source) => Err(StorageError::Io {
                operation: "removing an object",
                path,
                source,
            }),
        }
    }

    /// Delete leftover temporary files from interrupted ingests.
    ///
    /// Safe at any time: a `.part` file is never referenced by anything, and
    /// an ingest in flight holds its own uniquely named file. Returns how many
    /// were removed.
    ///
    /// # Errors
    ///
    /// [`StorageError::Io`] if the incoming directory cannot be read.
    pub async fn clean_incoming(&self) -> Result<usize, StorageError> {
        let incoming = self.root.join(OBJECTS_DIR).join(INCOMING_DIR);
        let mut entries = fs::read_dir(&incoming)
            .await
            .map_err(|source| StorageError::Io {
                operation: "listing the incoming directory",
                path: incoming.clone(),
                source,
            })?;

        let mut removed = 0;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|source| StorageError::Io {
                operation: "walking the incoming directory",
                path: incoming.clone(),
                source,
            })?
        {
            if fs::remove_file(entry.path()).await.is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }
}
