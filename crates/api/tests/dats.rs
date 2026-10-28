// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! DATs and what they identify, through the real import pipeline,
//! catalogue, identification runner and router
//! (docs/plans/dat-matching.md H2).
//!
//! The database is shared with every other test, so each DAT here has a
//! name and hashes of this test's own: bytes tagged with a fresh id, which
//! nothing else holds. Each test removes the DATs it loaded.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::fmt::Write as _;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tangible_api::identification::{
    IdentificationHandle, IdentificationRunner, IdentificationSettings,
};
use tangible_api::{ApiState, ImportCheckpoint, ImportPipeline, ImportRequest, catalog, router};
use tangible_domain::{ImportJobId, LogicalPath, Role};
use tangible_storage::preservation::{PreservationHashes, hash_bytes};
use tangible_storage::{FilesystemStore, IngestLimits, ManifestStore, StagingManager};
use tempfile::TempDir;
use tower::ServiceExt as _;

mod support;

const RAW: usize = 2352;

struct Harness {
    _dir: TempDir,
    pipeline: ImportPipeline,
    database: tangible_db::Database,
    runner: IdentificationRunner,
    administrator: axum::Router,
    operator: axum::Router,
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
    let database = support::database().await;
    let handle = IdentificationHandle::new();
    let runner = IdentificationRunner::new(
        database.clone(),
        objects,
        manifests.clone(),
        handle.clone(),
        IdentificationSettings {
            owner: format!("test-{}", uuid::Uuid::now_v7()),
            ..IdentificationSettings::default()
        },
    );
    let state = ApiState::with_manifests(database.clone(), manifests).with_identification(handle);
    let app = router(state);
    let administrator = support::signed_in(
        app.clone(),
        support::sign_in(database.pool(), Role::Administrator).await,
    );
    let operator = support::signed_in(app, support::sign_in(database.pool(), Role::Operator).await);
    Harness {
        _dir: dir,
        pipeline,
        database,
        runner,
        administrator,
        operator,
    }
}

async fn send(
    router: &axum::Router,
    method: &str,
    uri: &str,
    body: Vec<u8>,
) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/xml")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

impl Harness {
    /// Import, catalogue and hash these files as one artifact.
    async fn artifact(&self, files: &[(&str, Vec<u8>)]) -> String {
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
        let id = outcome.artifact_id.to_string();
        assert!(
            self.runner
                .identify(uuid::Uuid::parse_str(&id).unwrap())
                .await
                .unwrap()
        );
        id
    }

    async fn load(&self, xml: &str) -> (StatusCode, Value) {
        send(
            &self.administrator,
            "POST",
            "/api/v1/dats",
            xml.as_bytes().to_vec(),
        )
        .await
    }

    async fn remove(&self, id: &str) -> StatusCode {
        send(
            &self.administrator,
            "DELETE",
            &format!("/api/v1/dats/{id}"),
            Vec::new(),
        )
        .await
        .0
    }

    /// The audit events about a DAT: each action and the name recorded.
    async fn audited(&self, dat: &str) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT action, metadata->>'name' FROM audit_events
             WHERE target_type = 'dat' AND target_id = $1 ORDER BY created_at, action",
        )
        .bind(dat)
        .fetch_all(self.database.pool())
        .await
        .unwrap()
    }

    async fn matches(&self, artifact: &str) -> Vec<Value> {
        let (status, view) = send(
            &self.operator,
            "GET",
            &format!("/api/v1/artifacts/{artifact}/identification"),
            Vec::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{view}");
        view["matches"].as_array().unwrap().clone()
    }
}

/// Raw sectors, each tagged with its number and the tag, so the bytes are
/// this test's own.
fn sectors(count: usize, tag: &str) -> Vec<u8> {
    let mut bytes = vec![0_u8; count * RAW];
    for (n, sector) in bytes.chunks_mut(RAW).enumerate() {
        sector[100..108].copy_from_slice(&(n as u64).to_le_bytes());
        sector[108..108 + tag.len()].copy_from_slice(tag.as_bytes());
    }
    bytes
}

fn tag() -> String {
    uuid::Uuid::now_v7().simple().to_string()
}

fn rom(name: &str, hashes: &PreservationHashes) -> String {
    format!(
        r#"<rom name="{name}" size="{}" crc="{}" md5="{}" sha1="{}"/>"#,
        hashes.length,
        hashes.crc32.to_uppercase(),
        hashes.md5,
        hashes.sha1
    )
}

