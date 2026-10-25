// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Integrations an administrator configures, through the real router
//! (ADR 0016).
//!
//! A secret goes in once and never comes back out: not in a response, not in
//! the audit trail, not in the database as written. It is stored sealed, and
//! opens only with the server's key, for its own row and field. The RomM
//! export's row is listed and left alone. Only administrators may look.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use tangible_api::secrets::{Sealed, SecretKey, reseal};
use tangible_api::{ApiState, ImportContext, ImportPipeline, router};
use tangible_domain::Role;
use tangible_storage::{FilesystemStore, ManifestStore, StagingManager, WatchRoots};
use tempfile::TempDir;
use tower::ServiceExt as _;

mod support;

struct Harness {
    _dir: TempDir,
    pool: PgPool,
    state: ApiState,
    key: SecretKey,
}

/// A server with a watched root named `downloads` and, unless `keyless`, a
/// secret key.
async fn harness(keyless: bool) -> Harness {
    let database = support::database().await;
    let pool = database.pool().clone();
    let dir = TempDir::new().unwrap();
    let objects = FilesystemStore::open(dir.path().join("library"))
        .await
        .unwrap();
    let manifests = ManifestStore::open(objects.clone()).await.unwrap();
    let staging = StagingManager::open(dir.path().join("staging"))
        .await
        .unwrap();
    let downloads = dir.path().join("downloads");
    std::fs::create_dir(&downloads).unwrap();
    let mut roots = BTreeMap::new();
    roots.insert("downloads".to_owned(), downloads);
    let state =
        ApiState::with_manifests(database.clone(), manifests.clone()).with_imports(ImportContext {
            pipeline: ImportPipeline::new(staging, objects, manifests),
            roots: WatchRoots::new(roots),
            max_upload_bytes: 1 << 20,
            outbound: None,
            upload_part_bytes: 4096,
        });
    let key_text = SecretKey::generate().unwrap();
    let state = if keyless {
        state
    } else {
        state.with_secret_key(SecretKey::from_base64(&key_text).unwrap())
    };
    Harness {
        _dir: dir,
        pool,
        state,
        key: SecretKey::from_base64(&key_text).unwrap(),
    }
}

async fn send(
    harness: &Harness,
    session: &support::Session,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    let mut request = request
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .unwrap();
    session.apply(&mut request);
    let response = router(harness.state.clone())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::now_v7().simple())
}

fn qbittorrent(name: &str, password: Option<&str>) -> Value {
    let mut body = json!({
        "kind": "qbittorrent",
        "name": name,
        "settings": {
            "base_url": "http://192.168.1.13:8080",
            "username": "admin",
            "client_path": "/downloads/tangible/",
            "local_root": "downloads",
        },
    });
    if let Some(password) = password {
        body["secrets"] = json!({ "password": password });
    }
    body
}

