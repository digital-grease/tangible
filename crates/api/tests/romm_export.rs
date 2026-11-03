// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The RomM export, from catalog to folder.
//!
//! Real imports, a real catalog through the routes, the real exporter, and a
//! temporary directory standing in for RomM's `roms` folder. What is asserted
//! is what RomM would see: one folder per game under its platform, the discs
//! inside it, a CUE sheet that names the renamed files, and nothing of
//! anybody else's touched.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;

use tangible_api::romm_export::ChdVerdict;
use tangible_api::secrets::SecretKey;
use tangible_api::{
    ApiState, DerivationPipeline, DerivationRunner, DerivationRunnerSettings, Derivations,
    ImportCheckpoint, ImportPipeline, ImportRequest, RommExport, RommExporter, RommSync,
    RommSyncHandle, RommSyncSettings, catalog, router,
};
use tangible_domain::{EditionId, ImportJobId, LogicalPath};
use tangible_image::derive::FakeDeriver;
use tangible_integrations::outbound::{OutboundClient, OutboundPolicy};
use tangible_storage::{FilesystemStore, IngestLimits, ManifestStore, StagingManager};
use tempfile::TempDir;
use tower::ServiceExt as _;

mod support;

const RAW: usize = 2352;

struct Harness {
    _dir: TempDir,
    root: PathBuf,
    router: axum::Router,
    pool: PgPool,
    database: tangible_db::Database,
    pipeline: ImportPipeline,
    exporter: RommExporter,
    derivations: DerivationRunner,
    client: OutboundClient,
    key: Arc<SecretKey>,
    sync: RommSyncHandle,
}

async fn harness() -> Harness {
    harness_at(None).await
}

/// A harness exporting into `root`, a real RomM's `roms` folder, or a
/// temporary one.
async fn harness_at(root: Option<PathBuf>) -> Harness {
    let dir = TempDir::new().unwrap();
    let objects = FilesystemStore::open(dir.path().join("library"))
        .await
        .unwrap();
    let manifests = ManifestStore::open(objects.clone()).await.unwrap();
    let staging = StagingManager::open(dir.path().join("staging"))
        .await
        .unwrap();
    let pipeline = ImportPipeline::new(staging.clone(), objects.clone(), manifests.clone());
    let root = root.unwrap_or_else(|| {
        let root = dir.path().join("roms");
        std::fs::create_dir(&root).unwrap();
        root
    });

    let database = support::database().await;
    let pool = database.pool().clone();
    let session = support::administrator(&pool).await;
    let integration = tangible_db::romm::ensure_integration(&pool, &json!({}))
        .await
        .unwrap();
    let sync = RommSyncHandle::new();
    let export = RommExport::new(root.clone()).with_sync(sync.clone());
    let exporter = RommExporter::new(
        export.clone(),
        database.clone(),
        manifests.clone(),
        integration,
    );
    // A fake engine that reports what chdman's round trip establishes for a
    // CD layout, so the export's preference for a checked CHD is exercised
    // without the tool.
    let derivations = Derivations::new(DerivationPipeline::new(
        staging,
        objects,
        manifests.clone(),
        Arc::new(
            FakeDeriver::default()
                .reporting(tangible_domain::LossCharacter::StructurallyEquivalent),
        ),
    ));
    let runner = DerivationRunner::new(
        database.clone(),
        derivations.clone(),
        DerivationRunnerSettings::default(),
    )
    .with_romm(export.clone());
    let client = OutboundClient::new(
        OutboundPolicy::for_integrations(vec!["127.0.0.1/32".parse().unwrap()], None),
        "tangible-test",
    )
    .unwrap();
    let key = Arc::new(SecretKey::from_base64(&SecretKey::generate().unwrap()).unwrap());
    let state = ApiState::with_manifests(database.clone(), manifests)
        .with_romm(export)
        .with_derivations(derivations)
        .with_secret_key(Arc::clone(&key))
        .with_integration_client(client.clone());
    Harness {
        _dir: dir,
        root,
        router: support::signed_in(router(state), session),
        pool,
        database,
        pipeline,
        exporter,
        derivations: runner,
        client,
        key,
        sync,
    }
}