fn dat(name: &str, games: &[(&str, Vec<String>)]) -> String {
    let mut xml = format!(
        "<?xml version=\"1.0\"?>\n<datafile><header><name>{name}</name>\
         <version>2026-10-28</version><author>tests</author></header>"
    );
    for (game, roms) in games {
        let _ = write!(
            xml,
            "<game name=\"{game}\"><category>Games</category><description>{game}</description>"
        );
        for rom in roms {
            xml.push_str(rom);
        }
        xml.push_str("</game>");
    }
    xml.push_str("</datafile>");
    xml
}

const SHEET: &[u8] = b"FILE \"disc.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n  \
    TRACK 02 AUDIO\n    INDEX 00 00:01:25\n    INDEX 01 00:02:25\n";

/// A two-track disc's image, and Redump's two track BINs cut from it.
fn disc(tag: &str) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let image = sectors(300, tag);
    let track1 = image[..100 * RAW].to_vec();
    let track2 = image[100 * RAW..].to_vec();
    (image, track1, track2)
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_merged_and_a_split_image_of_one_disc_both_match_its_game_exactly() {
    let h = harness().await;
    let tag = tag();
    let (image, track1, track2) = disc(&tag);
    let merged = h
        .artifact(&[("disc.cue", SHEET.to_vec()), ("disc.bin", image)])
        .await;

    let name = format!("Tests - Discs {tag}");
    let game = format!("Disc {tag} (World)");
    let xml = dat(
        &name,
        &[(
            &game,
            vec![
                // The CUE's own hash is Redump's file names: never a key.
                rom(&format!("{game}.cue"), &hash_bytes(b"not the sheet")),
                rom(&format!("{game} (Track 1).bin"), &hash_bytes(&track1)),
                rom(&format!("{game} (Track 2).bin"), &hash_bytes(&track2)),
            ],
        )],
    );
    let (status, loaded) = h.load(&xml).await;
    assert_eq!(status, StatusCode::CREATED, "{loaded}");
    assert_eq!(loaded["dat"]["name"], name.as_str());
    assert_eq!(loaded["dat"]["game_count"], 1);
    assert_eq!(loaded["dat"]["rom_count"], 3);
    assert_eq!(loaded["dat"]["matched_artifacts"], 1, "{loaded}");
    assert_eq!(loaded["replaced"], false);

    // Matched when the DAT was loaded.
    let matches = h.matches(&merged).await;
    assert_eq!(matches.len(), 1, "{matches:?}");
    assert_eq!(matches[0]["game_name"], game.as_str());
    assert_eq!(matches[0]["dat_name"], name.as_str());
    assert_eq!(matches[0]["kind"], "exact");
    assert_eq!(matches[0]["files_matched"], 2);
    assert_eq!(matches[0]["files_total"], 2, "the CUE is not counted");
    let files = matches[0]["files"].as_array().unwrap();
    assert_eq!(files[0]["matched_by"], "track 1");
    assert_eq!(files[1]["matched_by"], "track 2");

    // Matched when it was hashed, the DAT already loaded: the same disc as
    // two BINs.
    let split_sheet = b"FILE \"t1.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n\
        FILE \"t2.bin\" BINARY\n  TRACK 02 AUDIO\n    INDEX 00 00:00:00\n    INDEX 01 00:01:00\n"
        .to_vec();
    let split = h
        .artifact(&[
            ("split.cue", split_sheet),
            ("t1.bin", track1),
            ("t2.bin", track2),
        ])
        .await;
    let matches = h.matches(&split).await;
    assert_eq!(matches.len(), 1, "{matches:?}");
    assert_eq!(matches[0]["kind"], "exact");

    let (_, list) = send(&h.operator, "GET", "/api/v1/dats", Vec::new()).await;
    let listed = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"] == name.as_str())
        .unwrap()
        .clone();
    assert_eq!(listed["matched_artifacts"], 2, "{listed}");

    // The library list names each artifact's best match.
    let (_, page) = send(
        &h.operator,
        "GET",
        "/api/v1/artifacts?limit=200",
        Vec::new(),
    )
    .await;
    let summary = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == merged.as_str())
        .unwrap()
        .clone();
    assert_eq!(
        summary["best_match"]["game_name"],
        game.as_str(),
        "{summary}"
    );
    assert_eq!(summary["best_match"]["kind"], "exact");
    assert_eq!(summary["best_match"]["dat_name"], name.as_str());

    assert_eq!(
        h.remove(loaded["dat"]["id"].as_str().unwrap()).await,
        StatusCode::NO_CONTENT
    );
    assert!(
        h.matches(&merged).await.is_empty(),
        "its matches went with it"
    );
    assert_eq!(
        h.remove(loaded["dat"]["id"].as_str().unwrap()).await,
        StatusCode::NOT_FOUND
    );
    // Loading and removing are both audited, with the DAT's name.
    assert_eq!(
        h.audited(loaded["dat"]["id"].as_str().unwrap()).await,
        vec![
            ("dat.loaded".to_owned(), name.clone()),
            ("dat.removed".to_owned(), name.clone())
        ]
    );
}

