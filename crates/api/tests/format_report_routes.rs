// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Format support reports, through the real router, on imported artifacts.
//!
//! The point is what never appears: the files' names, as uploaded, as stored,
//! or as a CUE sheet declares them, and the text of a descriptor.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tangible_api::report::{BuildIdentity, DEFAULT_ISSUES_URL, ReportSettings};
use tangible_api::{ApiState, ImportCheckpoint, ImportPipeline, ImportRequest};
use tangible_domain::{ImportJobId, LogicalPath, Role};
use tangible_storage::{FilesystemStore, IngestLimits, ManifestStore, StagingManager};
use tempfile::TempDir;
use tower::ServiceExt as _;

mod support;

const SECTOR: usize = 2048;

/// A structurally valid ISO of 20 blocks.
fn iso_image() -> Vec<u8> {
    let mut bytes = vec![0_u8; 16 * SECTOR];
    let mut pvd = vec![0_u8; SECTOR];
    pvd[0] = 1;
    pvd[1..6].copy_from_slice(b"CD001");
    pvd[6] = 1;
    for slot in &mut pvd[40..72] {
        *slot = b' ';
    }
    pvd[40..43].copy_from_slice(b"VOL");
    pvd[80..84].copy_from_slice(&20_u32.to_le_bytes());
    pvd[84..88].copy_from_slice(&20_u32.to_be_bytes());
    pvd[128..130].copy_from_slice(&2048_u16.to_le_bytes());
    pvd[130..132].copy_from_slice(&2048_u16.to_be_bytes());
    bytes.extend_from_slice(&pvd);
    let mut terminator = vec![0_u8; SECTOR];
    terminator[0] = 255;
    terminator[1..6].copy_from_slice(b"CD001");
    terminator[6] = 1;
    bytes.extend_from_slice(&terminator);
    bytes.resize(20 * SECTOR, 0);
    bytes
}

/// Raw 2352-byte sectors that no detector claims: a sync pattern, and an ISO
/// identifier where a raw mode 1 image keeps one.
fn raw_image() -> Vec<u8> {
    let mut bytes = vec![0_u8; 20 * 2352];
    bytes[..12].copy_from_slice(&[0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 0]);
    bytes[37_649..37_654].copy_from_slice(b"CD001");
    bytes
}

struct Harness {
    _dir: TempDir,
    state: ApiState,
    database: tangible_db::Database,
    ids: Vec<String>,
}

/// One artifact imported from each set of staged files.
async fn harness(sets: Vec<Vec<(&str, Vec<u8>)>>, reports: ReportSettings) -> Harness {
    let dir = TempDir::new().expect("temp dir");
    let objects = FilesystemStore::open(dir.path().join("library"))
        .await
        .expect("objects");
    let manifests = ManifestStore::open(objects.clone())
        .await
        .expect("manifests");
    let staging = StagingManager::open(dir.path().join("staging"))
        .await
        .expect("staging");
    let pipeline = ImportPipeline::new(staging, objects, manifests.clone());
    let mut ids = Vec::new();
    for set in sets {
        let import_id = ImportJobId::generate();
        let area = pipeline.open_area(import_id).await.expect("area");
        for (path, bytes) in &set {
            area.write(&LogicalPath::parse(path).expect("path"), bytes)
                .await
                .expect("stage");
        }
        let mut checkpoint = ImportCheckpoint::default();
        let outcome = pipeline
            .run(
                &ImportRequest {
                    import_id,
                    source_kind: "upload".to_owned(),
                    source_filename: Some(set[0].0.to_owned()),
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
            .expect("import");
        ids.push(outcome.artifact_id.to_string());
    }
    let database = support::database().await;
    let state = ApiState::with_manifests(database.clone(), manifests).with_reports(reports);
    Harness {
        _dir: dir,
        state,
        database,
        ids,
    }
}

fn on() -> ReportSettings {
    let build = BuildIdentity::current(
        Some("v0.1.0"),
        Some("76fa5157aea012aae85461e694082d553071a606"),
    );
    ReportSettings::new(DEFAULT_ISSUES_URL, true, build).expect("settings")
}

async fn report(harness: &Harness, id: &str, role: Role) -> (StatusCode, serde_json::Value) {
    let session = support::sign_in(harness.database.pool(), role).await;
    let mut request = Request::get(format!("/api/v1/artifacts/{id}/format-report"))
        .body(Body::empty())
        .expect("request");
    session.apply(&mut request);
    let response = tangible_api::router(harness.state.clone())
        .oneshot(request)
        .await
        .expect("route the request");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

fn field(report: &serde_json::Value, id: &str) -> String {
    report["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .find(|field| field["id"] == id)
        .and_then(|field| field["value"].as_str())
        .unwrap_or_else(|| panic!("no field {id} in {report}"))
        .to_owned()
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_unrecognised_image_is_described_by_its_structure_alone() {
    let harness = harness(vec![vec![("Holiday Disc.mdf", raw_image())]], on()).await;
    let (status, body) = report(&harness, &harness.ids[0], Role::Operator).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["template"], "format_support.yml");
    assert_eq!(body["title"], "Format: an image Tangible did not recognise");
    let structure = field(&body, "structure");
    assert!(structure.starts_with("Detected: unknown"), "{structure}");
    assert!(
        structure.contains("- component 0: .mdf, 47040 bytes, sha256 "),
        "{structure}"
    );
    assert!(
        structure.contains("starts: binary, first bytes 00 ff ff ff ff ff ff ff ff ff ff 00"),
        "{structure}"
    );
    assert!(
        structure
            .contains("ISO 9660 identifier (CD001) at sector 16 of raw 2352-byte mode 1 sectors"),
        "{structure}"
    );
    assert!(!body.to_string().contains("Holiday"), "{body}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn names_a_cue_sheet_declares_are_replaced_even_for_files_that_are_missing() {
    let cue = "FILE \"Holiday Disc (Track 1).bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n\
               FILE \"Holiday Disc (Track 2).bin\" BINARY\n  TRACK 02 AUDIO\n    INDEX 01 00:00:00\n";
    let harness = harness(
        vec![vec![
            ("Holiday Disc.cue", cue.as_bytes().to_vec()),
            ("Holiday Disc (Track 1).bin", vec![0_u8; 2352 * 10]),
        ]],
        on(),
    )
    .await;
    let (status, body) = report(&harness, &harness.ids[0], Role::Operator).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let structure = field(&body, "structure");
    assert!(
        structure.contains("starts: text, starting with the keyword FILE"),
        "{structure}"
    );
    assert!(structure.contains("<declared-file-2>"), "{structure}");
    assert!(!body.to_string().contains("Holiday"), "{body}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_cleanly_recognised_image_has_nothing_to_ask_about() {
    let harness = harness(vec![vec![("disc.iso", iso_image())]], on()).await;
    let (status, body) = report(&harness, &harness.ids[0], Role::Operator).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "CONFLICT");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn viewers_may_not_and_reports_off_means_none() {
    let served = harness(vec![vec![("x.mdf", raw_image())]], on()).await;
    let (status, _) = report(&served, &served.ids[0], Role::Viewer).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let off = harness(
        vec![vec![("x.mdf", raw_image())]],
        ReportSettings::disabled(BuildIdentity::current(None, None)),
    )
    .await;
    let (status, body) = report(&off, &off.ids[0], Role::Operator).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_unknown_artifact_is_not_found() {
    let harness = harness(Vec::new(), on()).await;
    let (status, body) = report(&harness, &uuid::Uuid::now_v7().to_string(), Role::Operator).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, _) = report(&harness, "not-an-id", Role::Operator).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
