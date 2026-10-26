// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! What the object tree holds, read from disk alone: the walk half of
//! mark-and-sweep (docs/plans/storage-integrity.md, phase S2).
//!
//! Every entry is sorted into one of three kinds. An object is a regular
//! file whose name is a canonical digest, filed under that digest's own
//! prefix directories. A partial file is anything an ingest left in
//! `incoming`. Everything else is unrecognized: counted, never treated as an
//! object, so nothing that merely sits in the tree could ever be collected
//! as one.

use std::path::Path;
use std::time::SystemTime;

use tangible_domain::Sha256Digest;

use crate::filesystem::{FilesystemStore, StorageError};
use crate::object_key::{ALGORITHM_DIR, INCOMING_DIR, OBJECTS_DIR};

/// One object found in the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredObject {
    /// The digest it is filed under. Not re-hashed: a scrub does that.
    pub digest: Sha256Digest,
    /// Its length on disk.
    pub size_bytes: u64,
    /// When its file was last written.
    pub modified: SystemTime,
}

/// A temporary file an ingest left in `incoming`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialFile {
    /// Its length.
    pub size_bytes: u64,
    /// When it was last written.
    pub modified: SystemTime,
}

/// Everything in the object tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    /// Objects, in digest order.
    pub objects: Vec<StoredObject>,
    /// Temporary files from ingests.
    pub partial_files: Vec<PartialFile>,
    /// Entries that are neither: how many.
    pub unrecognized_count: u64,
    /// And the bytes of those that are files.
    pub unrecognized_bytes: u64,
}

impl FilesystemStore {
    /// Walk the object tree and say what is in it.
    ///
    /// Reads directory entries and their metadata only, never an object's
    /// bytes, and follows no symbolic link. Runs on a blocking thread.
    ///
    /// # Errors
    ///
    /// [`StorageError::Io`] if any directory cannot be read: an inventory
    /// that skipped one would describe a smaller store than the real one.
    pub async fn inventory(&self) -> Result<Inventory, StorageError> {
        let objects = self.root().join(OBJECTS_DIR);
        tokio::task::spawn_blocking(move || walk(&objects))
            .await
            .map_err(|error| StorageError::Io {
                operation: "walking the object tree",
                path: OBJECTS_DIR.into(),
                source: std::io::Error::other(error.to_string()),
            })?
    }
}

fn walk(objects: &Path) -> Result<Inventory, StorageError> {
    let mut inventory = Inventory::default();
    for entry in entries(objects)? {
        let (name, metadata) = entry;
        match name.as_str() {
            ALGORITHM_DIR if metadata.is_dir() => {
                walk_digests(&objects.join(ALGORITHM_DIR), &mut inventory)?;
            }
            INCOMING_DIR if metadata.is_dir() => {
                for (_, metadata) in entries(&objects.join(INCOMING_DIR))? {
                    if metadata.is_file() {
                        inventory.partial_files.push(PartialFile {
                            size_bytes: metadata.len(),
                            modified: modified(&metadata),
                        });
                    } else {
                        inventory.unrecognized(&metadata);
                    }
                }
            }
            _ => inventory.unrecognized(&metadata),
        }
    }
    inventory
        .objects
        .sort_unstable_by_key(|object| object.digest);
    Ok(inventory)
}

/// `sha256/<first two>/<next two>/<digest>`, and nothing else.
fn walk_digests(algorithm: &Path, inventory: &mut Inventory) -> Result<(), StorageError> {
    for (first, metadata) in entries(algorithm)? {
        if !(metadata.is_dir() && is_prefix(&first)) {
            inventory.unrecognized(&metadata);
            continue;
        }
        for (second, metadata) in entries(&algorithm.join(&first))? {
            if !(metadata.is_dir() && is_prefix(&second)) {
                inventory.unrecognized(&metadata);
                continue;
            }
            for (name, metadata) in entries(&algorithm.join(&first).join(&second))? {
                match object_digest(&first, &second, &name) {
                    Some(digest) if metadata.is_file() => {
                        inventory.objects.push(StoredObject {
                            digest,
                            size_bytes: metadata.len(),
                            modified: modified(&metadata),
                        });
                    }
                    _ => inventory.unrecognized(&metadata),
                }
            }
        }
    }
    Ok(())
}

impl Inventory {
    fn unrecognized(&mut self, metadata: &std::fs::Metadata) {
        self.unrecognized_count += 1;
        if metadata.is_file() {
            self.unrecognized_bytes += metadata.len();
        }
    }
}

