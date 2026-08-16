// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Staging behaviour.
//!
//! Everything in a staging area arrived from somewhere untrusted, so most of
//! what matters here is what the area refuses to do.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use tangible_domain::LogicalPath;
use tangible_storage::{
    FilesystemStore, IngestLimits, StagingArea, StagingError, StagingKind, StagingManager,
};
use tempfile::TempDir;

fn fast() -> IngestLimits {
    IngestLimits {
        max_bytes: None,
        fsync: false,
    }
}

fn path(text: &str) -> LogicalPath {
    LogicalPath::parse(text).expect("valid logical path")
}

async fn manager() -> (TempDir, StagingManager) {
    let dir = TempDir::new().expect("temp dir");
    let manager = StagingManager::open(dir.path().join("staging"))
        .await
        .expect("open staging");
    (dir, manager)
}

async fn area(manager: &StagingManager, id: &str) -> StagingArea {
    manager
        .create(StagingKind::Imports, id)
        .await
        .expect("create area")
}

#[tokio::test]
async fn areas_are_isolated_from_one_another() {
    let (dir, manager) = manager().await;
    let first = area(&manager, "import-a").await;
    let second = area(&manager, "import-b").await;

    first
        .write(&path("disc.iso"), b"first")
        .await
        .expect("write");
    second
        .write(&path("disc.iso"), b"second")
        .await
        .expect("write");

    assert_ne!(first.path(), second.path());
    assert_eq!(first.entries().await.expect("entries").len(), 1);
    assert_eq!(second.entries().await.expect("entries").len(), 1);

    let promoted = first
        .promote(&path("disc.iso"), &store(&dir).await, fast())
        .await
        .expect("promote");
    assert_eq!(promoted.size_bytes, 5);
}

async fn store(dir: &TempDir) -> FilesystemStore {
    FilesystemStore::open(dir.path().join("library"))
        .await
        .expect("open store")
}

#[tokio::test]
async fn nested_paths_are_created_and_listed() {
    let (_dir, manager) = manager().await;
    let area = area(&manager, "import-nested").await;

    area.write(&path("BDMV/index.bdmv"), b"idx")
        .await
        .expect("write");
    area.write(&path("BDMV/STREAM/00000.m2ts"), b"stream")
        .await
        .expect("write");

    let entries = area.entries().await.expect("entries");
    let listed: Vec<_> = entries.iter().map(LogicalPath::as_str).collect();
    assert_eq!(listed, vec!["BDMV/STREAM/00000.m2ts", "BDMV/index.bdmv"]);
}

#[tokio::test]
async fn entries_are_returned_in_a_stable_order() {
    // Promotion order determines component ordinals, so the walk must not
    // depend on filesystem iteration order.
    let (_dir, manager) = manager().await;
    let area = area(&manager, "import-order").await;
    for name in ["c.bin", "a.cue", "b.bin"] {
        area.write(&path(name), name.as_bytes())
            .await
            .expect("write");
    }

    let first: Vec<String> = area
        .entries()
        .await
        .expect("entries")
        .iter()
        .map(ToString::to_string)
        .collect();
    let second: Vec<String> = area
        .entries()
        .await
        .expect("entries")
        .iter()
        .map(ToString::to_string)
        .collect();

    assert_eq!(first, second);
    assert_eq!(first, vec!["a.cue", "b.bin", "c.bin"]);
}

// --- what staging refuses ----------------------------------------------------

#[tokio::test]
#[cfg(unix)]
async fn a_symlink_in_a_staged_tree_is_refused_not_followed() {
    // The standard way an extracted archive escapes its own directory.
    let (dir, manager) = manager().await;
    let area = area(&manager, "import-symlink").await;

    let outside = dir.path().join("outside-secret");
    std::fs::write(&outside, b"should never be read").expect("write");
    std::os::unix::fs::symlink(&outside, area.path().join("escape.iso")).expect("symlink");

    let error = area.entries().await.expect_err("must refuse");
    assert!(
        matches!(error, StagingError::Symlink { .. }),
        "got {error:?}"
    );

    let resolve_error = area
        .resolve(&path("escape.iso"))
        .await
        .expect_err("must refuse");
    assert!(matches!(resolve_error, StagingError::Symlink { .. }));
}

