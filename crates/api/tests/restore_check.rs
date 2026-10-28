// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The restore check against a real PostgreSQL and a real library
//! (STORAGE.md 12): a library restored newer than the catalogue, and one
//! restored with objects lost or cut short.
//!
//! The database is shared with every other test, and names thousands of
//! artifacts this test's library never held, so every finding is listed
//! and each assertion looks for this test's own objects and artifacts.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use sha2::{Digest as _, Sha256};
use tangible_api::restore_check::{RestoreCheckSettings, RestoreReport, restore_check};
use tangible_api::{ImportCheckpoint, ImportPipeline, ImportRequest, catalog};
use tangible_domain::{ImportJobId, LogicalPath, Sha256Digest};
use tangible_storage::{FilesystemStore, IngestLimits, ManifestStore, StagingManager};
use tempfile::TempDir;

mod support;

struct Harness {
    _dir: TempDir,
    objects: FilesystemStore,
    manifests: ManifestStore,
    pipeline: ImportPipeline,
    database: tangible_db::Database,
}

async fn harness() -> Harness {
    let dir = TempDir::new().unwrap();
    let objects = FilesystemStore::open(dir.path().join("library"))
        .await
        .unwrap();
    let manifests = ManifestStore::open(objects.clone()).await.unwrap();
    let staging = StagingManager::open(dir.path().join("staging"))
        .await
        .unwrap();
    let pipeline = ImportPipeline::new(staging, objects.clone(), manifests.clone());
    Harness {
        _dir: dir,
        objects,
        manifests,
        pipeline,
        database: support::database().await,
    }
}

fn digest(bytes: &[u8]) -> Sha256Digest {
    hex::encode(Sha256::digest(bytes)).parse().unwrap()
}

/// Bytes of this test's own.
fn bytes(len: usize) -> Vec<u8> {
    let tag = uuid::Uuid::now_v7();
    let mut bytes = vec![0_u8; len];
    bytes[..16].copy_from_slice(tag.as_bytes());
    bytes
}

impl Harness {
    /// Store one file as an artifact, catalogued only when asked: an
    /// artifact imported after the database was dumped is in the library
    /// and not the catalogue.
    async fn artifact(&self, bytes: &[u8], catalogue: bool) -> String {
        let import_id = ImportJobId::generate();
        let area = self.pipeline.open_area(import_id).await.unwrap();
        area.write(&LogicalPath::parse("game.img").unwrap(), bytes)
            .await
            .unwrap();
        let mut checkpoint = ImportCheckpoint::default();
        let outcome = self
            .pipeline
            .run(
                &ImportRequest {
                    import_id,
                    source_kind: "upload".to_owned(),
                    source_filename: Some("game.img".to_owned()),
                    source_reference: None,
                    physical_dump: None,
                    lineage: None,
                    limits: IngestLimits {
                        max_bytes: None,
                        fsync: false,
                    },
                },
                &mut checkpoint,
            )
            .await
            .unwrap();
        if catalogue {
            let manifest = self.manifests.read(outcome.artifact_id).await.unwrap();
            catalog::register_manifest(&self.database, &manifest)
                .await
                .unwrap();
        }
        outcome.artifact_id.to_string()
    }

    async fn check(&self, register: bool) -> RestoreReport {
        restore_check(
            &self.database,
            &self.objects,
            &self.manifests,
            RestoreCheckSettings {
                register,
                list: usize::MAX,
            },
        )
        .await
        .unwrap()
    }

    /// Make an object writable, as Tangible never leaves one, to damage it.
    fn writable(&self, digest: &Sha256Digest) -> std::path::PathBuf {
        let path = self.objects.object_path(digest);
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }
}

