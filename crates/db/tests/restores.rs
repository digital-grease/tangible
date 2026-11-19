// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The role a restore loads a copy's rows as (ADR 0026): it may insert rows
//! and set sequences in the database being restored into, and nothing else,
//! so whatever SQL a doctored copy carries is inert. Then it is gone.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use sqlx::{Connection as _, PgConnection};
use tangible_db::restores::{self, ToolConnection};
use tangible_domain::SecretString;

fn url() -> String {
    std::env::var("TANGIBLE_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TANGIBLE_DATABASE_URL"))
        .expect("set TANGIBLE_TEST_DATABASE_URL to run integration tests")
}

/// The test server as a [`ToolConnection`] to `database`, from a URL of the
/// form `postgres://user:password@host:port/name`.
fn connection(database: &str) -> ToolConnection {
    let url = url();
    let rest = url.split_once("://").unwrap().1;
    let (credentials, rest) = rest.split_once('@').unwrap();
    let (user, password) = credentials
        .split_once(':')
        .map_or((credentials, None), |(u, p)| (u, Some(p)));
    let host_port = rest.split('/').next().unwrap();
    let (host, port) = host_port
        .split_once(':')
        .map_or((host_port, 5432), |(h, p)| (h, p.parse().unwrap()));
    ToolConnection {
        host: host.to_owned(),
        port,
        user: user.to_owned(),
        password: password.map(|p| SecretString::from(p.to_owned())),
        database: database.to_owned(),
        sslmode: None,
    }
}

/// What the loading role may not do, each refused.
const REFUSED: &[&str] = &[
    "SELECT * FROM users",
    "UPDATE backup_schedule SET enabled = false",
    "DELETE FROM backup_schedule",
    "COPY (SELECT 1) TO PROGRAM 'true'",
    "COPY backup_schedule TO '/tmp/out'",
    "SELECT pg_read_file('/etc/passwd')",
    "CREATE TABLE public.planted (x integer)",
    "CREATE FUNCTION public.planted() RETURNS integer LANGUAGE sql AS 'SELECT 1'",
    "CREATE TEMP TABLE planted (x integer)",
    "CREATE SCHEMA planted",
    "ALTER TABLE users ENABLE TRIGGER USER",
    "ALTER TABLE users DROP COLUMN username",
    "DROP TABLE users",
    "SET session_replication_role = replica",
    "CREATE ROLE planted",
];

/// A connection to `database` as the test server's own user, its owner.
async fn as_owner(database: &str) -> PgConnection {
    PgConnection::connect(&format!("{}/{database}", url().rsplit_once('/').unwrap().0))
        .await
        .unwrap()
}

fn as_role(connection: &ToolConnection, role: &str, password: &str, database: &str) -> String {
    format!(
        "postgres://{role}:{password}@{}:{}/{database}",
        connection.host, connection.port
    )
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_loading_role_may_insert_rows_and_nothing_else() {
    let live = format!("tgl_{}", &uuid::Uuid::now_v7().simple().to_string()[16..]);
    let connection = connection(&live);
    let restoring = restores::create_restoring(&connection).await.unwrap();
    let newest = tangible_db::MIGRATOR
        .iter()
        .map(|m| m.version)
        .max()
        .unwrap();
    restores::migrate_to(&connection, &restoring, newest)
        .await
        .unwrap();
    let password = "0123456789abcdef0123456789abcdef";
    let loader = restores::prepare_loader(
        &connection,
        &restoring,
        SecretString::from(password.to_owned()),
    )
    .await
    .unwrap();
    assert!(loader.tables.iter().any(|t| t == "users"));
    assert!(!loader.tables.iter().any(|t| t == "_sqlx_migrations"));

    let mut conn = PgConnection::connect(&as_role(&connection, &loader.role, password, &restoring))
        .await
        .unwrap();
    // What loading a copy's rows needs: inserting, with the triggers off.
    sqlx::query(
        "INSERT INTO backup_schedule (enabled, minute_of_day, updated_by)
         VALUES (true, 180, 'a copy')",
    )
    .execute(&mut conn)
    .await
    .unwrap();
    // And nothing else.
    for refused in REFUSED {
        let result = sqlx::query(sqlx::AssertSqlSafe(*refused))
            .execute(&mut conn)
            .await;
        assert!(result.is_err(), "the loading role was allowed: {refused}");
    }
    // A grant by one who does not own the table is only warned of, and
    // grants nothing.
    sqlx::query("GRANT ALL ON users TO PUBLIC")
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    let mut owner = as_owner(&restoring).await;
    let granted: bool =
        sqlx::query_scalar("SELECT has_table_privilege('public', 'users', 'SELECT')")
            .fetch_one(&mut owner)
            .await
            .unwrap();
    assert!(!granted);
    owner.close().await.unwrap();

    restores::finish_loader(&connection, &restoring, &loader)
        .await
        .unwrap();
    // The role is gone, and with it any way in.
    assert!(
        PgConnection::connect(&as_role(&connection, &loader.role, password, &restoring))
            .await
            .is_err()
    );
    let mut admin = PgConnection::connect(&url()).await.unwrap();
    let left: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
            .bind(&loader.role)
            .fetch_one(&mut admin)
            .await
            .unwrap();
    assert!(!left);
    // The triggers and foreign keys are back.
    let disabled: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_trigger WHERE NOT tgisinternal AND tgenabled = 'D'",
    )
    .fetch_one(&mut as_owner(&restoring).await)
    .await
    .unwrap();
    assert_eq!(disabled, 0);
    admin.close().await.unwrap();
    restores::abandon(&connection, None).await.unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn rows_that_break_a_foreign_key_are_refused_when_the_keys_go_back() {
    let live = format!("tgk_{}", &uuid::Uuid::now_v7().simple().to_string()[16..]);
    let connection = connection(&live);
    let restoring = restores::create_restoring(&connection).await.unwrap();
    let newest = tangible_db::MIGRATOR
        .iter()
        .map(|m| m.version)
        .max()
        .unwrap();
    restores::migrate_to(&connection, &restoring, newest)
        .await
        .unwrap();
    let password = "fedcba9876543210fedcba9876543210";
    let loader = restores::prepare_loader(
        &connection,
        &restoring,
        SecretString::from(password.to_owned()),
    )
    .await
    .unwrap();
    let mut conn = PgConnection::connect(&as_role(&connection, &loader.role, password, &restoring))
        .await
        .unwrap();
    // A session for an account that does not exist: let in while the keys
    // are off, refused when they come back.
    sqlx::query(
        "INSERT INTO user_sessions (id, user_id, token_hash, csrf_token, expires_at)
         VALUES ($1, $2, $3, $4, now() + interval '1 hour')",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(uuid::Uuid::now_v7())
    .bind("0".repeat(64))
    .bind("c".repeat(32))
    .execute(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();
    let error = restores::finish_loader(&connection, &restoring, &loader)
        .await
        .unwrap_err();
    assert!(format!("{error:?}").contains("foreign key"), "{error:?}");
    restores::abandon(&connection, Some(&loader.role))
        .await
        .unwrap();
    let mut admin = PgConnection::connect(&url()).await.unwrap();
    let left: (bool, bool) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1),
                EXISTS (SELECT 1 FROM pg_database WHERE datname = $2)",
    )
    .bind(&loader.role)
    .bind(&restoring)
    .fetch_one(&mut admin)
    .await
    .unwrap();
    assert_eq!(left, (false, false));
    admin.close().await.unwrap();
}
