// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Library read routes, exercised through the real router.
//!
//! Requests go through axum's own routing and extraction rather than calling
//! handlers directly, so path parsing, query deserialization and the error
//! response shape are all covered.
//!
//! The library is read from the manifest store, but reading it needs a
//! session, and sessions live in PostgreSQL; hence most of these are ignored
//! without one.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tangible_api::{ApiState, ImportCheckpoint, ImportPipeline, ImportRequest};
use tangible_db::{Database, DbConfig};
use tangible_domain::{ImportJobId, LogicalPath};
use tangible_storage::{FilesystemStore, IngestLimits, ManifestStore, StagingManager};
use tempfile::TempDir;
use tower::ServiceExt as _;

mod support;

const SECTOR: usize = 2048;
const SYSTEM_AREA: usize = 16 * SECTOR;

/// A structurally valid ISO of `blocks` blocks.
fn iso_image(volume_id: &str, blocks: u32) -> Vec<u8> {
    let mut bytes = vec![0_u8; SYSTEM_AREA];
    let mut pvd = vec![0_u8; SECTOR];
    pvd[0] = 1;
    pvd[1..6].copy_from_slice(b"CD001");
    pvd[6] = 1;
    for slot in &mut pvd[40..72] {
        *slot = b' ';
    }
    pvd[40..40 + volume_id.len()].copy_from_slice(volume_id.as_bytes());
    pvd[80..84].copy_from_slice(&blocks.to_le_bytes());
    pvd[84..88].copy_from_slice(&blocks.to_be_bytes());
    pvd[128..130].copy_from_slice(&2048_u16.to_le_bytes());
    pvd[130..132].copy_from_slice(&2048_u16.to_be_bytes());
    bytes.extend_from_slice(&pvd);

    let mut terminator = vec![0_u8; SECTOR];
    terminator[0] = 255;
    terminator[1..6].copy_from_slice(b"CD001");
    terminator[6] = 1;
    bytes.extend_from_slice(&terminator);
    bytes.resize(blocks as usize * SECTOR, 0);
    bytes
}

struct Harness {
    _dir: TempDir,
    router: axum::Router,
    artifact_ids: Vec<String>,
}

/// Build a server with `count` imported artifacts.
async fn harness(count: usize) -> Harness {
    let sets = (0..count)
        .map(|index| vec![(format!("disc-{index}.iso"), iso_image("VOL", 20))])
        .collect();
    harness_with(sets).await
}

/// Build a server with one artifact imported from each set of staged files.
async fn harness_with(sets: Vec<Vec<(String, Vec<u8>)>>) -> Harness {
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
    let mut artifact_ids = Vec::new();
    for set in sets {
        let import_id = ImportJobId::generate();
        let area = pipeline.open_area(import_id).await.expect("area");
        let name = set[0].0.clone();
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
                    source_filename: Some(name),
                    source_reference: None,
                    limits: IngestLimits {
                        max_bytes: None,
                        fsync: false,
                    },
                },
                &mut checkpoint,
            )
            .await
            .expect("import");
        artifact_ids.push(outcome.artifact_id.to_string());
    }
    artifact_ids.sort();

    // The library itself is read from the manifest store, but the session
    // that lets anyone read it is in the database.
    let database = support::database().await;
    let session = support::sign_in(database.pool(), tangible_domain::Role::Viewer).await;

    let state = ApiState::with_manifests(database, manifests);
    Harness {
        _dir: dir,
        router: support::signed_in(tangible_api::router(state), session),
        artifact_ids,
    }
}

async fn get(router: &axum::Router, uri: &str) -> (StatusCode, serde_json::Value, String) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    let status = response.status();
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("body");
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json, content_type)
}