async fn sealed_row(pool: &PgPool, id: uuid::Uuid) -> Option<(String, Vec<u8>, Vec<u8>)> {
    sqlx::query_as(
        "SELECT key_id, nonce, ciphertext FROM integration_secrets
         WHERE integration_id = $1 AND field = 'password'",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .unwrap()
}

async fn audit_trail(pool: &PgPool, id: uuid::Uuid) -> Vec<(String, Value)> {
    sqlx::query_as(
        "SELECT action, metadata FROM audit_events
         WHERE target_type = 'integration' AND target_id = $1 ORDER BY id",
    )
    .bind(id.to_string())
    .fetch_all(pool)
    .await
    .unwrap()
}

fn opens(key: &SecretKey, id: uuid::Uuid, row: &(String, Vec<u8>, Vec<u8>)) -> String {
    let sealed = Sealed {
        key_id: row.0.clone(),
        nonce: row.1.clone(),
        ciphertext: row.2.clone(),
    };
    key.open(&id.to_string(), "password", &sealed)
        .unwrap()
        .expose()
        .to_owned()
}

/// Every change audited, the secret by its field's name only.
fn assert_audited(trail: &[(String, Value)], secrets: &[&str]) {
    let actions: Vec<&str> = trail.iter().map(|(action, _)| action.as_str()).collect();
    assert_eq!(
        actions,
        [
            "integration.created",
            "integration.updated",
            "integration.secret_changed",
            "integration.updated",
            "integration.secret_changed",
            "integration.deleted",
        ]
    );
    assert_eq!(trail[0].1["secrets"], json!(["password"]));
    assert_eq!(trail[2].1["set"], json!(["password"]));
    assert_eq!(trail[4].1["removed"], json!(["password"]));
    let written = format!("{trail:?}");
    for secret in secrets {
        assert!(!written.contains(secret), "{written}");
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_integration_is_made_changed_and_removed_and_its_password_never_answered() {
    let harness = harness(false).await;
    let admin = support::administrator(&harness.pool).await;
    let password = unique("first-password");
    let name = unique("qbittorrent");

    let (status, made) = send(
        &harness,
        &admin,
        Method::POST,
        "/api/v1/integrations",
        Some(qbittorrent(&name, Some(&password))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(made["kind"], "qbittorrent");
    assert_eq!(made["status"], "unconfigured");
    assert_eq!(made["managed"], false);
    assert_eq!(
        made["secrets"],
        json!([{ "field": "password", "set": true }])
    );
    assert_eq!(made["settings"]["base_url"], "http://192.168.1.13:8080/");
    assert_eq!(made["settings"]["category"], "tangible");
    assert_eq!(made["settings"]["client_path"], "/downloads/tangible");
    assert!(!made.to_string().contains(&password), "{made}");
    let id: uuid::Uuid = made["id"].as_str().unwrap().parse().unwrap();

    // Stored sealed under this server's key, for this row and field.
    let row = sealed_row(&harness.pool, id).await.expect("sealed");
    assert_eq!(row.0, harness.key.id());
    assert!(
        !row.2
            .windows(password.len())
            .any(|w| w == password.as_bytes())
    );
    assert_eq!(opens(&harness.key, id, &row), password);

    let path = format!("/api/v1/integrations/{id}");
    let (status, list) = send(&harness, &admin, Method::GET, "/api/v1/integrations", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["secret_key_loaded"], true);
    assert!(
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == made["id"])
    );
    assert!(!list.to_string().contains(&password));
    let (status, one) = send(&harness, &admin, Method::GET, &path, None).await;
    assert_eq!(status, StatusCode::OK, "{one}");
    assert_eq!(one["name"], name.as_str());

    // A new password and a new name.
    let second = unique("second-password");
    let renamed = unique("renamed");
    let (status, changed) = send(
        &harness,
        &admin,
        Method::PATCH,
        &path,
        Some(json!({ "name": renamed, "enabled": false, "secrets": { "password": second } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(changed["name"], renamed.as_str());
    assert_eq!(changed["enabled"], false);
    assert_eq!(
        changed["settings"], made["settings"],
        "absent settings stay"
    );
    assert!(!changed.to_string().contains(&second));
    let row = sealed_row(&harness.pool, id).await.expect("sealed");
    assert_eq!(opens(&harness.key, id, &row), second);

    // Removed with null.
    let (status, cleared) = send(
        &harness,
        &admin,
        Method::PATCH,
        &path,
        Some(json!({ "secrets": { "password": null } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert_eq!(
        cleared["secrets"],
        json!([{ "field": "password", "set": false }])
    );
    assert!(sealed_row(&harness.pool, id).await.is_none());

    let (status, _) = send(&harness, &admin, Method::DELETE, &path, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, gone) = send(&harness, &admin, Method::GET, &path, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{gone}");

    assert_audited(&audit_trail(&harness.pool, id).await, &[&password, &second]);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn what_will_not_do_is_refused_saying_why() {
    let harness = harness(false).await;
    let admin = support::administrator(&harness.pool).await;
    let name = unique("qbittorrent");
    let mut bad_url = qbittorrent(&unique("x"), None);
    bad_url["settings"]["base_url"] = json!("http://169.254.169.254/");
    let mut bad_root = qbittorrent(&unique("x"), None);
    bad_root["settings"]["local_root"] = json!("elsewhere");
    let mut wrong_secret = qbittorrent(&unique("x"), None);
    wrong_secret["secrets"] = json!({ "api_key": "x" });
    let mut empty_secret = qbittorrent(&unique("x"), None);
    empty_secret["secrets"] = json!({ "password": "" });
    let mut sabnzbd = qbittorrent(&unique("x"), None);
    sabnzbd["kind"] = json!("sabnzbd");
    for (body, says) in [
        (bad_url, "base_url"),
        (bad_root, "local_root"),
        (wrong_secret, "api_key"),
        (empty_secret, "password"),
        (sabnzbd, "kind"),
        (qbittorrent("", None), "name"),
    ] {
        let (status, problem) = send(
            &harness,
            &admin,
            Method::POST,
            "/api/v1/integrations",
            Some(body.clone()),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body}: {problem}"
        );
        assert_eq!(problem["code"], "VALIDATION_FAILED");
        assert!(
            problem["detail"].as_str().unwrap().contains(says),
            "{body}: {problem}"
        );
    }

    let (status, made) = send(
        &harness,
        &admin,
        Method::POST,
        "/api/v1/integrations",
        Some(qbittorrent(&name, None)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(
        made["secrets"],
        json!([{ "field": "password", "set": false }])
    );
    for taken in [
        name.as_str(),
        tangible_db::integrations::ROMM_INTEGRATION_NAME,
    ] {
        let (status, problem) = send(
            &harness,
            &admin,
            Method::POST,
            "/api/v1/integrations",
            Some(qbittorrent(taken, None)),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{taken}: {problem}");
        assert_eq!(problem["code"], "CONFLICT");
    }
    let path = format!("/api/v1/integrations/{}", made["id"].as_str().unwrap());
    let (status, problem) = send(
        &harness,
        &admin,
        Method::PATCH,
        &path,
        Some(json!({ "secrets": { "api_key": null } })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    let (status, _) = send(&harness, &admin, Method::DELETE, &path, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, problem) = send(
        &harness,
        &admin,
        Method::GET,
        "/api/v1/integrations/not-an-id",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_server_without_a_key_stores_no_secret() {
    let harness = harness(true).await;
    let admin = support::administrator(&harness.pool).await;
    let name = unique("qbittorrent");
    let (status, problem) = send(
        &harness,
        &admin,
        Method::POST,
        "/api/v1/integrations",
        Some(qbittorrent(&name, Some("hunter2"))),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(problem["code"], "SECRET_KEY_MISSING");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("TANGIBLE_SECRET_KEY_FILE")
    );
    let (status, list) = send(&harness, &admin, Method::GET, "/api/v1/integrations", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["secret_key_loaded"], false);
    assert!(
        !list["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"] == name.as_str()),
        "nothing was made"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_romm_export_is_listed_and_left_alone() {
    let harness = harness(false).await;
    let admin = support::administrator(&harness.pool).await;
    let id = tangible_db::romm::ensure_integration(&harness.pool, &json!({}))
        .await
        .unwrap();
    let path = format!("/api/v1/integrations/{id}");
    let (status, one) = send(&harness, &admin, Method::GET, &path, None).await;
    assert_eq!(status, StatusCode::OK, "{one}");
    assert_eq!(one["managed"], true);
    assert_eq!(one["secrets"], json!([]));
    let (status, problem) = send(
        &harness,
        &admin,
        Method::PATCH,
        &path,
        Some(json!({ "enabled": false })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("TANGIBLE_ROMM_EXPORT_ROOT")
    );
    let (status, _) = send(&harness, &admin, Method::DELETE, &path, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn only_administrators_may_look_or_change() {
    let harness = harness(false).await;
    for role in [Role::Viewer, Role::Operator] {
        let session = support::sign_in(&harness.pool, role).await;
        for (method, body) in [
            (Method::GET, None),
            (Method::POST, Some(qbittorrent(&unique("x"), Some("pw")))),
        ] {
            let (status, problem) = send(
                &harness,
                &session,
                method.clone(),
                "/api/v1/integrations",
                body,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{role:?} {method}: {problem}"
            );
            assert_eq!(problem["code"], "FORBIDDEN");
        }
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn rotation_reseals_every_secret_or_none() {
    let harness = harness(false).await;
    let admin = support::administrator(&harness.pool).await;
    let password = unique("rotated-password");
    let (status, made) = send(
        &harness,
        &admin,
        Method::POST,
        "/api/v1/integrations",
        Some(qbittorrent(&unique("qbittorrent"), Some(&password))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let id: uuid::Uuid = made["id"].as_str().unwrap().parse().unwrap();
    let new = SecretKey::from_base64(&SecretKey::generate().unwrap()).unwrap();
    let before = sealed_row(&harness.pool, id).await.unwrap();

    // Other tests' rows share this database and are sealed with their own
    // keys: they are handed back as they are, which rewrites them unchanged.
    let failed = tangible_db::integrations::reseal_all(&harness.pool, |row| {
        if row.integration_id == id {
            Err("stop")
        } else {
            Ok(row.clone())
        }
    })
    .await
    .unwrap();
    assert_eq!(failed, Err("stop"));
    assert_eq!(
        sealed_row(&harness.pool, id).await.unwrap(),
        before,
        "nothing changed"
    );

    let resealed = tangible_db::integrations::reseal_all(&harness.pool, |row| {
        if row.integration_id == id {
            reseal(&harness.key, &new, row)
        } else {
            Ok(row.clone())
        }
    })
    .await
    .unwrap()
    .unwrap();
    assert!(resealed >= 1);
    let after = sealed_row(&harness.pool, id).await.unwrap();
    assert_eq!(after.0, new.id());
    assert_eq!(opens(&new, id, &after), password);

    let path = format!("/api/v1/integrations/{id}");
    let (status, _) = send(&harness, &admin, Method::DELETE, &path, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}
