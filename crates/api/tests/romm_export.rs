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
use tangible_api::{
    ApiState, ImportCheckpoint, ImportPipeline, ImportRequest, RommExport, RommExporter, catalog,
    router,
};
use tangible_domain::{EditionId, ImportJobId, LogicalPath};
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
    let pipeline = ImportPipeline::new(staging, objects, manifests.clone());
    let root = dir.path().join("roms");
    std::fs::create_dir(&root).unwrap();

    let database = support::database().await;
    let pool = database.pool().clone();
    let session = support::administrator(&pool).await;
    let integration = tangible_db::romm::ensure_integration(&pool, &json!({}))
        .await
        .unwrap();
    let export = RommExport::new(root.clone());
    let exporter = RommExporter::new(
        export.clone(),
        database.clone(),
        manifests.clone(),
        integration,
    );
    let state = ApiState::with_manifests(database.clone(), manifests).with_romm(export);
    Harness {
        _dir: dir,
        root,
        router: support::signed_in(router(state), session),
        pool,
        database,
        pipeline,
        exporter,
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
        self.exporter
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

    // A second pass with nothing changed writes nothing.
    let before = std::fs::metadata(&folder).unwrap().modified().unwrap();
    let again = harness.reconcile(&edition).await;
    assert_eq!(again["state"], "current");
    assert_eq!(
        std::fs::metadata(&folder).unwrap().modified().unwrap(),
        before
    );

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
