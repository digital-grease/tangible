// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Filesystem CAS behaviour.
//!
//! These run against a real temporary directory rather than a mock, because
//! the properties worth testing here (atomic publication, deduplication,
//! corruption detection) are properties of the filesystem interaction.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::Path;

use tangible_domain::Sha256Digest;
use tangible_storage::{FilesystemStore, IngestLimits, StorageError};
use tempfile::TempDir;

/// SHA-256 of the empty input.
const EMPTY_DIGEST: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// fsync is off in tests: it is the single largest cost here and the property
/// it protects (durability across power loss) is not what these assert.
fn fast() -> IngestLimits {
    IngestLimits {
        max_bytes: None,
        fsync: false,
    }
}

async fn store() -> (TempDir, FilesystemStore) {
    let dir = TempDir::new().expect("temp dir");
    let store = FilesystemStore::open(dir.path())
        .await
        .expect("open the store");
    (dir, store)
}

fn count_files(root: &Path) -> usize {
    fn walk(path: &Path, count: &mut usize) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, count);
            } else {
                *count += 1;
            }
        }
    }
    let mut count = 0;
    walk(root, &mut count);
    count
}

#[tokio::test]
async fn a_round_trip_preserves_bytes_exactly() {
    let (_dir, store) = store().await;
    let payload = b"the disc image bytes".to_vec();

    let stored = store
        .put_stream(payload.as_slice(), fast())
        .await
        .expect("store");
    assert_eq!(stored.size_bytes, payload.len() as u64);
    assert!(!stored.deduplicated);

    let read = store
        .read_range(&stored.digest, 0, payload.len())
        .await
        .expect("read");
    assert_eq!(read, payload);
}

#[tokio::test]
async fn the_digest_is_computed_from_the_bytes_not_supplied() {
    // A supplied digest is never trusted. The API has no parameter for a
    // caller-asserted digest, so bytes cannot be filed under a name they do
    // not hash to.
    let (_dir, store) = store().await;
    let stored = store.put_stream(&b""[..], fast()).await.expect("store");
    assert_eq!(stored.digest.to_hex(), EMPTY_DIGEST);
}

#[tokio::test]
async fn identical_content_is_stored_once() {
    let (dir, store) = store().await;
    let payload = b"duplicate me".to_vec();

    let first = store
        .put_stream(payload.as_slice(), fast())
        .await
        .expect("first");
    let objects_before = count_files(&dir.path().join("objects"));

    let second = store
        .put_stream(payload.as_slice(), fast())
        .await
        .expect("second");

    assert_eq!(first.digest, second.digest);
    assert!(!first.deduplicated);
    assert!(second.deduplicated, "the second store must deduplicate");
    assert_eq!(
        count_files(&dir.path().join("objects")),
        objects_before,
        "deduplication must not add a second copy on disk"
    );
}

#[tokio::test]
async fn different_content_produces_different_objects() {
    let (_dir, store) = store().await;
    let a = store.put_stream(&b"alpha"[..], fast()).await.expect("a");
    let b = store.put_stream(&b"beta"[..], fast()).await.expect("b");
    assert_ne!(a.digest, b.digest);
    assert_ne!(store.object_path(&a.digest), store.object_path(&b.digest));
}

#[tokio::test]
async fn exceeding_the_size_limit_leaves_nothing_behind() {
    // The important half is not the rejection, it is that a rejected stream
    // does not leave a partial object or a stray temporary file, which for a
    // large image could be many gigabytes.
    let (dir, store) = store().await;
    let limits = IngestLimits {
        max_bytes: Some(8),
        fsync: false,
    };

    let error = store
        .put_stream(&b"far more than eight bytes"[..], limits)
        .await
        .expect_err("must reject");
    assert!(matches!(error, StorageError::TooLarge { limit: 8 }));

    assert_eq!(
        count_files(&dir.path().join("objects")),
        0,
        "a rejected stream must leave no object and no temporary file"
    );
}

#[tokio::test]
async fn a_partial_object_is_never_visible_at_its_canonical_path() {
    // Simulated by checking that the only file under objects/ after a
    // successful store is the finished object, and that nothing is left in
    // incoming/. The temporary file lives inside the objects tree so that the
    // rename is atomic, which makes this worth asserting explicitly.
    let (dir, store) = store().await;
    let stored = store
        .put_stream(&b"complete content"[..], fast())
        .await
        .expect("store");

    let incoming = dir.path().join("objects").join("incoming");
    assert_eq!(
        count_files(&incoming),
        0,
        "no temporary file may survive a successful store"
    );
    assert!(store.object_path(&stored.digest).exists());
}

#[tokio::test]
async fn verification_accepts_an_intact_object() {
    let (_dir, store) = store().await;
    let stored = store
        .put_stream(&b"intact"[..], fast())
        .await
        .expect("store");
    let stat = store.verify(&stored.digest).await.expect("verify");
    assert_eq!(stat.size_bytes, 6);
}