/// Two lowercase hex characters, as an object's prefix directories are.
fn is_prefix(name: &str) -> bool {
    name.len() == 2
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The digest a file names, when its name is a digest spelled exactly as
/// the store spells one and it sits under that digest's own prefixes.
fn object_digest(first: &str, second: &str, name: &str) -> Option<Sha256Digest> {
    let digest = name.parse::<Sha256Digest>().ok()?;
    let hex = digest.to_hex();
    (hex == name && hex.get(..2) == Some(first) && hex.get(2..4) == Some(second)).then_some(digest)
}

fn modified(metadata: &std::fs::Metadata) -> SystemTime {
    metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH)
}

/// A directory's entries, names and metadata, not following links. A name
/// that is not UTF-8 is kept lossily: it cannot be a digest either way.
fn entries(directory: &Path) -> Result<Vec<(String, std::fs::Metadata)>, StorageError> {
    let failed = |operation, source| StorageError::Io {
        operation,
        path: directory.to_path_buf(),
        source,
    };
    let mut found = Vec::new();
    for entry in std::fs::read_dir(directory).map_err(|e| failed("listing a directory", e))? {
        let entry = entry.map_err(|e| failed("walking a directory", e))?;
        let metadata = entry
            .metadata()
            .map_err(|e| failed("reading an entry's metadata", e))?;
        found.push((entry.file_name().to_string_lossy().into_owned(), metadata));
    }
    Ok(found)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::IngestLimits;

    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    async fn store() -> (tempfile::TempDir, FilesystemStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = FilesystemStore::open(dir.path().join("library"))
            .await
            .unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn objects_are_found_in_digest_order_with_their_sizes() {
        let (_dir, store) = store().await;
        let limits = IngestLimits {
            max_bytes: None,
            fsync: false,
        };
        let a = store
            .put_stream(Cursor::new(b"first".to_vec()), limits)
            .await
            .unwrap();
        let b = store
            .put_stream(Cursor::new(b"second object".to_vec()), limits)
            .await
            .unwrap();
        let inventory = store.inventory().await.unwrap();
        let mut expected = vec![(a.digest, 5), (b.digest, 13)];
        expected.sort_unstable();
        let found: Vec<_> = inventory
            .objects
            .iter()
            .map(|object| (object.digest, object.size_bytes))
            .collect();
        assert_eq!(found, expected);
        assert!(inventory.partial_files.is_empty());
        assert_eq!(inventory.unrecognized_count, 0);
    }

    #[tokio::test]
    async fn partial_files_are_counted_apart() {
        let (_dir, store) = store().await;
        let incoming = store.root().join("objects/incoming");
        std::fs::write(incoming.join("abc.part"), b"1234").unwrap();
        let inventory = store.inventory().await.unwrap();
        assert_eq!(inventory.partial_files.len(), 1);
        assert_eq!(inventory.partial_files[0].size_bytes, 4);
        assert!(inventory.objects.is_empty());
    }

    #[tokio::test]
    async fn anything_not_filed_exactly_as_an_object_is_unrecognized() {
        let (_dir, store) = store().await;
        let tree = store.root().join("objects/sha256");
        let right = tree.join("e3/b0");
        std::fs::create_dir_all(&right).unwrap();
        // Under the wrong prefix.
        let wrong = tree.join("aa/bb");
        std::fs::create_dir_all(&wrong).unwrap();
        std::fs::write(wrong.join(EMPTY), b"").unwrap();
        // Spelled in capitals.
        std::fs::write(right.join(EMPTY.to_uppercase()), b"xy").unwrap();
        // Not a digest at all.
        std::fs::write(right.join("notes.txt"), b"abc").unwrap();
        // A directory where an object would be.
        std::fs::create_dir(right.join(EMPTY)).unwrap();
        // A prefix that is not one, and a stray file beside the tree.
        std::fs::create_dir(tree.join("zz")).unwrap();
        std::fs::write(store.root().join("objects/stray"), b"1").unwrap();
        let inventory = store.inventory().await.unwrap();
        assert!(inventory.objects.is_empty(), "{inventory:?}");
        assert_eq!(inventory.unrecognized_count, 6);
        assert_eq!(inventory.unrecognized_bytes, 6);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_link_is_never_an_object() {
        let (dir, store) = store().await;
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, b"").unwrap();
        let right = store.root().join("objects/sha256/e3/b0");
        std::fs::create_dir_all(&right).unwrap();
        std::os::unix::fs::symlink(&target, right.join(EMPTY)).unwrap();
        let inventory = store.inventory().await.unwrap();
        assert!(inventory.objects.is_empty());
        assert_eq!(inventory.unrecognized_count, 1);
    }

    #[test]
    fn prefixes_are_two_lowercase_hex_characters() {
        assert!(is_prefix("0f"));
        assert!(!is_prefix("0F"));
        assert!(!is_prefix("0"));
        assert!(!is_prefix("0fa"));
        assert!(!is_prefix("g0"));
    }
}
