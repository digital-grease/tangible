// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Restoring a copy of the database through the routes (ADR 0026,
//! docs/plans/backups-ui.md D4), against PostgreSQL and its own tools: the
//! copy's rows and nothing after it, the server answering 503 meanwhile; a
//! copy from a later version or another schema refused; a copy's own
//! definitions never run; work under way refusing it; and a restore that
//! fails leaving the database as it was.
//!
//! Each test restores into a database of its own on the test server, which
//! it makes and drops: a restore renames the database it replaces.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::PathBuf;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::Value;
use sqlx::PgPool;
use tangible_api::backups::{BackupSettings, Backups, ToolConnection};
use tangible_api::restoring::RestoreOrder;
use tangible_api::{ApiState, router};
use tangible_db::{Database, DbConfig};
use tangible_domain::{Role, SecretString};
use tangible_storage::{FilesystemStore, ManifestStore};
use tempfile::TempDir;
use tokio::sync::mpsc::Receiver;
use tower::ServiceExt as _;

mod support;

fn base_url() -> String {
    std::env::var("TANGIBLE_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TANGIBLE_DATABASE_URL"))
        .expect("set TANGIBLE_TEST_DATABASE_URL to run integration tests")
}

fn tools() -> PathBuf {
    std::env::var_os("TANGIBLE_TEST_POSTGRES_TOOLS_DIR").map_or_else(
        || PathBuf::from(tangible_api::backups::DEFAULT_TOOLS_DIR),
        PathBuf::from,
    )
}

fn url_for(database: &str) -> String {
    let mut url = reqwest::Url::parse(&base_url()).unwrap();
    url.set_path(&format!("/{database}"));
    url.to_string()
}

fn connection(database: &str) -> ToolConnection {
    let url = reqwest::Url::parse(&base_url()).unwrap();
    ToolConnection {
        host: url.host_str().unwrap().to_owned(),
        port: url.port().unwrap_or(5432),
        user: url.username().to_owned(),
        password: url.password().map(|p| SecretString::from(p.to_owned())),
        database: database.to_owned(),
        sslmode: None,
    }
}

struct Restorable {
    name: String,
    dir: TempDir,
    _library: TempDir,
    state: ApiState,
    orders: Receiver<RestoreOrder>,
    pool: PgPool,
    admin: support::Session,
}

/// A database of the test's own, migrated, with an administrator, copies
/// kept in a directory of its own and restores sent to the test.
async fn restorable() -> Restorable {
    let name = format!("tgr_{}", &uuid::Uuid::now_v7().simple().to_string()[16..]);
    let server = Database::connect(&DbConfig::new(base_url())).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(server.pool())
        .await
        .unwrap();
    server.pool().close().await;
    let database = Database::connect(&DbConfig::new(url_for(&name)))
        .await
        .unwrap();
    database.migrate().await.unwrap();
    let library = TempDir::new().unwrap();
    let objects = FilesystemStore::open(library.path().join("library"))
        .await
        .unwrap();
    let manifests = ManifestStore::open(objects).await.unwrap();
    let dir = TempDir::new().unwrap();
    let backups = Backups::new(BackupSettings {
        dir: dir.path().to_owned(),
        kept: 10,
        tools: tools(),
        connection: connection(&name),
        max_upload_bytes: 1 << 30,
    });
    let (sender, orders) = tokio::sync::mpsc::channel(1);
    let pool = database.pool().clone();
    let admin = support::administrator(&pool).await;
    Restorable {
        state: ApiState::with_manifests(database, manifests)
            .with_backups(backups)
            .with_restore_orders(sender),
        name,
        dir,
        _library: library,
        orders,
        pool,
        admin,
    }
}

/// Drop the test's database, and its neighbours if a restore left any.
async fn drop_all(name: &str) {
    let server = Database::connect(&DbConfig::new(base_url())).await.unwrap();
    for database in [
        name.to_owned(),
        format!("{name}_restoring"),
        format!("{name}_replaced"),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS {database} WITH (FORCE)"
        )))
        .execute(server.pool())
        .await
        .unwrap();
    }
    server.pool().close().await;
}

