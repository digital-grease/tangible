// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The catalog, through the real router.
//!
//! Four levels (title, edition, disc set, disc) because the canonical object
//! of this system is a disc and everything else exists to say which disc.
//! These tests walk that chain the way an operator does, and then check the
//! two rules that keep it meaningful: a disc number is unique within its set,
//! and a child cannot be hung off a parent that does not exist.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use sqlx::PgPool;
use tangible_api::{ApiState, router};
use tangible_db::{Database, DbConfig};
use tower::ServiceExt as _;

struct Harness {
    router: axum::Router,
    pool: PgPool,
}

async fn harness() -> Harness {
    let url = std::env::var("TANGIBLE_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TANGIBLE_DATABASE_URL"))
        .expect("set TANGIBLE_TEST_DATABASE_URL to run integration tests");
    let database = Database::connect(&DbConfig::new(url))
        .await
        .expect("connect");
    database.migrate().await.expect("migrate");
    let pool = database.pool().clone();
    Harness {
        router: router(ApiState::new(database)),
        pool,
    }
}

impl Harness {
    async fn send(&self, request: Request<Body>) -> (StatusCode, serde_json::Value) {
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("route the request");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("read the body");
        let body = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).expect("a JSON body")
        };
        (status, body)
    }

    async fn get(&self, path: &str) -> (StatusCode, serde_json::Value) {
        self.send(
            Request::builder()
                .method("GET")
                .uri(path)
                .body(Body::empty())
                .expect("build"),
        )
        .await
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        self.send(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("build"),
        )
        .await
    }

    /// Walk the chain down to a disc, the way an operator does.
    async fn a_disc(&self) -> (String, String, String, String) {
        let name = format!("Example {}", uuid::Uuid::now_v7());
        let (status, title) = self
            .post(
                "/api/v1/titles",
                json!({ "display_title": name, "kind": "movie", "release_year": 1999 }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{title}");
        let title_id = title["id"].as_str().expect("title").to_owned();

        let (status, edition) = self
            .post(
                &format!("/api/v1/titles/{title_id}/editions"),
                json!({ "display_name": "Special Edition", "region": "PAL" }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{edition}");
        let edition_id = edition["id"].as_str().expect("edition").to_owned();

        let (status, set) = self
            .post(
                &format!("/api/v1/editions/{edition_id}/disc-sets"),
                json!({ "name": "Two-disc set", "set_kind": "multi_disc", "disc_count_expected": 2 }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{set}");
        let set_id = set["id"].as_str().expect("set").to_owned();

        let (status, disc) = self
            .post(
                &format!("/api/v1/disc-sets/{set_id}/discs"),
                json!({ "sequence_number": 1, "media_family": "dvd", "display_name": "Feature" }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{disc}");
        let disc_id = disc["id"].as_str().expect("disc").to_owned();

        (title_id, edition_id, set_id, disc_id)
    }

    /// An artifact to link, inserted directly: importing one is another
    /// module's job.
    async fn an_artifact(&self) -> String {
        let id = uuid::Uuid::now_v7();
        sqlx::query(
            "INSERT INTO artifacts
                 (id, origin, manifest_version, total_bytes, component_count, validation_state)
             VALUES ($1, 'imported_original', 'v1alpha1', 10, 1, 'valid')",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .expect("seed an artifact");
        id.to_string()
    }
}

// --- the chain -------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_disc_can_be_catalogued_from_nothing() {
    // Until this existed the only way to queue a burn was to write a disc row
    // by hand.
    let harness = harness().await;
    let (title, edition, set, disc) = harness.a_disc().await;

    let (status, body) = harness.get(&format!("/api/v1/discs/{disc}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["sequence_number"], 1);
    assert_eq!(body["media_family"], "dvd");
    assert_eq!(body["display_name"], "Feature");
    assert_eq!(body["disc_set_id"], set);
    // Nothing claims more than it can support: the schema refuses a stronger
    // claim without evidence, and nothing has offered any.
    assert_eq!(body["compatibility_claim"], "unknown");
    assert_eq!(body["artifact_count"], 0);
    assert_eq!(body["copy_count"], 0);

    let (_, editions) = harness
        .get(&format!("/api/v1/titles/{title}/editions"))
        .await;
    assert_eq!(editions[0]["id"], edition);
    assert_eq!(editions[0]["set_count"], 1);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_incomplete_set_is_visible_without_counting_by_eye() {
    // The set says it shipped as two discs and one is catalogued. Somebody
    // looking for what is missing should not have to count rows.
    let harness = harness().await;
    let (_, edition, set, _) = harness.a_disc().await;

    let (status, sets) = harness
        .get(&format!("/api/v1/editions/{edition}/disc-sets"))
        .await;
    assert_eq!(status, StatusCode::OK, "{sets}");
    assert_eq!(sets[0]["id"], set);
    assert_eq!(sets[0]["disc_count_expected"], 2);
    assert_eq!(sets[0]["disc_count"], 1);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn titles_are_listed_in_sort_order_and_searchable() {
    let harness = harness().await;
    let marker = uuid::Uuid::now_v7().simple().to_string();
    for name in ["Zebra", "Apple"] {
        harness
            .post(
                "/api/v1/titles",
                json!({ "display_title": format!("{name} {marker}") }),
            )
            .await;
    }

    let (status, page) = harness
        .get(&format!("/api/v1/titles?search={marker}"))
        .await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = page["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|item| item["display_title"].as_str())
        .collect();
    assert_eq!(names.len(), 2, "{page}");
    assert!(names[0].starts_with("Apple"), "sorted: {names:?}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_title_sorts_by_its_own_name_unless_told_otherwise() {
    // Stripping a leading article is a language-specific decision, and
    // getting it wrong for somebody else's language is worse than sorting
    // plainly.
    let harness = harness().await;
    let (status, body) = harness
        .post("/api/v1/titles", json!({ "display_title": "The Example" }))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["sort_title"], "The Example");

    let (_, explicit) = harness
        .post(
            "/api/v1/titles",
            json!({ "display_title": "The Example", "sort_title": "Example, The" }),
        )
        .await;
    assert_eq!(explicit["sort_title"], "Example, The");
}

// --- what the catalog refuses ------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn two_discs_cannot_share_a_number_within_a_set() {
    // A second disc two would make the set ambiguous forever.
    let harness = harness().await;
    let (_, _, set, _) = harness.a_disc().await;

    let (status, problem) = harness
        .post(
            &format!("/api/v1/disc-sets/{set}/discs"),
            json!({ "sequence_number": 1 }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .expect("detail")
            .contains("ambiguous"),
        "{problem}"
    );

    // A different number is fine.
    let (status, _) = harness
        .post(
            &format!("/api/v1/disc-sets/{set}/discs"),
            json!({ "sequence_number": 2 }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn disc_numbers_start_at_one() {
    let harness = harness().await;
    let (_, _, set, _) = harness.a_disc().await;
    let (status, problem) = harness
        .post(
            &format!("/api/v1/disc-sets/{set}/discs"),
            json!({ "sequence_number": 0 }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_child_cannot_hang_off_a_parent_that_does_not_exist() {
    let harness = harness().await;
    let missing = uuid::Uuid::now_v7();

    for (path, body) in [
        (
            format!("/api/v1/titles/{missing}/editions"),
            json!({ "display_name": "Edition" }),
        ),
        (
            format!("/api/v1/editions/{missing}/disc-sets"),
            json!({ "name": "Set" }),
        ),
        (
            format!("/api/v1/disc-sets/{missing}/discs"),
            json!({ "sequence_number": 1 }),
        ),
    ] {
        let (status, problem) = harness.post(&path, body).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{path}: {problem}"
        );
        assert_eq!(problem["code"], "REFERENCE_NOT_FOUND");
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_unknown_parent_is_not_found_rather_than_empty() {
    // An empty list would read as "this title has no editions".
    let harness = harness().await;
    let missing = uuid::Uuid::now_v7();
    for path in [
        format!("/api/v1/titles/{missing}/editions"),
        format!("/api/v1/editions/{missing}/disc-sets"),
        format!("/api/v1/disc-sets/{missing}/discs"),
        format!("/api/v1/discs/{missing}/artifacts"),
    ] {
        let (status, _) = harness.get(&path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_kind_this_server_does_not_know_is_refused() {
    let harness = harness().await;
    let (status, problem) = harness
        .post(
            "/api/v1/titles",
            json!({ "display_title": "Example", "kind": "interpretive dance" }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert_eq!(problem["code"], "VALIDATION_FAILED");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_nameless_title_is_refused() {
    let harness = harness().await;
    for name in ["", "   "] {
        let (status, _) = harness
            .post("/api/v1/titles", json!({ "display_title": name }))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{name:?}");
    }
}

// --- artifact links ------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_artifact_can_be_linked_to_the_disc_it_represents() {
    let harness = harness().await;
    let (_, _, _, disc) = harness.a_disc().await;
    let artifact = harness.an_artifact().await;

    let (status, links) = harness
        .post(
            &format!("/api/v1/discs/{disc}/artifact-links"),
            json!({ "artifact_id": artifact }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{links}");
    assert_eq!(links[0]["artifact_id"], artifact);
    assert_eq!(links[0]["relationship"], "representation_of");
    assert_eq!(links[0]["confidence"], 1.0);

    let (_, disc_body) = harness.get(&format!("/api/v1/discs/{disc}")).await;
    assert_eq!(disc_body["artifact_count"], 1);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn linking_again_corrects_the_claim_rather_than_failing() {
    // An operator fixing a relationship should not have to unlink first.
    let harness = harness().await;
    let (_, _, _, disc) = harness.a_disc().await;
    let artifact = harness.an_artifact().await;
    let path = format!("/api/v1/discs/{disc}/artifact-links");

    harness
        .post(&path, json!({ "artifact_id": artifact }))
        .await;
    let (status, links) = harness
        .post(
            &path,
            json!({ "artifact_id": artifact, "relationship": "supplement_for", "confidence": 0.5 }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{links}");
    assert_eq!(links.as_array().expect("links").len(), 1);
    assert_eq!(links[0]["relationship"], "supplement_for");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn linking_an_artifact_that_does_not_exist_is_refused() {
    let harness = harness().await;
    let (_, _, _, disc) = harness.a_disc().await;

    let (status, problem) = harness
        .post(
            &format!("/api/v1/discs/{disc}/artifact-links"),
            json!({ "artifact_id": uuid::Uuid::now_v7().to_string() }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert_eq!(problem["code"], "REFERENCE_NOT_FOUND");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_confidence_outside_the_range_is_refused() {
    let harness = harness().await;
    let (_, _, _, disc) = harness.a_disc().await;
    let artifact = harness.an_artifact().await;

    let (status, _) = harness
        .post(
            &format!("/api/v1/discs/{disc}/artifact-links"),
            json!({ "artifact_id": artifact, "confidence": 1.5 }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

// --- the whole point --------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_catalogued_disc_can_have_a_burn_queued_against_it() {
    // The reason the catalog routes exist: before them, queueing a burn
    // needed a disc row written by hand.
    let harness = harness().await;
    let (_, _, _, disc) = harness.a_disc().await;
    let artifact = harness.an_artifact().await;

    let (status, job) = harness
        .post(
            "/api/v1/burn-jobs",
            json!({ "disc_id": disc, "artifact_id": artifact }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{job}");
    assert_eq!(job["disc_id"], disc);
}
