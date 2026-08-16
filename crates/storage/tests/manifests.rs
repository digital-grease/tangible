// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Manifest persistence.
//!
//! The property that matters here is ordering, not just atomicity: a manifest
//! must not become visible until the bytes it references are.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use tangible_domain::enums::{
    ArtifactFormat, ArtifactKind, ArtifactOrigin, ComponentRole, ValidationState,
};
use tangible_domain::manifest::{
    ArtifactManifest, BurnSupport, Classification, Compatibility, Component, ContentHashes,
    Extensions, Origin, SCHEMA_ID, StorageRef, Topology, Validation,
};
use tangible_domain::{ArtifactId, ComponentId, LogicalPath, Sha256Digest};
use tangible_storage::{FilesystemStore, IngestLimits, ManifestStore, ManifestStoreError};
use tempfile::TempDir;
use time::OffsetDateTime;

fn fast() -> IngestLimits {
    IngestLimits {
        max_bytes: None,
        fsync: false,
    }
}

async fn store() -> (TempDir, ManifestStore) {
    let dir = TempDir::new().expect("temp dir");
    let objects = FilesystemStore::open(dir.path()).await.expect("objects");
    let manifests = ManifestStore::open(objects).await.expect("manifests");
    (dir, manifests)
}

/// Build a manifest describing exactly the payloads given, storing them first.
async fn manifest_for(
    store: &ManifestStore,
    payloads: &[(&str, &[u8])],
    put: bool,
) -> ArtifactManifest {
    let mut components = Vec::new();
    let mut total = 0_u64;
    for (ordinal, (path, bytes)) in payloads.iter().enumerate() {
        let digest = if put {
            store
                .objects()
                .put_stream(*bytes, fast())
                .await
                .expect("store object")
                .digest
        } else {
            // A digest for bytes deliberately never written.
            let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
            sha2::Digest::update(&mut hasher, bytes);
            Sha256Digest::from_bytes(sha2::Digest::finalize(hasher).into())
        };
        total += bytes.len() as u64;
        components.push(Component {
            id: ComponentId::generate(),
            logical_path: LogicalPath::parse(path).expect("valid path"),
            source_filename: Some((*path).to_owned()),
            role: ComponentRole::PrimaryImage,
            ordinal: u32::try_from(ordinal).expect("small"),
            length_bytes: bytes.len() as u64,
            storage: StorageRef::for_digest(&digest),
            content: ContentHashes {
                sha256: digest,
                sha1: None,
                md5: None,
                crc32: None,
            },
            media_type: None,
            attributes: Extensions::new(),
        });
    }

    ArtifactManifest {
        schema: SCHEMA_ID.to_owned(),
        artifact_id: ArtifactId::generate(),
        created_at: OffsetDateTime::UNIX_EPOCH,
        origin: Origin {
            kind: ArtifactOrigin::ImportedOriginal,
            source_kind: "upload".to_owned(),
            source_reference: None,
            source_filename: None,
            acquired_at: None,
            operator_note: None,
        },
        classification: Classification {
            artifact_kind: ArtifactKind::SingleFileImage,
            format: ArtifactFormat::Iso,
            format_confidence: 1.0,
            detectors: vec![],
            media_family: None,
            total_bytes: total,
        },
        components,
        topology: Topology::Unknown,
        validation: Validation {
            state: ValidationState::Valid,
            validated_at: None,
            validators: vec![],
        },
        lineage: None,
        associations: vec![],
        compatibility: Compatibility {
            structurally_valid: true,
            burn_support: BurnSupport {
                state: "supported".to_owned(),
                profiles: vec![],
                engines: vec![],
                warnings: vec![],
            },
            target_claims: vec![],
        },
        extensions: Extensions::new(),
    }
}

#[tokio::test]
async fn a_manifest_round_trips_through_disk() {
    let (_dir, store) = store().await;
    let manifest = manifest_for(&store, &[("disc.iso", b"payload")], true).await;

    store.write(&manifest).await.expect("write");
    let read = store.read(manifest.artifact_id).await.expect("read");
    assert_eq!(read, manifest);
}

#[tokio::test]
async fn a_manifest_referencing_absent_bytes_is_refused() {
    // The ordering guarantee. A manifest naming objects that do not exist
    // cannot rebuild anything, so publishing it would record a lie.
    let (_dir, store) = store().await;
    let manifest = manifest_for(&store, &[("ghost.iso", b"never stored")], false).await;

    let error = store.write(&manifest).await.expect_err("must refuse");
    assert!(
        matches!(error, ManifestStoreError::MissingObject { .. }),
        "got {error:?}"
    );
}

#[tokio::test]
async fn a_refused_manifest_leaves_nothing_on_disk() {
    let (_dir, store) = store().await;
    let manifest = manifest_for(&store, &[("ghost.iso", b"never stored")], false).await;

    let _ = store.write(&manifest).await;
    assert!(
        !store.manifest_path(manifest.artifact_id).exists(),
        "a refused manifest must not be published"
    );
    assert!(
        store.list().await.expect("list").is_empty(),
        "a refused manifest must not appear in the listing"
    );
}