async fn send(
    state: &ApiState,
    session: &support::Session,
    request: Request<Body>,
) -> (StatusCode, Value) {
    let app = support::signed_in(router(state.clone()), session.clone());
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// Take a copy through the route and wait for it; its name.
async fn take(r: &Restorable) -> String {
    let (status, _) = send(
        &r.state,
        &r.admin,
        Request::post("/api/v1/backups")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    for _ in 0..600 {
        let (_, view) = send(
            &r.state,
            &r.admin,
            Request::get("/api/v1/backups").body(Body::empty()).unwrap(),
        )
        .await;
        if view["operation"].is_null() {
            assert!(view["last_outcome"]["error"].is_null(), "{view}");
            return view["last_outcome"]["name"].as_str().unwrap().to_owned();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the copy was never finished");
}

fn restore(name: &str, confirm: &str) -> Request<Body> {
    Request::post(format!("/api/v1/backups/{name}/restore"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({ "confirm": confirm }).to_string(),
        ))
        .unwrap()
}

async fn usernames(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT username FROM users ORDER BY username")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn exists(database: &str) -> bool {
    let server = Database::connect(&DbConfig::new(base_url())).await.unwrap();
    let found: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(database)
            .fetch_one(server.pool())
            .await
            .unwrap();
    server.pool().close().await;
    found
}

#[tokio::test]
#[ignore = "requires PostgreSQL and its client tools"]
async fn a_copy_restores_its_rows_and_nothing_after_it() {
    let mut r = restorable().await;
    let before = support::sign_in(&r.pool, Role::Viewer).await;
    let copy = take(&r).await;
    let after = support::sign_in(&r.pool, Role::Operator).await;
    tangible_db::backups::set_schedule(&r.pool, false, 60, &r.admin.user.username)
        .await
        .unwrap();

    let (status, problem) = send(&r.state, &r.admin, restore(&copy, "not the name")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert_eq!(problem["code"], "VALIDATION_FAILED");

    let (status, progress) = send(&r.state, &r.admin, restore(&copy, &copy)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{progress}");
    assert_eq!(progress["step"], "stopping");
    // Meanwhile nothing else answers, and anyone may ask how it goes.
    let (status, problem) = send(
        &r.state,
        &r.admin,
        Request::get("/api/v1/backups").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(problem["code"], "SERVER_RESTORING");
    let status_now = router(r.state.clone())
        .oneshot(
            Request::get("/api/v1/backups/restore")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(status_now.status(), StatusCode::OK);
    let body = axum::body::to_bytes(status_now.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["restoring"], true);

    let order = r.orders.recv().await.unwrap();
    r.pool.close().await;
    let record = tangible_api::restoring::run(order, &r.state).await.unwrap();
    assert_eq!(record.outcome, "restored");
    let safety = record.safety_copy.clone().unwrap();
    assert!(r.dir.path().join(&safety).is_file(), "{safety}");
    assert!(record.report.is_some());

    let restored = Database::connect(&DbConfig::new(url_for(&r.name)))
        .await
        .unwrap();
    let pool = restored.pool();
    let names = usernames(pool).await;
    assert!(names.contains(&r.admin.user.username));
    assert!(names.contains(&before.user.username));
    assert!(!names.contains(&after.user.username), "{names:?}");
    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM user_sessions")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(sessions, 0, "every sign-in ends");
    assert_eq!(
        tangible_db::backups::schedule(pool).await.unwrap(),
        tangible_db::backups::Schedule::default(),
        "the schedule set after the copy goes with the rest"
    );
    let last = tangible_db::restores::last_restore(pool)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (last.copy_name.as_str(), last.outcome.as_str()),
        (copy.as_str(), "restored")
    );
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action = 'database.restored' AND actor_id = $1",
    )
    .bind(&r.admin.user.username)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(audited, 1);
    // Every migration this version has, and its foreign keys back.
    assert!(restored.pending_migrations().await.unwrap().is_empty());
    let keys: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_constraint WHERE contype = 'f'")
        .fetch_one(pool)
        .await
        .unwrap();
    assert!(keys > 20, "{keys}");
    restored.pool().close().await;
    assert!(!exists(&format!("{}_restoring", r.name)).await);
    assert!(!exists(&format!("{}_replaced", r.name)).await);
    drop_all(&r.name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL and its client tools"]
async fn a_copy_from_a_later_version_or_another_schema_is_refused() {
    let r = restorable().await;
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
         VALUES (99990101000000, 'from a later version', true, '\\x00', 0)",
    )
    .execute(&r.pool)
    .await
    .unwrap();
    let later = take(&r).await;
    let (status, problem) = send(&r.state, &r.admin, restore(&later, &later)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert_eq!(problem["code"], "BACKUP_NOT_RESTORABLE");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("later version"),
        "{problem}"
    );

    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 99990101000000")
        .execute(&r.pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE _sqlx_migrations SET checksum = '\\x00'
         WHERE version = (SELECT min(version) FROM _sqlx_migrations)",
    )
    .execute(&r.pool)
    .await
    .unwrap();
    // A second later, so the names differ.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let foreign = take(&r).await;
    let (status, problem) = send(&r.state, &r.admin, restore(&foreign, &foreign)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("not of a Tangible database"),
        "{problem}"
    );
    // Nothing began: the server answers as before.
    let (status, _) = send(
        &r.state,
        &r.admin,
        Request::get("/api/v1/backups").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    r.pool.close().await;
    drop_all(&r.name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL and its client tools"]
async fn a_copys_own_definitions_are_never_run() {
    let mut r = restorable().await;
    // A copy carrying what Tangible never made: a table, a function, and a
    // trigger that would refuse every new account.
    for statement in [
        "CREATE TABLE public.planted_table (x integer)",
        "INSERT INTO public.planted_table VALUES (1)",
        "CREATE FUNCTION public.planted() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'the copy''s trigger ran'; END $$",
        "CREATE TRIGGER planted BEFORE INSERT ON users FOR EACH ROW EXECUTE FUNCTION planted()",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .execute(&r.pool)
            .await
            .unwrap();
    }
    let copy = take(&r).await;
    let (status, problem) = send(&r.state, &r.admin, restore(&copy, &copy)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{problem}");
    let order = r.orders.recv().await.unwrap();
    r.pool.close().await;
    tangible_api::restoring::run(order, &r.state).await.unwrap();

    let restored = Database::connect(&DbConfig::new(url_for(&r.name)))
        .await
        .unwrap();
    let pool = restored.pool();
    let planted: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT to_regclass('public.planted_table')::text, to_regproc('public.planted')::text",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(planted, (None, None));
    // The users the copy had are back, and a new one is let in.
    assert!(usernames(pool).await.contains(&r.admin.user.username));
    support::sign_in(pool, Role::Viewer).await;
    restored.pool().close().await;
    drop_all(&r.name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL and its client tools"]
async fn a_restore_is_refused_while_work_is_under_way() {
    let r = restorable().await;
    let copy = take(&r).await;
    sqlx::query(
        "INSERT INTO integrity_checks (id, scope, state, created_by)
         VALUES ($1, 'all', 'running', 'test')",
    )
    .bind(uuid::Uuid::now_v7())
    .execute(&r.pool)
    .await
    .unwrap();
    let (status, problem) = send(&r.state, &r.admin, restore(&copy, &copy)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(problem["code"], "BACKUP_WORK_UNDER_WAY");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("a integrity check running"),
        "{problem}"
    );
    r.pool.close().await;
    drop_all(&r.name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL and its client tools"]
async fn a_restore_that_fails_leaves_the_database_as_it_was() {
    let mut r = restorable().await;
    let copy = take(&r).await;
    let after = support::sign_in(&r.pool, Role::Operator).await;
    let (status, _) = send(&r.state, &r.admin, restore(&copy, &copy)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let order = r.orders.recv().await.unwrap();
    // The copy goes between being checked and being loaded.
    std::fs::remove_file(r.dir.path().join(&copy)).unwrap();
    let failed = tangible_api::restoring::run(order, &r.state)
        .await
        .unwrap_err();
    assert!(
        failed.0.contains("loading the copy's rows failed"),
        "{}",
        failed.0
    );

    let unchanged = Database::connect(&DbConfig::new(url_for(&r.name)))
        .await
        .unwrap();
    let pool = unchanged.pool();
    assert!(usernames(pool).await.contains(&after.user.username));
    let last = tangible_db::restores::last_restore(pool)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(last.outcome, "failed");
    assert!(
        last.error
            .unwrap()
            .contains("loading the copy's rows failed")
    );
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action = 'database.restore_failed'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(audited, 1);
    unchanged.pool().close().await;
    assert!(!exists(&format!("{}_restoring", r.name)).await);
    r.pool.close().await;
    drop_all(&r.name).await;
}