#[tokio::test]
#[cfg(unix)]
async fn a_symlinked_parent_directory_is_refused() {
    // The link need not be the final component: a symlinked directory in the
    // middle redirects everything written beneath it.
    let (dir, manager) = manager().await;
    let area = area(&manager, "import-symlink-parent").await;

    let outside = dir.path().join("outside-dir");
    std::fs::create_dir_all(&outside).expect("mkdir");
    std::os::unix::fs::symlink(&outside, area.path().join("sub")).expect("symlink");

    let error = area
        .resolve(&path("sub/payload.iso"))
        .await
        .expect_err("must refuse");
    assert!(
        matches!(error, StagingError::Symlink { .. }),
        "got {error:?}"
    );
}

#[tokio::test]
async fn an_area_identifier_cannot_contain_a_separator() {
    // Otherwise a job id would decide where its area is created.
    let (_dir, manager) = manager().await;
    for bad in ["../escape", "a/b", "", ".", "..", "a\\b"] {
        let error = manager
            .create(StagingKind::Imports, bad)
            .await
            .expect_err("must refuse");
        assert!(
            matches!(error, StagingError::Escape { .. }),
            "{bad:?} produced {error:?}"
        );
    }
}

#[tokio::test]
async fn a_traversing_logical_path_cannot_be_constructed_at_all() {
    // The type is the boundary, so staging never sees an unsafe path.
    assert!(LogicalPath::parse("../escape.iso").is_err());
    assert!(LogicalPath::parse("/etc/passwd").is_err());
}

// --- promotion ---------------------------------------------------------------

#[tokio::test]
async fn promotion_computes_the_digest_from_the_staged_bytes() {
    let (dir, manager) = manager().await;
    let store = store(&dir).await;
    let area = area(&manager, "import-promote").await;
    area.write(&path("disc.iso"), b"payload")
        .await
        .expect("write");

    let promoted = area
        .promote(&path("disc.iso"), &store, fast())
        .await
        .expect("promote");

    assert_eq!(promoted.size_bytes, 7);
    assert!(!promoted.deduplicated);
    assert!(store.stat(&promoted.digest).await.expect("stat").is_some());
}

#[tokio::test]
async fn promotion_detects_a_staged_file_altered_after_arrival() {
    // Copy-plus-verify: the digest reflects what was read at promotion time,
    // not what the source claimed on arrival.
    let (dir, manager) = manager().await;
    let store = store(&dir).await;
    let area = area(&manager, "import-altered").await;

    area.write(&path("disc.iso"), b"original")
        .await
        .expect("write");
    let before = area
        .promote(&path("disc.iso"), &store, fast())
        .await
        .expect("promote");

    area.write(&path("disc.iso"), b"tampered")
        .await
        .expect("rewrite");
    let after = area
        .promote(&path("disc.iso"), &store, fast())
        .await
        .expect("promote");

    assert_ne!(
        before.digest, after.digest,
        "altered bytes must produce a different digest"
    );
}

#[tokio::test]
async fn promoting_identical_staged_files_deduplicates() {
    let (dir, manager) = manager().await;
    let store = store(&dir).await;
    let first = area(&manager, "import-dedupe-a").await;
    let second = area(&manager, "import-dedupe-b").await;

    first
        .write(&path("disc.iso"), b"same")
        .await
        .expect("write");
    second
        .write(&path("disc.iso"), b"same")
        .await
        .expect("write");

    let a = first
        .promote(&path("disc.iso"), &store, fast())
        .await
        .expect("a");
    let b = second
        .promote(&path("disc.iso"), &store, fast())
        .await
        .expect("b");

    assert_eq!(a.digest, b.digest);
    assert!(!a.deduplicated);
    assert!(b.deduplicated);
}

