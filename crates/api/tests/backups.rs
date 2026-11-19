// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Copies of the database through the routes (ADR 0026,
//! docs/plans/backups-ui.md), against PostgreSQL and its own `pg_dump` and
//! `pg_restore`: taken, listed, pruned, downloaded, uploaded and removed,
//! each audited, and refused to anyone but an administrator.
//!
//! The tools are PostgreSQL 17's: `TANGIBLE_TEST_POSTGRES_TOOLS_DIR` names
//! a directory holding them, such as `crates/api/tests/support/pg-tools`,
//! which runs them from PostgreSQL's image.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::Value;
use tangible_api::backups::{BackupSettings, Backups, ToolConnection};
use tangible_api::{ApiState, router};
use tangible_domain::{Role, SecretString};
use tempfile::TempDir;
use tower::ServiceExt as _;

mod support;

/// Where PostgreSQL's client tools are.
fn tools() -> PathBuf {
    std::env::var_os("TANGIBLE_TEST_POSTGRES_TOOLS_DIR").map_or_else(
        || PathBuf::from(tangible_api::backups::DEFAULT_TOOLS_DIR),
        PathBuf::from,
    )
}

/// How the tools reach the test database.
fn connection() -> ToolConnection {
    let url = std::env::var("TANGIBLE_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TANGIBLE_DATABASE_URL"))
        .expect("set TANGIBLE_TEST_DATABASE_URL to run integration tests");
    let url = reqwest::Url::parse(&url).expect("a URL");
    ToolConnection {
        host: url.host_str().expect("a host").to_owned(),
        port: url.port().unwrap_or(5432),
        user: url.username().to_owned(),
        password: url.password().map(|p| SecretString::from(p.to_owned())),
        database: url.path().trim_start_matches('/').to_owned(),
        sslmode: None,
    }
}

struct Harness {
    dir: TempDir,
    pool: sqlx::PgPool,
    state: ApiState,
    admin: support::Session,
}

impl Harness {
    fn path(&self) -> &Path {
        self.dir.path()
    }
}

async fn harness(kept: u32, max_upload_bytes: u64) -> Harness {
    let database = support::database().await;
    let dir = TempDir::new().unwrap();
    let backups = Backups::new(BackupSettings {
        dir: dir.path().to_owned(),
        kept,
        tools: tools(),
        connection: connection(),
        max_upload_bytes,
    });
    let pool = database.pool().clone();
    let admin = support::administrator(&pool).await;
    Harness {
        state: ApiState::new(database).with_backups(backups),
        pool,
        admin,
        dir,
    }
}

async fn send(
    h: &Harness,
    session: &support::Session,
    request: Request<Body>,
) -> (StatusCode, axum::body::Bytes, axum::http::HeaderMap) {
    let app = support::signed_in(router(h.state.clone()), session.clone());
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, body, headers)
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

async fn listed(h: &Harness) -> Value {
    let (status, body, _) = send(h, &h.admin, get("/api/v1/backups")).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).unwrap()
}