impl Harness {
    async fn send(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(uri);
        let body = match body {
            Some(body) => {
                request = request.header("content-type", "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = self
            .router
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, value)
    }

    /// Import files as one artifact and register it, returning its id.
    async fn import(&self, files: &[(&str, Vec<u8>)]) -> String {
        let import_id = ImportJobId::generate();
        let area = self.pipeline.open_area(import_id).await.unwrap();
        for (name, bytes) in files {
            area.write(&LogicalPath::parse(name).unwrap(), bytes)
                .await
                .unwrap();
        }
        let mut checkpoint = ImportCheckpoint::default();
        let outcome = self
            .pipeline
            .run(
                &ImportRequest {
                    import_id,
                    source_kind: "upload".to_owned(),
                    source_filename: Some(files[0].0.to_owned()),
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
        let manifest = self
            .pipeline
            .manifests()
            .read(outcome.artifact_id)
            .await
            .unwrap();
        catalog::register_manifest(&self.database, &manifest)
            .await
            .unwrap();
        outcome.artifact_id.to_string()
    }

    /// A game edition with one disc per artifact, linked.
    async fn game(&self, title: &str, region: &str, artifacts: &[String]) -> String {
        let (_, t) = self
            .send(
                "POST",
                "/api/v1/titles",
                Some(json!({ "display_title": title, "kind": "game" })),
            )
            .await;
        let t = t["id"].as_str().unwrap();
        let (_, e) = self
            .send(
                "POST",
                &format!("/api/v1/titles/{t}/editions"),
                Some(json!({ "display_name": "Retail", "region": region })),
            )
            .await;
        let edition = e["id"].as_str().unwrap().to_owned();
        let (_, s) = self
            .send(
                "POST",
                &format!("/api/v1/editions/{edition}/disc-sets"),
                Some(json!({ "name": "Discs", "set_kind": "multi_disc" })),
            )
            .await;
        let set = s["id"].as_str().unwrap();
        for (index, artifact) in artifacts.iter().enumerate() {
            let (status, d) = self
                .send(
                    "POST",
                    &format!("/api/v1/disc-sets/{set}/discs"),
                    Some(json!({ "sequence_number": index + 1, "media_family": "cd" })),
                )
                .await;
            assert_eq!(status, StatusCode::CREATED, "{d}");
            let disc = d["id"].as_str().unwrap();
            let (status, links) = self
                .send(
                    "POST",
                    &format!("/api/v1/discs/{disc}/artifact-links"),
                    Some(json!({ "artifact_id": artifact })),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{links}");
        }
        edition
    }

    async fn set(
        &self,
        edition: &str,
        platform: Option<&str>,
        export: bool,
    ) -> (StatusCode, Value) {
        self.send(
            "PUT",
            &format!("/api/v1/editions/{edition}/romm"),
            Some(json!({ "platform": platform, "export": export })),
        )
        .await
    }

    async fn reconcile(&self, edition: &str) -> Value {
        self.reconcile_with(&self.exporter, edition).await
    }

    /// Reconcile with `exporter`, which may verify CHDs as this one does not.
    async fn reconcile_with(&self, exporter: &RommExporter, edition: &str) -> Value {
        exporter
            .reconcile(edition.parse::<EditionId>().unwrap())
            .await
            .unwrap();
        self.send("GET", &format!("/api/v1/editions/{edition}/romm"), None)
            .await
            .1
    }
}

fn cue_disc(tag: &str) -> Vec<(&'static str, Vec<u8>)> {
    let sheet =
        b"FILE \"disc.bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n".to_vec();
    let mut data = vec![0_u8; RAW * 100];
    data[..tag.len()].copy_from_slice(tag.as_bytes());
    vec![("disc.cue", sheet), ("disc.bin", data)]
}

fn names(folder: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(folder)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_two_disc_game_appears_in_romm_as_one_game_and_goes_when_switched_off() {
    let harness = harness().await;
    let one = harness.import(&cue_disc("one")).await;
    let two = harness.import(&cue_disc("two")).await;
    let title = format!("Romm Test {}", uuid::Uuid::now_v7().simple());
    let edition = harness.game(&title, "USA", &[one, two]).await;

    let (status, saved) = harness.set(&edition, Some("psx"), true).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["export"], true);

    let status = harness.reconcile(&edition).await;
    assert_eq!(status["state"], "current", "{status}");
    let folder = harness.root.join(format!("psx/{title} (USA)"));
    assert_eq!(status["folder"], format!("psx/{title} (USA)"));
    assert_eq!(
        names(&folder),
        vec![
            ".tangible-export.json".to_owned(),
            format!("{title} (USA) (Disc 1).bin"),
            format!("{title} (USA) (Disc 1).cue"),
            format!("{title} (USA) (Disc 2).bin"),
            format!("{title} (USA) (Disc 2).cue"),
        ]
    );
    let sheet =
        std::fs::read_to_string(folder.join(format!("{title} (USA) (Disc 2).cue"))).unwrap();
    assert!(
        sheet.starts_with(&format!("FILE \"{title} (USA) (Disc 2).bin\" BINARY\n")),
        "{sheet}"
    );
    let data = std::fs::read(folder.join(format!("{title} (USA) (Disc 2).bin"))).unwrap();
    assert!(data.starts_with(b"two"), "the right disc's bytes");

    // Every file has a receipt.
    let receipts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM export_receipts WHERE destination_key LIKE $1 AND state = 'present'",
    )
    .bind(format!("psx/{title} (USA)/%"))
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(receipts, 4);

    // A second pass with nothing changed writes nothing, but gives
    // receipts from before sizes were recorded their sizes.
    let lengths = |pool: PgPool, title: String| async move {
        sqlx::query_scalar::<_, Option<i64>>(
            "SELECT length_bytes FROM export_receipts WHERE destination_key LIKE $1
             ORDER BY destination_key",
        )
        .bind(format!("psx/{title} (USA)/%"))
        .fetch_all(&pool)
        .await
        .unwrap()
    };
    let written = lengths(harness.pool.clone(), title.clone()).await;
    let on_disk: Vec<Option<i64>> = names(&folder)
        .iter()
        .filter(|name| !name.starts_with('.'))
        .map(|name| {
            Some(i64::try_from(std::fs::metadata(folder.join(name)).unwrap().len()).unwrap())
        })
        .collect();
    assert_eq!(
        written, on_disk,
        "the bytes written, the sheets as rewritten"
    );
    sqlx::query("UPDATE export_receipts SET length_bytes = NULL WHERE destination_key LIKE $1")
        .bind(format!("psx/{title} (USA)/%"))
        .execute(&harness.pool)
        .await
        .unwrap();
    let before = std::fs::metadata(&folder).unwrap().modified().unwrap();
    let again = harness.reconcile(&edition).await;
    assert_eq!(again["state"], "current");
    assert_eq!(
        std::fs::metadata(&folder).unwrap().modified().unwrap(),
        before
    );
    assert_eq!(lengths(harness.pool.clone(), title.clone()).await, written);

    // Switched off: the folder goes, and the receipts say so.
    harness.set(&edition, Some("psx"), false).await;
    let off = harness.reconcile(&edition).await;
    assert_eq!(off["state"], "removed", "{off}");
    assert!(!folder.exists());
    let present: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM export_receipts WHERE destination_key LIKE $1 AND state = 'present'",
    )
    .bind(format!("psx/{title} (USA)/%"))
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(present, 0);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_checked_chd_replaces_the_cue_bin_in_romm() {
    let harness = harness().await;
    // Only this test makes derivatives in this binary; anything a previous
    // run left claimable is withdrawn so the runner takes this job.
    sqlx::query(
        "UPDATE derivation_jobs SET state = 'canceled', completed_at = now(),
                lease_owner = NULL, lease_expires_at = NULL
         WHERE state IN ('queued', 'failed_retryable', 'running')",
    )
    .execute(&harness.pool)
    .await
    .unwrap();
    let parent = harness.import(&cue_disc("chd")).await;
    let title = format!("Romm Chd {}", uuid::Uuid::now_v7().simple());
    let edition = harness
        .game(&title, "USA", std::slice::from_ref(&parent))
        .await;
    harness.set(&edition, Some("psx"), true).await;
    let folder = harness.root.join(format!("psx/{title} (USA)"));
    assert_eq!(harness.reconcile(&edition).await["state"], "current");
    assert_eq!(
        names(&folder),
        vec![
            ".tangible-export.json".to_owned(),
            format!("{title} (USA).bin"),
            format!("{title} (USA).cue"),
        ]
    );

    let (status, asked) = harness
        .send(
            "POST",
            &format!("/api/v1/artifacts/{parent}/derivatives"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{asked}");
    assert!(harness.derivations.step().await.unwrap());

    // The CHD is linked to the same disc and checked, so it is what RomM gets.
    let status = harness.reconcile(&edition).await;
    assert_eq!(status["state"], "current", "{status}");
    assert_eq!(
        names(&folder),
        vec![
            ".tangible-export.json".to_owned(),
            format!("{title} (USA).chd")
        ],
        "the CUE/BIN is replaced, not kept beside it"
    );
    let marker: Value =
        serde_json::from_slice(&std::fs::read(folder.join(".tangible-export.json")).unwrap())
            .unwrap();
    assert_eq!(marker["files"].as_array().unwrap().len(), 1);
    let present: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM export_receipts WHERE destination_key LIKE $1 AND state = 'present'",
    )
    .bind(format!("psx/{title} (USA)/%"))
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(present, 1, "the old files' receipts are marked removed");
}

/// Stands in for chdman's verify: passes, fails or never gets to say, as
/// told, counting runs and saying what chdman would, the file's path
/// included.
#[derive(Debug)]
struct FakeVerifier {
    version: String,
    says: Says,
    runs: std::sync::atomic::AtomicUsize,
}

#[derive(Debug, Clone, Copy)]
enum Says {
    Passed,
    Failed,
    TookTooLong,
}

impl FakeVerifier {
    fn new(says: Says) -> Arc<Self> {
        Arc::new(Self {
            version: format!("0.fake-{}", uuid::Uuid::now_v7().simple()),
            says,
            runs: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn runs(&self) -> usize {
        self.runs.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl tangible_api::romm_export::ChdVerifier for FakeVerifier {
    fn tool_version(&self) -> String {
        self.version.clone()
    }

    fn verify<'a>(
        &'a self,
        chd: &'a Path,
    ) -> std::pin::Pin<Box<dyn Future<Output = ChdVerdict> + Send + 'a>> {
        Box::pin(async move {
            self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert!(chd.is_file(), "verified in place, in the library");
            match self.says {
                Says::Passed => ChdVerdict::Passed,
                Says::Failed => ChdVerdict::Failed(format!(
                    "chdman verify failed: Error reading input file ({}): corrupt data",
                    chd.display()
                )),
                Says::TookTooLong => ChdVerdict::NotRun(format!(
                    "verify: chdman took longer than 21600s reading {}",
                    chd.display()
                )),
            }
        })
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_imported_chd_goes_to_romm_as_it_is_once_chdman_has_verified_it() {
    let harness = harness().await;
    let bytes = std::fs::read(format!(
        "{}/../../fixtures/images/chd/mixed-mode.chd",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let sha256 = {
        use sha2::Digest as _;
        hex::encode(sha2::Sha256::digest(&bytes))
    };
    // Verdicts are by the CHD's bytes; this fixture's from earlier runs go.
    sqlx::query("DELETE FROM chd_verifications WHERE sha256 = $1")
        .bind(&sha256)
        .execute(&harness.pool)
        .await
        .unwrap();
    let chd = harness.import(&[("Game.chd", bytes)]).await;
    let title = format!("Romm Imported Chd {}", uuid::Uuid::now_v7().simple());
    let edition = harness
        .game(&title, "USA", std::slice::from_ref(&chd))
        .await;
    harness.set(&edition, Some("psx"), true).await;
    let folder = harness.root.join(format!("psx/{title} (USA)"));
    let with = |verifier: &Arc<FakeVerifier>| {
        harness.exporter.clone().with_chd_verifier(
            Arc::clone(verifier) as Arc<dyn tangible_api::romm_export::ChdVerifier>
        )
    };

    // No chdman: it waits, and says so.
    let waiting = harness.reconcile(&edition).await;
    assert_eq!(waiting["state"], "blocked", "{waiting}");
    assert!(
        waiting["detail"]
            .as_str()
            .unwrap()
            .contains("runs no chdman"),
        "{waiting}"
    );

    // A verify that never gets to say: blocked for now, nothing kept, so the
    // same chdman is asked again.
    let stuck = FakeVerifier::new(Says::TookTooLong);
    let waiting = harness.reconcile_with(&with(&stuck), &edition).await;
    let detail = waiting["detail"].as_str().unwrap().to_owned();
    assert_eq!(waiting["state"], "blocked", "{waiting}");
    assert!(detail.contains("could not this time"), "{detail}");
    assert!(!detail.contains("/library/"), "no library path: {detail}");
    harness.reconcile_with(&with(&stuck), &edition).await;
    assert_eq!(stuck.runs(), 2, "asked again");

    // A verify that fails: blocked, saying why, the library's path not shown.
    let failing = FakeVerifier::new(Says::Failed);
    let refused = harness.reconcile_with(&with(&failing), &edition).await;
    let detail = refused["detail"].as_str().unwrap().to_owned();
    assert_eq!(refused["state"], "blocked", "{refused}");
    assert!(detail.contains("did not pass chdman's verify"), "{detail}");
    assert!(detail.contains("the CHD"), "{detail}");
    assert!(!detail.contains("/library/"), "no library path: {detail}");
    assert!(!folder.exists());
    // The same chdman is not asked twice about the same bytes.
    harness.reconcile_with(&with(&failing), &edition).await;
    assert_eq!(failing.runs(), 1);

    // Another chdman that passes it: sent as it is.
    let passing = FakeVerifier::new(Says::Passed);
    let current = harness.reconcile_with(&with(&passing), &edition).await;
    assert_eq!(current["state"], "current", "{current}");
    assert_eq!(
        names(&folder),
        vec![
            ".tangible-export.json".to_owned(),
            format!("{title} (USA).chd")
        ]
    );
    assert_eq!(passing.runs(), 1);
    let representation: String =
        sqlx::query_scalar("SELECT representation FROM export_receipts WHERE destination_key = $1")
            .bind(format!("psx/{title} (USA)/{title} (USA).chd"))
            .fetch_one(&harness.pool)
            .await
            .unwrap();
    assert_eq!(representation, "chd");

    // Verified once, it stays verified, with chdman or without.
    harness.reconcile_with(&with(&passing), &edition).await;
    assert_eq!(harness.reconcile(&edition).await["state"], "current");
    assert_eq!(passing.runs(), 1);
    let verdicts: Vec<(String, bool)> = sqlx::query_as(
        "SELECT tool_version, passed FROM chd_verifications WHERE sha256 = $1 ORDER BY created_at",
    )
    .bind(&sha256)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    assert_eq!(
        verdicts,
        vec![
            (failing.version.clone(), false),
            (passing.version.clone(), true)
        ]
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn somebody_elses_folder_blocks_the_export_and_is_left_alone() {
    let harness = harness().await;
    let disc = harness.import(&cue_disc("mine")).await;
    let title = format!("Romm Clash {}", uuid::Uuid::now_v7().simple());
    let edition = harness.game(&title, "Japan", &[disc]).await;
    let theirs = harness.root.join(format!("saturn/{title} (Japan)"));
    std::fs::create_dir_all(&theirs).unwrap();
    std::fs::write(theirs.join("game.chd"), b"theirs").unwrap();

    harness.set(&edition, Some("saturn"), true).await;
    let status = harness.reconcile(&edition).await;
    assert_eq!(status["state"], "blocked", "{status}");
    assert!(status["detail"].as_str().unwrap().contains("left alone"));
    assert_eq!(names(&theirs), vec!["game.chd".to_owned()]);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_disc_without_an_image_romm_reads_blocks_the_export_and_says_why() {
    let harness = harness().await;
    let toc = harness
        .import(&[
            (
                "disc.toc",
                b"CD_ROM\nTRACK MODE1\nDATAFILE \"disc.bin\"\n".to_vec(),
            ),
            ("disc.bin", vec![0_u8; 2048 * 100]),
        ])
        .await;
    let title = format!("Romm Toc {}", uuid::Uuid::now_v7().simple());
    let edition = harness.game(&title, "USA", &[toc]).await;
    harness.set(&edition, Some("psx"), true).await;
    let status = harness.reconcile(&edition).await;
    assert_eq!(status["state"], "blocked");
    assert!(
        status["detail"].as_str().unwrap().contains("TOC/BIN"),
        "{status}"
    );
    assert!(
        !harness
            .root
            .join("psx")
            .join(format!("{title} (USA)"))
            .exists()
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn settings_are_checked_before_they_are_saved() {
    let harness = harness().await;
    let disc = harness.import(&cue_disc("x")).await;
    let edition = harness
        .game(
            &format!("Romm Settings {}", uuid::Uuid::now_v7().simple()),
            "USA",
            &[disc],
        )
        .await;

    let (status, _) = harness.set(&edition, Some("not-a-platform"), false).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = harness.set(&edition, None, true).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an export needs a platform"
    );
    let (status, saved) = harness.set(&edition, Some("ps2"), false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["platform"], "ps2");
    assert_eq!(saved["export"], false);

    let (status, settings) = harness.send("GET", "/api/v1/romm", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(settings["configured"], true);
    assert!(
        settings["platforms"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["slug"] == "psx")
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_server_with_no_export_root_refuses_to_export() {
    let database = support::database().await;
    let session = support::administrator(database.pool()).await;
    let app = support::signed_in(router(ApiState::new(database.clone())), session);
    let title: Value = {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/titles")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "display_title": "No Root" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap(),
        )
        .unwrap()
    };
    let t = title["id"].as_str().unwrap();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/titles/{t}/editions"))
                .header("content-type", "application/json")
                .body(Body::from(json!({ "display_name": "Retail" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let edition: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    let e = edition["id"].as_str().unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/v1/editions/{e}/romm"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "platform": "psx", "export": true }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

/// The platforms owed a scan among `platforms`.
async fn owed(pool: &PgPool, platforms: &[&str]) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT platform FROM romm_owed_scans WHERE platform = ANY($1) ORDER BY platform",
    )
    .bind(platforms)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Taken by the tests that need RomM's API to themselves: there is one
/// RomM API integration, and the one that has RomM scan settles every
/// platform owed, which another test here reads.
static ROMM_API: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_folder_written_moved_or_taken_out_owes_its_platforms_a_scan() {
    // Only this test exports to these platforms, so what is owed of them is
    // its own doing.
    const MINE: [&str; 2] = ["dc", "segacd"];
    let _romm_api = ROMM_API.lock().await;
    let harness = harness().await;
    sqlx::query("DELETE FROM romm_owed_scans WHERE platform = ANY($1)")
        .bind(MINE)
        .execute(&harness.pool)
        .await
        .unwrap();
    let disc = harness.import(&cue_disc("owed")).await;
    let title = format!("Romm Owed {}", uuid::Uuid::now_v7().simple());
    let edition = harness.game(&title, "Japan", &[disc]).await;

    harness.set(&edition, Some("dc"), true).await;
    assert_eq!(harness.reconcile(&edition).await["state"], "current");
    assert_eq!(owed(&harness.pool, &MINE).await, ["dc"]);
    let (_, view) = harness
        .send("GET", &format!("/api/v1/editions/{edition}/romm"), None)
        .await;
    assert_eq!(view["scan_owed"], true, "{view}");
    assert_eq!(view["last_scan"], Value::Null);

    // A pass that changes nothing owes nothing.
    sqlx::query("DELETE FROM romm_owed_scans WHERE platform = ANY($1)")
        .bind(MINE)
        .execute(&harness.pool)
        .await
        .unwrap();
    assert_eq!(harness.reconcile(&edition).await["scan_owed"], false);
    assert!(owed(&harness.pool, &MINE).await.is_empty());

    // Moved to another platform: both folders changed.
    harness.set(&edition, Some("segacd"), true).await;
    let moved = harness.reconcile(&edition).await;
    assert_eq!(
        moved["folder"],
        format!("segacd/{title} (Japan)"),
        "{moved}"
    );
    assert_eq!(owed(&harness.pool, &MINE).await, ["dc", "segacd"]);

    // Taken out: its platform changed again.
    sqlx::query("DELETE FROM romm_owed_scans WHERE platform = ANY($1)")
        .bind(MINE)
        .execute(&harness.pool)
        .await
        .unwrap();
    harness.set(&edition, Some("segacd"), false).await;
    assert_eq!(harness.reconcile(&edition).await["state"], "removed");
    assert_eq!(owed(&harness.pool, &MINE).await, ["segacd"]);
    sqlx::query("DELETE FROM romm_owed_scans WHERE platform = ANY($1)")
        .bind(MINE)
        .execute(&harness.pool)
        .await
        .unwrap();
}

/// What a RomM that scanned `folder` holds for it: its files as written,
/// the marker among them as a RomM not told to leave it out lists it.
fn scanned(id: i64, platform_id: i64, root: &Path, folder: &str) -> support::romm::FakeRom {
    let (platform, name) = folder.split_once('/').unwrap();
    let files = std::fs::read_dir(root.join(folder))
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_string_lossy().into_owned(),
                i64::try_from(entry.metadata().unwrap().len()).unwrap(),
            )
        })
        .collect();
    support::romm::FakeRom {
        id,
        platform_id,
        platform: platform.to_owned(),
        fs_name: name.to_owned(),
        missing: false,
        files,
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
#[allow(clippy::too_many_lines)]
async fn an_export_is_found_in_romm_and_followed_through_a_rename_and_its_removal() {
    let _romm_api = ROMM_API.lock().await;
    let harness = harness().await;
    // There is one RomM API integration, and this is the only test here
    // that makes one.
    for statement in [
        "DELETE FROM integration_secrets WHERE integration_id IN
             (SELECT id FROM integrations WHERE kind = 'romm_api')",
        "DELETE FROM integrations WHERE kind = 'romm_api'",
    ] {
        sqlx::query(statement).execute(&harness.pool).await.unwrap();
    }
    let disc = harness.import(&cue_disc("link")).await;
    let title = format!("Romm Link {}", uuid::Uuid::now_v7().simple());
    let edition = harness.game(&title, "USA", &[disc]).await;
    harness.set(&edition, Some("psx"), true).await;
    let folder = format!("psx/{title} (USA)");
    assert_eq!(harness.reconcile(&edition).await["folder"], folder.as_str());

    let (romm, fake) = support::romm::start().await;
    let (status, made) = harness
        .send(
            "POST",
            "/api/v1/integrations",
            Some(json!({
                "kind": "romm_api",
                "name": "RomM",
                "enabled": true,
                // Scans are another test's business, and what is owed is
                // shared with the tests running beside this one.
                "settings": {
                    "base_url": format!("http://{romm}/"),
                    "web_url": "https://romm.example.net/",
                    "scan_after_export": false,
                },
                "secrets": { "token": support::romm::TOKEN },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let integration = made["id"].as_str().unwrap().to_owned();
    let sync = RommSync::new(
        harness.database.clone(),
        Some(harness.client.clone()),
        Some(Arc::clone(&harness.key)),
        RommSyncSettings::default(),
        harness.sync.clone(),
    );
    let in_romm = || async {
        harness
            .send("GET", &format!("/api/v1/editions/{edition}/romm"), None)
            .await
            .1["in_romm"]
            .clone()
    };

    // RomM has not scanned the platform yet.
    sync.pass().await;
    let link = in_romm().await;
    assert_eq!(link["state"], "unseen", "{link}");
    assert_eq!(link["rom_id"], Value::Null);
    assert_eq!(link["url"], Value::Null);
    assert_eq!(
        link["detail"],
        "RomM has no psx platform yet; a scan of its folder makes one"
    );
    let (_, settings) = harness.send("GET", "/api/v1/romm", None).await;
    let known = |slug: &str| {
        settings["platforms"]
            .as_array()
            .unwrap()
            .iter()
            .find(|platform| platform["slug"] == slug)
            .unwrap()["known_to_romm"]
            .clone()
    };
    assert_eq!(known("psx"), false, "RomM said it knows nothing");

    // Scanned: the folder is a ROM whose files agree, marker and all. The
    // search is blind, as RomM's fuzzy one may be, so the platform is
    // walked, past a page of other games.
    {
        let mut fake = fake.lock().unwrap();
        fake.platforms = vec![(7, "psx".to_owned())];
        fake.known = vec!["psx".to_owned(), "saturn".to_owned()];
        fake.roms = (1..=120)
            .map(|id| support::romm::FakeRom {
                id,
                platform_id: 7,
                platform: "psx".to_owned(),
                fs_name: format!("Another Game {id}"),
                missing: false,
                files: vec![(format!("Another Game {id}.iso"), 1)],
            })
            .collect();
        fake.roms.push(scanned(141, 7, &harness.root, &folder));
        fake.blind_search = true;
    }
    sync.pass().await;
    let link = in_romm().await;
    assert_eq!(link["state"], "linked", "{link}");
    assert_eq!(link["rom_id"], 141);
    assert_eq!(link["url"], "https://romm.example.net/rom/141");
    assert_eq!(link["detail"], Value::Null);
    let walked = fake
        .lock()
        .unwrap()
        .asked
        .iter()
        .filter(|asked| asked.starts_with("GET /api/roms?") && !asked.contains("search_term"))
        .count();
    assert_eq!(walked, 2, "two pages of the platform, once");
    let (_, settings) = harness.send("GET", "/api/v1/romm", None).await;
    let known = |slug: &str| {
        settings["platforms"]
            .as_array()
            .unwrap()
            .iter()
            .find(|platform| platform["slug"] == slug)
            .unwrap()["known_to_romm"]
            .clone()
    };
    assert_eq!((known("psx"), known("ps2")), (json!(true), json!(false)));

    // RomM's files stop agreeing; a prompted pass leaves a linked export be,
    // the hourly one notices.
    fake.lock()
        .unwrap()
        .roms
        .last_mut()
        .unwrap()
        .files
        .iter_mut()
        .find(|(name, _)| {
            Path::new(name)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("bin"))
        })
        .unwrap()
        .1 += 1;
    fake.lock().unwrap().blind_search = false;
    sync.prompted_pass().await;
    assert_eq!(in_romm().await["state"], "linked");
    sync.pass().await;
    let link = in_romm().await;
    assert_eq!(link["state"], "differs", "{link}");
    assert!(
        link["detail"].as_str().unwrap().contains("bytes in RomM"),
        "{link}"
    );

    // RomM marks it missing.
    let fresh = scanned(141, 7, &harness.root, &folder);
    *fake.lock().unwrap().roms.last_mut().unwrap() = support::romm::FakeRom {
        missing: true,
        ..fresh.clone()
    };
    sync.pass().await;
    assert_eq!(in_romm().await["state"], "missing");
    *fake.lock().unwrap().roms.last_mut().unwrap() = fresh;
    sync.pass().await;
    assert_eq!(in_romm().await["state"], "linked");

    // Renamed: until RomM scans, it has no game in the new folder; once it
    // has, the same ROM holds it.
    sqlx::query("UPDATE editions SET region = 'Europe' WHERE id = $1")
        .bind(edition.parse::<uuid::Uuid>().unwrap())
        .execute(&harness.pool)
        .await
        .unwrap();
    let renamed = format!("psx/{title} (Europe)");
    assert_eq!(
        harness.reconcile(&edition).await["folder"],
        renamed.as_str()
    );
    sync.prompted_pass().await;
    let link = in_romm().await;
    assert_eq!(link["state"], "unseen", "{link}");
    *fake.lock().unwrap().roms.last_mut().unwrap() = scanned(141, 7, &harness.root, &renamed);
    sync.prompted_pass().await;
    let link = in_romm().await;
    assert_eq!(
        (link["state"].clone(), link["rom_id"].clone()),
        (json!("linked"), json!(141))
    );

    // Taken out: RomM lists it until it scans, then marks it missing, and
    // then it is left alone.
    harness.set(&edition, Some("psx"), false).await;
    assert_eq!(harness.reconcile(&edition).await["state"], "removed");
    sync.prompted_pass().await;
    let link = in_romm().await;
    assert_eq!(link["state"], "linked", "{link}");
    assert!(
        link["detail"].as_str().unwrap().contains("until a scan"),
        "{link}"
    );
    fake.lock().unwrap().roms.last_mut().unwrap().missing = true;
    sync.prompted_pass().await;
    assert_eq!(in_romm().await["state"], "missing");
    let asked = fake.lock().unwrap().asked.len();
    sync.prompted_pass().await;
    assert!(
        !fake.lock().unwrap().asked[asked..]
            .iter()
            .any(|asked| asked == "GET /api/roms/141"),
        "a missing removed export is not asked after again"
    );

    // RomM's API removed: its ids mean nothing any more.
    let (status, _) = harness
        .send(
            "DELETE",
            &format!("/api/v1/integrations/{integration}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(in_romm().await, Value::Null);
}

/// Against the RomM 5.4.1 that `TANGIBLE_TEST_ROMM_URL` and
/// `TANGIBLE_TEST_ROMM_TOKEN` name, exporting into its `roms` folder,
/// `TANGIBLE_TEST_ROMM_ROOT`; skipped otherwise. Its `config.yml` must
/// leave out the export's marker, as the deploy guide says. The game is
/// exported, scanned and linked; renamed, scanned and linked to the ROM
/// RomM made of it; taken out, scanned and marked missing, which RomM
/// keeps.
#[tokio::test]
#[ignore = "requires PostgreSQL"]
#[allow(clippy::too_many_lines)]
async fn a_real_romm_scans_and_links_an_export_through_a_rename_and_its_removal() {
    let (Ok(url), Ok(token), Ok(root)) = (
        std::env::var("TANGIBLE_TEST_ROMM_URL"),
        std::env::var("TANGIBLE_TEST_ROMM_TOKEN"),
        std::env::var("TANGIBLE_TEST_ROMM_ROOT"),
    ) else {
        eprintln!(
            "skipping: set TANGIBLE_TEST_ROMM_URL, TANGIBLE_TEST_ROMM_TOKEN and \
             TANGIBLE_TEST_ROMM_ROOT"
        );
        return;
    };
    let _romm_api = ROMM_API.lock().await;
    let harness = harness_at(Some(PathBuf::from(root))).await;
    for statement in [
        "DELETE FROM integration_secrets WHERE integration_id IN
             (SELECT id FROM integrations WHERE kind = 'romm_api')",
        "DELETE FROM integrations WHERE kind = 'romm_api'",
    ] {
        sqlx::query(statement).execute(&harness.pool).await.unwrap();
    }
    let (status, made) = harness
        .send(
            "POST",
            "/api/v1/integrations",
            Some(json!({
                "kind": "romm_api",
                "name": "RomM",
                "enabled": true,
                "settings": { "base_url": url },
                "secrets": { "token": token },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let sync = RommSync::new(
        harness.database.clone(),
        Some(harness.client.clone()),
        Some(Arc::clone(&harness.key)),
        RommSyncSettings {
            poll: std::time::Duration::from_millis(500),
            scan_bound: std::time::Duration::from_secs(120),
            ..RommSyncSettings::default()
        },
        harness.sync.clone(),
    );
    let disc = harness.import(&cue_disc("real")).await;
    let title = format!(
        "Tangible E2E {}",
        &uuid::Uuid::now_v7().simple().to_string()[20..]
    );
    let edition = harness.game(&title, "USA", &[disc]).await;
    let view = || async {
        harness
            .send("GET", &format!("/api/v1/editions/{edition}/romm"), None)
            .await
            .1
    };

    harness.set(&edition, Some("psx"), true).await;
    assert_eq!(harness.reconcile(&edition).await["state"], "current");
    let pass = sync.prompted_pass().await;
    let seen = view().await;
    assert_eq!(
        pass.scan,
        Some(tangible_db::romm_api::ScanState::Finished),
        "{seen}"
    );
    assert_eq!(seen["scan_owed"], false, "{seen}");
    assert_eq!(seen["last_scan"]["state"], "finished", "{seen}");
    let link = &seen["in_romm"];
    assert_eq!(link["state"], "linked", "{seen}");
    let rom_id = link["rom_id"].as_i64().unwrap();
    assert_eq!(
        link["url"],
        format!("{}/rom/{rom_id}", url.trim_end_matches('/'))
    );

    sqlx::query("UPDATE editions SET region = 'Europe' WHERE id = $1")
        .bind(edition.parse::<uuid::Uuid>().unwrap())
        .execute(&harness.pool)
        .await
        .unwrap();
    assert_eq!(
        harness.reconcile(&edition).await["folder"],
        format!("psx/{title} (Europe)")
    );
    sync.prompted_pass().await;
    let seen = view().await;
    assert_eq!(seen["in_romm"]["state"], "linked", "{seen}");
    // RomM keeps a moved ROM's id only when its files hash the same. A
    // CUE/BIN export renamed renames its BIN, so its sheet's bytes change:
    // RomM makes a new ROM and marks the old one missing.
    let renamed_id = seen["in_romm"]["rom_id"].as_i64().unwrap();
    assert_ne!(renamed_id, rom_id, "{seen}");

    harness.set(&edition, Some("psx"), false).await;
    assert_eq!(harness.reconcile(&edition).await["state"], "removed");
    sync.prompted_pass().await;
    let seen = view().await;
    assert_eq!(seen["in_romm"]["state"], "missing", "{seen}");
    assert_eq!(seen["in_romm"]["rom_id"], renamed_id);

    let (_, settings) = harness.send("GET", "/api/v1/romm", None).await;
    let integration = settings["api"]["integration_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, _) = harness
        .send(
            "DELETE",
            &format!("/api/v1/integrations/{integration}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}