#[tokio::test]
async fn promotion_respects_the_size_limit() {
    let (dir, manager) = manager().await;
    let store = store(&dir).await;
    let area = area(&manager, "import-toolarge").await;
    area.write(&path("big.iso"), b"far more than eight")
        .await
        .expect("write");

    let error = area
        .promote(
            &path("big.iso"),
            &store,
            IngestLimits {
                max_bytes: Some(8),
                fsync: false,
            },
        )
        .await
        .expect_err("must refuse");
    assert!(matches!(error, StagingError::Storage(_)), "got {error:?}");
}

#[tokio::test]
async fn promotion_leaves_the_staged_file_in_place() {
    // The caller discards the area only once every component is promoted and
    // the manifest is published, so promotion must not consume its input.
    let (dir, manager) = manager().await;
    let store = store(&dir).await;
    let area = area(&manager, "import-keep").await;
    area.write(&path("disc.iso"), b"payload")
        .await
        .expect("write");

    area.promote(&path("disc.iso"), &store, fast())
        .await
        .expect("promote");
    assert!(area.path().join("disc.iso").exists());
}

// --- recovery ----------------------------------------------------------------

#[tokio::test]
async fn areas_survive_a_restart_and_can_be_reopened() {
    let (dir, manager) = manager().await;
    let original = area(&manager, "import-restart").await;
    original
        .write(&path("disc.iso"), b"partial")
        .await
        .expect("write");
    drop(original);
    drop(manager);

    let reopened_manager = StagingManager::open(dir.path().join("staging"))
        .await
        .expect("reopen manager");
    let reopened = reopened_manager
        .reopen(StagingKind::Imports, "import-restart")
        .await
        .expect("reopen area");

    let entries = reopened.entries().await.expect("entries");
    assert_eq!(entries.len(), 1);
}

#[tokio::test]
async fn listing_finds_interrupted_work() {
    // What recovery walks after a restart.
    let (_dir, manager) = manager().await;
    for id in ["import-1", "import-2"] {
        area(&manager, id).await;
    }
    manager
        .create(StagingKind::Derivatives, "deriv-1")
        .await
        .expect("create");

    assert_eq!(
        manager.list(StagingKind::Imports).await.expect("list"),
        vec!["import-1", "import-2"]
    );
    assert_eq!(
        manager.list(StagingKind::Derivatives).await.expect("list"),
        vec!["deriv-1"]
    );
    assert!(
        manager
            .list(StagingKind::Materializations)
            .await
            .expect("list")
            .is_empty()
    );
}

#[tokio::test]
async fn reopening_an_absent_area_reports_it() {
    let (_dir, manager) = manager().await;
    let error = manager
        .reopen(StagingKind::Imports, "never-existed")
        .await
        .expect_err("must fail");
    assert!(matches!(error, StagingError::NoSuchArea { .. }));
}

#[tokio::test]
async fn discarding_removes_the_area_and_its_contents() {
    let (_dir, manager) = manager().await;
    let area = area(&manager, "import-discard").await;
    area.write(&path("nested/disc.iso"), b"payload")
        .await
        .expect("write");
    let path_on_disk = area.path().to_path_buf();

    assert!(
        manager
            .discard(StagingKind::Imports, "import-discard")
            .await
            .expect("discard")
    );
    assert!(!path_on_disk.exists());
    assert!(
        manager
            .list(StagingKind::Imports)
            .await
            .expect("list")
            .is_empty()
    );
}

#[tokio::test]
async fn discarding_an_absent_area_is_not_an_error() {
    // Cleanup must be safely re-runnable after a partial sweep.
    let (_dir, manager) = manager().await;
    assert!(
        !manager
            .discard(StagingKind::Imports, "never-existed")
            .await
            .expect("discard")
    );
}

#[tokio::test]
async fn staging_and_the_library_are_separate_trees() {
    // Nothing half-received may be reachable through the object tree.
    let (dir, manager) = manager().await;
    let store = store(&dir).await;
    assert!(!manager.root().starts_with(store.root()));
    assert!(!store.root().starts_with(manager.root()));
}