#[tokio::test]
async fn a_declared_length_that_disagrees_with_the_object_is_refused() {
    // Catches a manifest assembled against different bytes than the ones in
    // the store: a rebuild would then produce a different artifact.
    let (_dir, store) = store().await;
    let mut manifest = manifest_for(&store, &[("disc.iso", b"payload")], true).await;
    manifest.components[0].length_bytes = 9999;
    manifest.classification.total_bytes = 9999;

    let error = store.write(&manifest).await.expect_err("must refuse");
    assert!(
        matches!(error, ManifestStoreError::LengthMismatch { .. }),
        "got {error:?}"
    );
}

#[tokio::test]
async fn a_multi_file_artifact_requires_every_component_present() {
    // A CUE and its BINs are one artifact; a manifest missing one BIN is not
    // partially useful, it is wrong.
    let (_dir, store) = store().await;
    let mut manifest = manifest_for(
        &store,
        &[("disc.cue", b"CUE TEXT"), ("track01.bin", b"AUDIO")],
        true,
    )
    .await;

    // Point the second component at bytes that were never stored.
    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    sha2::Digest::update(&mut hasher, b"different");
    let absent = Sha256Digest::from_bytes(sha2::Digest::finalize(hasher).into());
    manifest.components[1].content.sha256 = absent;
    manifest.components[1].storage = StorageRef::for_digest(&absent);

    assert!(matches!(
        store.write(&manifest).await,
        Err(ManifestStoreError::MissingObject { .. })
    ));
}

#[tokio::test]
async fn an_internally_inconsistent_manifest_is_refused_before_any_io() {
    let (_dir, store) = store().await;
    let mut manifest = manifest_for(&store, &[("disc.iso", b"payload")], true).await;
    manifest.classification.total_bytes = 1; // disagrees with the component

    let error = store.write(&manifest).await.expect_err("must refuse");
    assert!(
        matches!(error, ManifestStoreError::Invalid(_)),
        "got {error:?}"
    );
    assert!(!store.manifest_path(manifest.artifact_id).exists());
}

#[tokio::test]
async fn reading_an_absent_manifest_reports_not_found() {
    let (_dir, store) = store().await;
    let error = store
        .read(ArtifactId::generate())
        .await
        .expect_err("must fail");
    assert!(matches!(error, ManifestStoreError::NotFound { .. }));
}

#[tokio::test]
async fn listing_finds_published_manifests_in_a_stable_order() {
    // The catalog rebuild walks this listing, so it must be deterministic.
    let (_dir, store) = store().await;
    let mut written = Vec::new();
    for name in ["a.iso", "b.iso", "c.iso"] {
        let manifest = manifest_for(&store, &[(name, name.as_bytes())], true).await;
        store.write(&manifest).await.expect("write");
        written.push(manifest.artifact_id);
    }
    written.sort_unstable();

    assert_eq!(store.list().await.expect("list"), written);
}

#[tokio::test]
async fn a_stray_file_does_not_break_the_listing() {
    // One unrelated file in the directory must not block a catalog rebuild.
    let (dir, store) = store().await;
    let manifest = manifest_for(&store, &[("disc.iso", b"payload")], true).await;
    store.write(&manifest).await.expect("write");

    std::fs::write(dir.path().join("manifests").join("notes.txt"), b"stray").expect("write");
    std::fs::write(dir.path().join("manifests").join("bogus.json"), b"{}").expect("write");

    let listed = store.list().await.expect("list");
    assert_eq!(listed, vec![manifest.artifact_id]);
}

#[tokio::test]
async fn republishing_replaces_the_previous_manifest_atomically() {
    let (_dir, store) = store().await;
    let mut manifest = manifest_for(&store, &[("disc.iso", b"payload")], true).await;
    store.write(&manifest).await.expect("first");

    manifest.validation.state = ValidationState::ValidWithWarnings;
    store.write(&manifest).await.expect("second");

    let read = store.read(manifest.artifact_id).await.expect("read");
    assert_eq!(read.validation.state, ValidationState::ValidWithWarnings);
    assert_eq!(store.list().await.expect("list").len(), 1);
}

#[tokio::test]
async fn no_temporary_file_survives_a_successful_write() {
    let (dir, store) = store().await;
    let manifest = manifest_for(&store, &[("disc.iso", b"payload")], true).await;
    store.write(&manifest).await.expect("write");

    let strays: Vec<_> = std::fs::read_dir(dir.path().join("manifests"))
        .expect("read dir")
        .flatten()
        .filter(|e| e.path().to_string_lossy().contains(".part"))
        .collect();
    assert!(strays.is_empty(), "left {} temporary files", strays.len());
}

#[tokio::test]
async fn a_published_manifest_is_byte_identical_to_what_was_serialized() {
    // The manifest can be hashed and compared across machines only if writing
    // it does not perturb the bytes.
    let (_dir, store) = store().await;
    let manifest = manifest_for(&store, &[("disc.iso", b"payload")], true).await;
    let path = store.write(&manifest).await.expect("write");

    let on_disk = std::fs::read_to_string(path).expect("read");
    assert_eq!(on_disk, manifest.to_json().expect("serialize"));
}