#[tokio::test]
async fn verification_detects_corruption() {
    // The integrity-scrub primitive. Bit rot and a careless edit look the
    // same from here, and both must be caught.
    let (_dir, store) = store().await;
    let stored = store
        .put_stream(&b"original content"[..], fast())
        .await
        .expect("store");

    // Corrupt the object behind the store's back.
    std::fs::write(store.object_path(&stored.digest), b"tampered content!").expect("tamper");

    let error = store
        .verify(&stored.digest)
        .await
        .expect_err("corruption must be detected");
    match error {
        StorageError::Corrupt { expected, actual } => {
            assert_eq!(expected, stored.digest.to_hex());
            assert_ne!(actual, expected);
        }
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

#[tokio::test]
async fn reading_a_range_returns_only_that_range() {
    let (_dir, store) = store().await;
    let payload = b"0123456789".to_vec();
    let stored = store
        .put_stream(payload.as_slice(), fast())
        .await
        .expect("store");

    let middle = store.read_range(&stored.digest, 3, 4).await.expect("range");
    assert_eq!(middle, b"3456");
}

#[tokio::test]
async fn a_range_past_the_end_is_truncated_not_an_error() {
    // A reader asking for more than remains gets what exists. Erroring would
    // make resumable transfers awkward for no safety benefit.
    let (_dir, store) = store().await;
    let stored = store
        .put_stream(&b"short"[..], fast())
        .await
        .expect("store");
    let read = store
        .read_range(&stored.digest, 3, 100)
        .await
        .expect("range");
    assert_eq!(read, b"rt");
}

#[tokio::test]
async fn absent_objects_are_reported_distinctly() {
    let (_dir, store) = store().await;
    let missing: Sha256Digest = EMPTY_DIGEST.parse().expect("digest");

    assert!(store.stat(&missing).await.expect("stat").is_none());
    assert!(matches!(
        store.read_range(&missing, 0, 1).await,
        Err(StorageError::NotFound { .. })
    ));
    assert!(matches!(
        store.verify(&missing).await,
        Err(StorageError::NotFound { .. })
    ));
}

#[tokio::test]
async fn removing_an_absent_object_is_not_an_error() {
    // Garbage collection must be safely re-runnable after a partial sweep.
    let (_dir, store) = store().await;
    let missing: Sha256Digest = EMPTY_DIGEST.parse().expect("digest");
    assert!(!store.remove(&missing).await.expect("remove"));
}

#[tokio::test]
async fn removal_deletes_the_object() {
    let (_dir, store) = store().await;
    let stored = store
        .put_stream(&b"transient"[..], fast())
        .await
        .expect("store");

    assert!(store.remove(&stored.digest).await.expect("remove"));
    assert!(store.stat(&stored.digest).await.expect("stat").is_none());
    assert!(!store.object_path(&stored.digest).exists());
}

#[tokio::test]
async fn an_existing_object_is_never_overwritten_by_a_duplicate_import() {
    // The existing object is authoritative, so a concurrent reader of it
    // cannot be disturbed by someone importing the same content.
    let (_dir, store) = store().await;
    let payload = b"stable bytes".to_vec();
    let first = store
        .put_stream(payload.as_slice(), fast())
        .await
        .expect("first");

    let path = store.object_path(&first.digest);
    let before = std::fs::metadata(&path).expect("metadata");
    let inode_before = file_identity(&before);

    store
        .put_stream(payload.as_slice(), fast())
        .await
        .expect("second");

    let after = std::fs::metadata(&path).expect("metadata");
    assert_eq!(
        inode_before,
        file_identity(&after),
        "a duplicate import must leave the original file in place"
    );
}

#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.ino()
}

#[cfg(not(unix))]
fn file_identity(metadata: &std::fs::Metadata) -> u64 {
    metadata.len()
}

#[tokio::test]
async fn interrupted_ingests_can_be_swept_up() {
    let (dir, store) = store().await;
    let incoming = dir.path().join("objects").join("incoming");
    std::fs::write(incoming.join("abandoned.part"), b"leftover").expect("write");

    assert_eq!(store.clean_incoming().await.expect("clean"), 1);
    assert_eq!(count_files(&incoming), 0);
}

#[tokio::test]
async fn reopening_an_existing_library_succeeds() {
    let dir = TempDir::new().expect("temp dir");
    let first = FilesystemStore::open(dir.path()).await.expect("open");
    let stored = first
        .put_stream(&b"persisted"[..], fast())
        .await
        .expect("store");
    drop(first);

    let second = FilesystemStore::open(dir.path()).await.expect("reopen");
    assert!(second.stat(&stored.digest).await.expect("stat").is_some());
}

#[tokio::test]
async fn an_incompatible_layout_version_is_refused() {
    // Better to refuse than to misinterpret a layout written by a future
    // version.
    let dir = TempDir::new().expect("temp dir");
    FilesystemStore::open(dir.path()).await.expect("open");
    std::fs::write(dir.path().join("storage-version"), "99").expect("write");

    let error = FilesystemStore::open(dir.path())
        .await
        .expect_err("must refuse");
    assert!(matches!(error, StorageError::IncompatibleVersion { .. }));
}

#[tokio::test]
async fn an_empty_stream_is_a_valid_object() {
    // A zero-length component is legal and must not be confused with absence.
    let (_dir, store) = store().await;
    let stored = store.put_stream(&b""[..], fast()).await.expect("store");
    assert_eq!(stored.size_bytes, 0);
    let stat = store
        .stat(&stored.digest)
        .await
        .expect("stat")
        .expect("present");
    assert_eq!(stat.size_bytes, 0);
}

#[tokio::test]
async fn content_larger_than_the_copy_buffer_streams_correctly() {
    // Guards the chunk loop: images are never read whole, so a payload
    // spanning several buffers must hash and store identically.
    let (_dir, store) = store().await;
    let payload: Vec<u8> = (0..(400_u32 * 1024))
        .map(|i| u8::try_from(i % 251).unwrap_or(0))
        .collect();

    let stored = store
        .put_stream(payload.as_slice(), fast())
        .await
        .expect("store");
    assert_eq!(stored.size_bytes, payload.len() as u64);

    store.verify(&stored.digest).await.expect("verify");
    let read = store
        .read_range(&stored.digest, 0, payload.len())
        .await
        .expect("read");
    assert_eq!(read, payload);
}