// --- listing -----------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn listing_returns_imported_artifacts() {
    let harness = harness(3).await;
    let (status, body, _) = get(&harness.router, "/api/v1/artifacts").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().expect("items").len(), 3);
    assert_eq!(body["items"][0]["format"], "iso");
    assert!(body["items"][0]["total_bytes"].as_u64().expect("bytes") > 0);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_empty_library_lists_cleanly() {
    // An empty collection is a normal state, not an error.
    let harness = harness(0).await;
    let (status, body, _) = get(&harness.router, "/api/v1/artifacts").await;

    assert_eq!(status, StatusCode::OK);
    assert!(body["items"].as_array().expect("items").is_empty());
    assert!(body["next_cursor"].is_null());
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_short_page_carries_no_cursor() {
    let harness = harness(2).await;
    let (_, body, _) = get(&harness.router, "/api/v1/artifacts?limit=10").await;
    assert!(body["next_cursor"].is_null());
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn pagination_walks_the_whole_library_without_repeating() {
    // The property that matters: every artifact appears exactly once across
    // the pages.
    let harness = harness(5).await;

    let mut seen = Vec::new();
    let mut uri = "/api/v1/artifacts?limit=2".to_owned();
    for _ in 0..10 {
        let (status, body, _) = get(&harness.router, &uri).await;
        assert_eq!(status, StatusCode::OK);
        for item in body["items"].as_array().expect("items") {
            seen.push(item["id"].as_str().expect("id").to_owned());
        }
        match body["next_cursor"].as_str() {
            Some(cursor) => uri = format!("/api/v1/artifacts?limit=2&cursor={cursor}"),
            None => break,
        }
    }

    seen.sort();
    assert_eq!(seen, harness.artifact_ids, "every artifact exactly once");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_forged_cursor_is_rejected_with_a_stable_code() {
    let harness = harness(1).await;
    let (status, body, content_type) =
        get(&harness.router, "/api/v1/artifacts?cursor=!!!not-valid!!!").await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "INVALID_CURSOR");
    assert!(
        content_type.contains("application/problem+json"),
        "problems must use the RFC media type, got {content_type:?}"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_over_large_limit_is_clamped_rather_than_refused() {
    let harness = harness(3).await;
    let (status, body, _) = get(&harness.router, "/api/v1/artifacts?limit=999999").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().expect("items").len(), 3);
}

// --- one artifact ------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn one_artifact_can_be_fetched() {
    let harness = harness(1).await;
    let id = &harness.artifact_ids[0];
    let (status, body, _) = get(&harness.router, &format!("/api/v1/artifacts/{id}")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], id.as_str());
    assert_eq!(body["format"], "iso");
    assert_eq!(body["source_kind"], "upload");
    assert_eq!(body["components"].as_array().expect("components").len(), 1);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_unknown_artifact_is_a_not_found_problem() {
    let harness = harness(1).await;
    let missing = tangible_domain::ArtifactId::generate();
    let (status, body, content_type) =
        get(&harness.router, &format!("/api/v1/artifacts/{missing}")).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "NOT_FOUND");
    assert_eq!(body["status"], 404);
    assert!(content_type.contains("application/problem+json"));
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_malformed_identifier_is_a_bad_request_not_a_not_found() {
    // Distinguishing these matters: one means the caller sent nonsense, the
    // other means the library does not have it.
    let harness = harness(1).await;
    let (status, body, _) = get(&harness.router, "/api/v1/artifacts/not-a-uuid").await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "INVALID_PARAMETER");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_problem_document_names_the_failing_resource() {
    let harness = harness(1).await;
    let missing = tangible_domain::ArtifactId::generate();
    let (_, body, _) = get(&harness.router, &format!("/api/v1/artifacts/{missing}")).await;

    assert!(
        body["detail"]
            .as_str()
            .expect("detail")
            .contains(&missing.to_string()),
        "detail was {:?}",
        body["detail"]
    );
}

// --- components and manifest --------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn components_can_be_listed() {
    let harness = harness(1).await;
    let id = &harness.artifact_ids[0];
    let (status, body, _) = get(
        &harness.router,
        &format!("/api/v1/artifacts/{id}/components"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let components = body.as_array().expect("array");
    assert_eq!(components.len(), 1);
    assert_eq!(components[0]["ordinal"], 0);
    assert_eq!(
        components[0]["sha256"].as_str().expect("digest").len(),
        64,
        "digests are lowercase hex"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_manifest_is_served_verbatim() {
    // An external tool must receive the same bytes the library holds, not a
    // re-serialization that might differ.
    let harness = harness(1).await;
    let id = &harness.artifact_ids[0];
    let (status, body, _) = get(&harness.router, &format!("/api/v1/artifacts/{id}/manifest")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["schema"], "org.tangible.artifact-manifest/v1alpha1",
        "the manifest schema identifier must be present"
    );
    assert_eq!(body["artifact_id"], id.as_str());
    assert!(body["components"].is_array());
}

// --- component bytes -----------------------------------------------------------

/// Fetch bytes, returning the status, headers of interest, and the body.
async fn fetch_bytes(
    router: &axum::Router,
    uri: &str,
    range: Option<&str>,
) -> (StatusCode, Vec<u8>, String, String) {
    let mut request = Request::builder().uri(uri);
    if let Some(range) = range {
        request = request.header(axum::http::header::RANGE, range);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).expect("request"))
        .await
        .expect("response");

    let status = response.status();
    let header = |name: axum::http::HeaderName| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    let content_range = header(axum::http::header::CONTENT_RANGE);
    let accept_ranges = header(axum::http::header::ACCEPT_RANGES);
    let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body")
        .to_vec();
    (status, bytes, content_range, accept_ranges)
}

/// The component identifier of an artifact's only file.
async fn only_component(router: &axum::Router, artifact: &str) -> String {
    let (status, body, _) = get(router, &format!("/api/v1/artifacts/{artifact}/manifest")).await;
    assert_eq!(status, StatusCode::OK);
    body["components"][0]["id"]
        .as_str()
        .expect("a component id")
        .to_owned()
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_component_serves_the_bytes_that_were_imported() {
    let harness = harness(1).await;
    let artifact = &harness.artifact_ids[0];
    let component = only_component(&harness.router, artifact).await;

    let (status, bytes, _, accept_ranges) = fetch_bytes(
        &harness.router,
        &format!("/api/v1/artifacts/{artifact}/components/{component}/content"),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, iso_image("VOL", 20), "the bytes must be unaltered");
    assert_eq!(accept_ranges, "bytes", "resumable downloads are advertised");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_range_request_returns_exactly_that_window() {
    // How a worker resumes staging a partly-downloaded image, so the window
    // has to be exact.
    let harness = harness(1).await;
    let artifact = &harness.artifact_ids[0];
    let component = only_component(&harness.router, artifact).await;
    let image = iso_image("VOL", 20);

    let (status, bytes, content_range, _) = fetch_bytes(
        &harness.router,
        &format!("/api/v1/artifacts/{artifact}/components/{component}/content"),
        Some("bytes=32768-32787"),
    )
    .await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(bytes, image[32768..=32787]);
    assert_eq!(
        content_range,
        format!("bytes 32768-32787/{}", image.len()),
        "the client is told what it received and of what whole"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn resuming_from_an_offset_returns_the_rest() {
    let harness = harness(1).await;
    let artifact = &harness.artifact_ids[0];
    let component = only_component(&harness.router, artifact).await;
    let image = iso_image("VOL", 20);
    let resume_from = image.len() - 100;

    let (status, bytes, _, _) = fetch_bytes(
        &harness.router,
        &format!("/api/v1/artifacts/{artifact}/components/{component}/content"),
        Some(&format!("bytes={resume_from}-")),
    )
    .await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(bytes, image[resume_from..]);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_range_beyond_the_object_is_refused_with_its_size() {
    let harness = harness(1).await;
    let artifact = &harness.artifact_ids[0];
    let component = only_component(&harness.router, artifact).await;
    let image = iso_image("VOL", 20);

    let (status, _, content_range, _) = fetch_bytes(
        &harness.router,
        &format!("/api/v1/artifacts/{artifact}/components/{component}/content"),
        Some("bytes=99999999-"),
    )
    .await;

    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        content_range,
        format!("bytes */{}", image.len()),
        "the refusal tells the client how big the object actually is"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_unknown_component_is_not_found() {
    let harness = harness(1).await;
    let artifact = &harness.artifact_ids[0];
    let missing = tangible_domain::ComponentId::generate();

    let (status, body, _) = get(
        &harness.router,
        &format!("/api/v1/artifacts/{artifact}/components/{missing}/content"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "NOT_FOUND");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_malformed_component_identifier_is_rejected() {
    let harness = harness(1).await;
    let artifact = &harness.artifact_ids[0];

    let (status, body, _) = get(
        &harness.router,
        &format!("/api/v1/artifacts/{artifact}/components/not-a-uuid/content"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "INVALID_PARAMETER");
}

// --- degraded storage ----------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_library_reports_unavailable_when_no_store_is_configured() {
    // The server must still start and answer when storage is misconfigured,
    // rather than refusing to boot.
    let database = support::database().await;
    let session = support::sign_in(database.pool(), tangible_domain::Role::Viewer).await;
    let router = support::signed_in(tangible_api::router(ApiState::new(database)), session);

    let (status, body, _) = get(&router, "/api/v1/artifacts").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["code"], "STORAGE_UNAVAILABLE");
    assert!(
        !body["detail"].as_str().expect("detail").contains('/'),
        "the error must not leak a filesystem path"
    );
}

#[tokio::test]
async fn liveness_still_answers_alongside_the_library_routes() {
    // Nesting the library under /api/v1 must not disturb the operational
    // probes at the root.
    let (status, body, _) = get(&anonymous_router(), "/livez").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "alive");
}

/// A server nobody is signed in to, whose database is never reached.
fn anonymous_router() -> axum::Router {
    let database = Database::connect_lazy(&DbConfig::new(
        "postgres://tangible:tangible@127.0.0.1:1/tangible",
    ))
    .expect("lazy pool");
    tangible_api::router(ApiState::new(database))
}

#[tokio::test]
async fn the_library_is_not_open_to_anyone_who_can_reach_the_server() {
    // Refused before any lookup: with no cookie there is nothing to look up,
    // which is why this needs no database.
    for uri in [
        "/api/v1/artifacts",
        "/api/v1/artifacts/01890a5d-ac96-774b-bcce-b302099a8057",
        "/api/v1/artifacts/01890a5d-ac96-774b-bcce-b302099a8057/manifest",
        "/api/v1/burn-jobs",
        "/api/v1/titles",
        "/api/v1/users",
    ] {
        let (status, body, _) = get(&anonymous_router(), uri).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(body["code"], "UNAUTHENTICATED", "{uri}");
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_disc_of_tracks_shows_its_tracks_and_where_their_bytes_are() {
    let sheet = b"CATALOG 1234567890123\n\
FILE \"disc.bin\" BINARY\n\
  TRACK 01 MODE1/2352\n\
    INDEX 01 00:00:00\n\
  TRACK 02 AUDIO\n\
    PREGAP 00:02:00\n\
    FLAGS DCP\n\
    INDEX 01 00:01:00\n"
        .to_vec();
    let harness = harness_with(vec![vec![
        ("disc.cue".to_owned(), sheet),
        ("disc.bin".to_owned(), vec![0_u8; 2352 * 200]),
    ]])
    .await;
    let artifact = &harness.artifact_ids[0];

    let (status, detail, _) = get(&harness.router, &format!("/api/v1/artifacts/{artifact}")).await;
    assert_eq!(status, StatusCode::OK);
    let disc = &detail["disc"];
    assert_eq!(disc["catalog"], "1234567890123");
    assert_eq!(disc["session_count"], 1);
    let tracks = disc["tracks"].as_array().expect("tracks");
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0]["mode"], "MODE1/2352");
    assert_eq!(tracks[0]["is_audio"], false);
    assert_eq!(tracks[1]["is_audio"], true);
    assert_eq!(tracks[1]["component_path"], "disc.bin");
    assert_eq!(tracks[1]["pregap_sectors"], 150);
    assert_eq!(tracks[1]["start_lba"], 225);
    assert_eq!(tracks[1]["flags"][0], "DCP");
    assert_eq!(tracks[1]["sample_byte_order"], "little_endian");

    // Every component can be fetched by the identifier the view gives it.
    let bin = detail["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|component| component["logical_path"] == "disc.bin")
        .expect("the data file");
    assert_eq!(bin["id"], tracks[1]["component_id"]);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_component_downloads_as_an_attachment_and_is_never_rendered() {
    let harness = harness(1).await;
    let artifact = &harness.artifact_ids[0];
    let component = only_component(&harness.router, artifact).await;
    let response = harness
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/artifacts/{artifact}/components/{component}/content"
                ))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let disposition = response
        .headers()
        .get(axum::http::header::CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok())
        .expect("a disposition");
    assert_eq!(
        disposition,
        "attachment; filename=\"disc-0.iso\"; filename*=UTF-8''disc-0.iso"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_awkward_file_name_cannot_break_out_of_the_header() {
    let harness = harness_with(vec![vec![(
        "Disc \"one\"; ünïcode.iso".to_owned(),
        iso_image("VOL", 20),
    )]])
    .await;
    let artifact = &harness.artifact_ids[0];
    let component = only_component(&harness.router, artifact).await;
    let response = harness
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/artifacts/{artifact}/components/{component}/content"
                ))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let disposition = response
        .headers()
        .get(axum::http::header::CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok())
        .expect("a disposition");
    assert_eq!(
        disposition,
        "attachment; filename=\"Disc _one__ _n_code.iso\"; \
         filename*=UTF-8''Disc%20%22one%22%3B%20%C3%BCn%C3%AFcode.iso"
    );
}