/// Take a copy through the route, and wait for it.
async fn take(h: &Harness) -> Value {
    let (status, body, _) = send(
        h,
        &h.admin,
        Request::post("/api/v1/backups")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    for _ in 0..600 {
        let view = listed(h).await;
        if view["operation"].is_null() {
            assert!(
                view["last_outcome"]["error"].is_null(),
                "the copy was not taken: {}",
                view["last_outcome"]
            );
            return view;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the copy was never finished");
}

async fn audited(h: &Harness, action: &str) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT target_id, outcome FROM audit_events
         WHERE actor_id = $1 AND action = $2 ORDER BY id",
    )
    .bind(&h.admin.user.username)
    .bind(action)
    .fetch_all(&h.pool)
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "requires PostgreSQL and its client tools"]
async fn a_copy_taken_is_an_archive_readable_by_the_group_alone_and_audited() {
    let h = harness(10, 1 << 30).await;
    let view = listed(&h).await;
    assert_eq!(view["tools"]["ready"], true, "{view}");
    assert_eq!(view["writable"], true);
    assert_eq!(view["copies"].as_array().unwrap().len(), 0);

    let view = take(&h).await;
    assert!(view["last_outcome"]["error"].is_null(), "{view}");
    let copies = view["copies"].as_array().unwrap();
    assert_eq!(copies.len(), 1);
    let name = copies[0]["name"].as_str().unwrap();
    assert!(
        name.starts_with("tangible-") && name.ends_with("Z.dump"),
        "{name}"
    );
    assert_eq!(copies[0]["kind"], "taken");
    let path = h.path().join(name);
    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.starts_with(b"PGDMP"));
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );
    // Nothing partial is left beside it.
    assert_eq!(std::fs::read_dir(h.path()).unwrap().count(), 1);
    assert_eq!(
        audited(&h, "backup.taken").await,
        [(name.to_owned(), "success".to_owned())]
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL and its client tools"]
async fn taking_one_prunes_the_oldest_of_tangibles_and_nothing_else() {
    let h = harness(2, 1 << 30).await;
    for name in [
        "tangible-20200101T000000Z.dump",
        "tangible-20200102T000000Z.dump",
        "uploaded-20190101T000000Z.dump",
        "mine.dump",
    ] {
        std::fs::write(h.path().join(name), b"PGDMP").unwrap();
    }
    let view = take(&h).await;
    let names: Vec<&str> = view["copies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 4, "{names:?}");
    assert!(names.contains(&"tangible-20200102T000000Z.dump"));
    assert!(!names.contains(&"tangible-20200101T000000Z.dump"));
    assert!(names.contains(&"uploaded-20190101T000000Z.dump"));
    assert!(names.contains(&"mine.dump"));
}

#[tokio::test]
#[ignore = "requires PostgreSQL and its client tools"]
async fn a_copy_downloads_as_it_is_and_is_audited() {
    let h = harness(10, 1 << 30).await;
    let view = take(&h).await;
    let name = view["copies"][0]["name"].as_str().unwrap().to_owned();
    let (status, body, headers) = send(
        &h,
        &h.admin,
        get(&format!("/api/v1/backups/{name}/download")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_ref(), std::fs::read(h.path().join(&name)).unwrap());
    assert_eq!(
        headers[header::CONTENT_DISPOSITION],
        format!("attachment; filename=\"{name}\"").as_str()
    );
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        audited(&h, "backup.downloaded").await,
        [(name.clone(), "success".to_owned())]
    );

    for missing in ["nothing.dump", "..%2Fetc%2Fpasswd", ".hidden.dump"] {
        let (status, _, _) = send(
            &h,
            &h.admin,
            get(&format!("/api/v1/backups/{missing}/download")),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
    }
}

fn upload(bytes: Vec<u8>) -> Request<Body> {
    Request::put("/api/v1/backups/uploads")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, bytes.len())
        .body(Body::from(bytes))
        .unwrap()
}

#[tokio::test]
#[ignore = "requires PostgreSQL and its client tools"]
async fn an_upload_is_kept_only_when_it_is_an_archive_and_small_enough() {
    let h = harness(10, 1 << 28).await;
    let view = take(&h).await;
    let taken = view["copies"][0]["name"].as_str().unwrap().to_owned();
    let archive = std::fs::read(h.path().join(&taken)).unwrap();

    let (status, body, _) = send(&h, &h.admin, upload(archive.clone())).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let kept: Value = serde_json::from_slice(&body).unwrap();
    let name = kept["name"].as_str().unwrap();
    assert!(name.starts_with("uploaded-"), "{name}");
    assert_eq!(kept["kind"], "uploaded");
    assert_eq!(std::fs::read(h.path().join(name)).unwrap(), archive);
    assert_eq!(
        audited(&h, "backup.uploaded").await,
        [(name.to_owned(), "success".to_owned())]
    );

    // Plain SQL is not an archive, and an archive's first bytes alone are
    // not one either: pg_restore must read it.
    for refused in [
        b"-- plain SQL\n".to_vec(),
        b"PGDMP and nothing else".to_vec(),
    ] {
        let (status, body, _) = send(&h, &h.admin, upload(refused)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let problem: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["code"], "BACKUP_NOT_A_COPY");
    }
    // One said to be larger than allowed is refused before it is read.
    let mut large = upload(b"PGDMP".to_vec());
    large
        .headers_mut()
        .insert(header::CONTENT_LENGTH, ((1_u64 << 28) + 1).into());
    let (status, _, _) = send(&h, &h.admin, large).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    // Only the two copies are there: nothing refused was left behind.
    assert_eq!(std::fs::read_dir(h.path()).unwrap().count(), 2);
}

#[tokio::test]
#[ignore = "requires PostgreSQL and its client tools"]
async fn a_copy_is_removed_and_audited() {
    let h = harness(10, 1 << 30).await;
    std::fs::write(h.path().join("mine.dump"), b"PGDMP").unwrap();
    let (status, _, _) = send(
        &h,
        &h.admin,
        Request::delete("/api/v1/backups/mine.dump")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!h.path().join("mine.dump").exists());
    assert_eq!(
        audited(&h, "backup.deleted").await,
        [("mine.dump".to_owned(), "success".to_owned())]
    );
    let (status, _, _) = send(
        &h,
        &h.admin,
        Request::delete("/api/v1/backups/mine.dump")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore = "requires PostgreSQL and its client tools"]
async fn only_an_administrator_reaches_the_copies() {
    let h = harness(10, 1 << 30).await;
    std::fs::write(h.path().join("mine.dump"), b"PGDMP").unwrap();
    for role in [Role::Operator, Role::Viewer] {
        let session = support::sign_in(&h.pool, role).await;
        for request in [
            get("/api/v1/backups"),
            get("/api/v1/backups/mine.dump/download"),
            Request::post("/api/v1/backups")
                .body(Body::empty())
                .unwrap(),
            upload(b"PGDMP".to_vec()),
            Request::delete("/api/v1/backups/mine.dump")
                .body(Body::empty())
                .unwrap(),
        ] {
            let what = format!("{} {}", request.method(), request.uri());
            let (status, _, _) = send(&h, &session, request).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{role:?} {what}");
        }
    }
    assert!(h.path().join("mine.dump").exists());
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn without_the_tools_nothing_is_attempted_and_the_page_says_why() {
    let database = support::database().await;
    let dir = TempDir::new().unwrap();
    let state = ApiState::new(database.clone()).with_backups(Backups::new(BackupSettings {
        dir: dir.path().to_owned(),
        kept: 10,
        tools: PathBuf::from("/nonexistent/postgresql/bin"),
        connection: connection(),
        max_upload_bytes: 1 << 20,
    }));
    let h = Harness {
        admin: support::administrator(database.pool()).await,
        pool: database.pool().clone(),
        state,
        dir,
    };
    let view = listed(&h).await;
    assert_eq!(view["tools"]["ready"], false);
    assert!(
        view["tools"]["problem"]
            .as_str()
            .unwrap()
            .contains("/nonexistent/postgresql/bin"),
        "{view}"
    );
    let (status, body, _) = send(
        &h,
        &h.admin,
        Request::post("/api/v1/backups")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let problem: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(problem["code"], "BACKUPS_UNAVAILABLE");
}

/// One test at a time changes the schedule, which every test shares.
static SCHEDULE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn put_schedule(body: &Value) -> Request<Body> {
    Request::put("/api/v1/backups/schedule")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_schedule_is_set_audited_and_shown() {
    let _one = SCHEDULE.lock().await;
    let h = harness(10, 1 << 20).await;
    let (status, body, _) = send(
        &h,
        &h.admin,
        put_schedule(&serde_json::json!({ "enabled": false, "time": "04:30" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let view = listed(&h).await;
    assert_eq!(view["schedule"]["enabled"], false);
    assert_eq!(view["schedule"]["time"], "04:30");
    assert!(view["schedule"]["next_at"].is_null());
    assert_eq!(
        view["schedule"]["updated_by"],
        h.admin.user.username.as_str()
    );
    assert_eq!(
        audited(&h, "backup.schedule_changed").await,
        [("schedule".to_owned(), "success".to_owned())]
    );

    let (status, body, _) = send(
        &h,
        &h.admin,
        put_schedule(&serde_json::json!({ "enabled": true, "time": "03:00" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let schedule: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(schedule["enabled"], true);
    let next = time::OffsetDateTime::parse(
        schedule["next_at"].as_str().unwrap(),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    assert_eq!((next.hour(), next.minute(), next.second()), (3, 0, 0));
    assert!(next > time::OffsetDateTime::now_utc());

    for refused in ["24:00", "3:00", "noon"] {
        let (status, _, _) = send(
            &h,
            &h.admin,
            put_schedule(&serde_json::json!({ "enabled": true, "time": refused })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    }
    let operator = support::sign_in(&h.pool, Role::Operator).await;
    let (status, _, _) = send(
        &h,
        &operator,
        put_schedule(&serde_json::json!({ "enabled": false, "time": "03:00" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