/// A three-track disc whose tracks 2 and 3 are the same bytes, as two
/// silent tracks are, unless `corrupt_track_3`.
fn twin_tracks(tag: &str, corrupt_track_3: bool) -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) {
    let track1 = sectors(100, tag);
    let mut twin = vec![0_u8; 100 * RAW];
    twin[..tag.len()].copy_from_slice(tag.as_bytes());
    twin[200] = 1;
    let mut track3 = twin.clone();
    if corrupt_track_3 {
        track3[300] = 0xff;
    }
    let image = [track1.clone(), twin.clone(), track3.clone()].concat();
    (image, track1, twin, track3)
}

const TWIN_SHEET: &[u8] =
    b"FILE \"disc.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n  \
    TRACK 02 AUDIO\n    INDEX 01 00:01:25\n  TRACK 03 AUDIO\n    INDEX 01 00:02:50\n";

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn two_identical_files_need_two_matching_tracks_to_be_exact() {
    let h = harness().await;
    let tag = tag();
    let (image, track1, twin, _) = twin_tracks(&tag, false);
    let good = h
        .artifact(&[("disc.cue", TWIN_SHEET.to_vec()), ("disc.bin", image)])
        .await;
    let (image, _, _, _) = twin_tracks(&tag, true);
    let bad = h
        .artifact(&[("disc.cue", TWIN_SHEET.to_vec()), ("disc.bin", image)])
        .await;
    let game = format!("Twins {tag}");
    let (status, loaded) = h
        .load(&dat(
            &format!("Tests - Twins {tag}"),
            &[(
                &game,
                vec![
                    rom("Track 1.bin", &hash_bytes(&track1)),
                    rom("Track 2.bin", &hash_bytes(&twin)),
                    rom("Track 3.bin", &hash_bytes(&twin)),
                ],
            )],
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{loaded}");

    let matches = h.matches(&good).await;
    assert_eq!(matches[0]["kind"], "exact", "{matches:?}");
    let by: Vec<&Value> = matches[0]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| &file["matched_by"])
        .collect();
    assert_eq!(by, ["track 1", "track 2", "track 3"]);

    // Track 3 corrupt: track 2 matches one of the two identical files, not
    // both, so the dump is not called exact.
    let matches = h.matches(&bad).await;
    assert_eq!(matches[0]["kind"], "partial", "{matches:?}");
    assert_eq!(matches[0]["files_matched"], 2);
    assert_eq!(matches[0]["files_total"], 3);
    let by: Vec<&Value> = matches[0]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| &file["matched_by"])
        .collect();
    assert_eq!(
        by,
        [
            &Value::from("track 1"),
            &Value::from("track 2"),
            &Value::Null
        ]
    );
    h.remove(loaded["dat"]["id"].as_str().unwrap()).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_disc_whose_audio_differs_partly_matches_saying_which_track() {
    let h = harness().await;
    let tag = tag();
    let (image, track1, _) = disc(&tag);
    let artifact = h
        .artifact(&[("disc.cue", SHEET.to_vec()), ("disc.bin", image)])
        .await;
    let game = format!("Offset {tag}");
    let (status, loaded) = h
        .load(&dat(
            &format!("Tests - Offsets {tag}"),
            &[(
                &game,
                vec![
                    rom("Track 1.bin", &hash_bytes(&track1)),
                    // As Redump has it, offset-corrected: other bytes.
                    rom(
                        "Track 2.bin",
                        &hash_bytes(&sectors(200, &format!("{tag}-corrected"))),
                    ),
                ],
            )],
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{loaded}");
    let matches = h.matches(&artifact).await;
    assert_eq!(matches.len(), 1, "{matches:?}");
    assert_eq!(matches[0]["kind"], "partial");
    assert_eq!(matches[0]["files_matched"], 1);
    assert_eq!(matches[0]["files_total"], 2);
    let files = matches[0]["files"].as_array().unwrap();
    assert_eq!(files[0]["matched_by"], "track 1");
    assert_eq!(files[1]["matched_by"], Value::Null);
    h.remove(loaded["dat"]["id"].as_str().unwrap()).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_image_without_tracks_matches_by_its_file_and_never_by_a_cue() {
    let h = harness().await;
    let tag = tag();
    let bytes = sectors(40, &tag);
    let artifact = h.artifact(&[("game.img", bytes.clone())]).await;
    let whole = hash_bytes(&bytes);
    let (status, loaded) = h
        .load(&dat(
            &format!("Tests - Images {tag}"),
            &[
                (&format!("Image {tag}"), vec![rom("Image.iso", &whole)]),
                // A CUE with the same hash identifies nothing.
                (&format!("Sheet {tag}"), vec![rom("Sheet.cue", &whole)]),
                // A size that differs is not a match, whatever the hash.
                (
                    &format!("Resized {tag}"),
                    vec![rom(
                        "Resized.iso",
                        &PreservationHashes {
                            length: whole.length + 1,
                            ..whole.clone()
                        },
                    )],
                ),
            ],
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{loaded}");
    let matches = h.matches(&artifact).await;
    assert_eq!(matches.len(), 1, "{matches:?}");
    assert_eq!(matches[0]["game_name"], format!("Image {tag}"));
    assert_eq!(matches[0]["kind"], "exact");
    assert_eq!(matches[0]["files"][0]["matched_by"], "game.img");
    h.remove(loaded["dat"]["id"].as_str().unwrap()).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_dat_of_the_same_name_replaces_it_and_the_same_file_changes_nothing() {
    let h = harness().await;
    let tag = tag();
    let bytes = sectors(10, &tag);
    let artifact = h.artifact(&[("old.img", bytes.clone())]).await;
    let name = format!("Tests - Replaced {tag}");
    let first = dat(
        &name,
        &[(
            &format!("Old {tag}"),
            vec![rom("a.iso", &hash_bytes(&bytes))],
        )],
    );
    let (status, loaded) = h.load(&first).await;
    assert_eq!(status, StatusCode::CREATED, "{loaded}");
    let (status, again) = h.load(&first).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["unchanged"], true);
    assert_eq!(again["dat"]["id"], loaded["dat"]["id"]);

    let second = dat(
        &name,
        &[(
            &format!("Other {tag}"),
            vec![rom("b.iso", &hash_bytes(b"else"))],
        )],
    );
    let (status, replaced) = h.load(&second).await;
    assert_eq!(status, StatusCode::CREATED, "{replaced}");
    assert_eq!(replaced["replaced"], true);
    assert_ne!(replaced["dat"]["id"], loaded["dat"]["id"]);
    assert!(
        h.matches(&artifact).await.is_empty(),
        "the old DAT's match went with it"
    );
    let (_, list) = send(&h.operator, "GET", "/api/v1/dats", Vec::new()).await;
    let named = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["name"] == name.as_str())
        .count();
    assert_eq!(named, 1);
    h.remove(replaced["dat"]["id"].as_str().unwrap()).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn only_administrators_load_dats_and_only_dats_are_taken() {
    let h = harness().await;
    let xml = dat(&format!("Tests - Refused {}", tag()), &[]);
    let (status, refused) = send(&h.operator, "POST", "/api/v1/dats", xml.into_bytes()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");

    for body in [
        "<html><body>not a dat</body></html>",
        "<datafile><header></header></datafile>",
        "<datafile><header><name>x</header></datafile>",
        "<!DOCTYPE datafile [<!ENTITY e SYSTEM \"file:///etc/passwd\">]>\
         <datafile><header><name>&e;</name></header></datafile>",
    ] {
        let (status, problem) = h.load(body).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body}: {problem}"
        );
        assert_eq!(problem["code"], "DAT_INVALID", "{problem}");
    }

    let (status, problem) = send(
        &h.administrator,
        "DELETE",
        "/api/v1/dats/not-an-id",
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
}