fn uncatalogued<'a>(report: &'a RestoreReport, artifact: &str) -> Option<&'a str> {
    report
        .uncatalogued
        .listed
        .iter()
        .find(|found| found.artifact_id == artifact)
        .map(|found| found.outcome.as_str())
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn artifacts_the_library_has_and_the_catalogue_lacks_are_found_and_catalogued_again() {
    let h = harness().await;
    let kept = bytes(4096);
    let catalogued = h.artifact(&kept, true).await;
    let newer = h.artifact(&bytes(8192), false).await;
    let unrecoverable_bytes = bytes(2048);
    let unrecoverable = h.artifact(&unrecoverable_bytes, false).await;
    std::fs::remove_file(h.writable(&digest(&unrecoverable_bytes))).unwrap();

    let found = h.check(false).await;
    assert!(found.pending_migrations.is_empty(), "{found:?}");
    assert_eq!(uncatalogued(&found, &newer), Some("found"));
    assert_eq!(uncatalogued(&found, &unrecoverable), Some("found"));
    assert_eq!(uncatalogued(&found, &catalogued), None);
    assert!(
        !found.missing_manifests.listed.contains(&catalogued),
        "its manifest is there"
    );
    assert!(
        !found
            .missing_objects
            .listed
            .contains(&digest(&kept).to_hex()),
        "its object is there"
    );
    assert!(found.needs_attention());

    let registered = h.check(true).await;
    assert_eq!(uncatalogued(&registered, &newer), Some("registered"));
    let refused = uncatalogued(&registered, &unrecoverable).unwrap();
    assert!(
        refused.contains("game.img is not in the library"),
        "{refused}"
    );
    let present: Vec<uuid::Uuid> =
        sqlx::query_scalar("SELECT id FROM artifacts WHERE id = ANY($1)")
            .bind(vec![
                uuid::Uuid::parse_str(&newer).unwrap(),
                uuid::Uuid::parse_str(&unrecoverable).unwrap(),
            ])
            .fetch_all(h.database.pool())
            .await
            .unwrap();
    assert_eq!(present, vec![uuid::Uuid::parse_str(&newer).unwrap()]);
    let audited: Vec<String> = sqlx::query_scalar(
        "SELECT actor_id FROM audit_events
         WHERE action = 'artifact.recatalogued' AND target_id = $1",
    )
    .bind(&newer)
    .fetch_all(h.database.pool())
    .await
    .unwrap();
    assert_eq!(
        audited,
        vec!["restore-check"],
        "cataloguing again is audited"
    );

    let after = h.check(false).await;
    assert_eq!(uncatalogued(&after, &newer), None, "catalogued now");
    assert_eq!(uncatalogued(&after, &unrecoverable), Some("found"));
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_manifest_filed_under_another_artifact_is_not_catalogued() {
    let h = harness().await;
    let catalogued = h.artifact(&bytes(4096), true).await;
    // A copy gone wrong: the catalogued artifact's manifest, filed under an
    // artifact the catalogue has never heard of.
    let stray = uuid::Uuid::now_v7().to_string();
    let source = h.manifests.manifest_path(catalogued.parse().unwrap());
    let target = h.manifests.manifest_path(stray.parse().unwrap());
    std::fs::copy(&source, &target).unwrap();

    let report = h.check(true).await;
    let outcome = uncatalogued(&report, &stray).expect("the stray file is found");
    assert!(
        outcome.contains(&format!("describes artifact {catalogued} instead")),
        "{outcome}"
    );
    assert!(
        report.uncatalogued.count > report.registered,
        "still needs attention"
    );
    assert!(report.needs_attention());
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn objects_lost_or_cut_short_in_the_restore_are_named() {
    let h = harness().await;
    let lost = bytes(4096);
    let short = bytes(4096);
    h.artifact(&lost, true).await;
    h.artifact(&short, true).await;
    std::fs::remove_file(h.writable(&digest(&lost))).unwrap();
    let path = h.writable(&digest(&short));
    std::fs::write(&path, &short[..100]).unwrap();

    let report = h.check(false).await;
    assert!(
        report
            .missing_objects
            .listed
            .contains(&digest(&lost).to_hex()),
        "the lost object is named"
    );
    let wrong = report
        .wrong_size
        .listed
        .iter()
        .find(|wrong| wrong.sha256 == digest(&short).to_hex())
        .expect("the short object is named");
    assert_eq!((wrong.recorded, wrong.stored), (4096, 100));
    assert!(report.objects_checked >= 2);
    assert!(report.needs_attention());
}
