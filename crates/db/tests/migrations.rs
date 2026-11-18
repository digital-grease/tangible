// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! A server can tell when its schema is behind, and migrating leaves a
//! database a later version migrated alone.
//!
//! Production migrations are a deliberate step, so a fresh or upgraded
//! deployment that skipped it has to be told so, by the healthcheck and at
//! start, rather than serving every request into missing tables.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use tangible_db::{Database, DbConfig, DbError, MIGRATOR, Migrated};

fn url() -> String {
    std::env::var("TANGIBLE_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TANGIBLE_DATABASE_URL"))
        .expect("set TANGIBLE_TEST_DATABASE_URL to run integration tests")
}

/// The test URL with its database name replaced.
fn with_database(url: &str, name: &str) -> String {
    let (base, _) = url.rsplit_once('/').expect("a database URL has a path");
    format!("{base}/{name}")
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_empty_database_has_every_migration_pending_until_migrated() {
    let admin = Database::connect(&DbConfig::new(url())).await.unwrap();
    let name = format!("tangible_pending_{}", uuid::Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(admin.pool())
        .await
        .unwrap();

    let fresh = Database::connect(&DbConfig::new(with_database(&url(), &name)))
        .await
        .unwrap();
    let every: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
    assert!(!every.is_empty());
    assert_eq!(fresh.pending_migrations().await.unwrap(), every);

    fresh.migrate().await.unwrap();
    assert_eq!(fresh.pending_migrations().await.unwrap(), Vec::<i64>::new());

    fresh.pool().close().await;
    // FORCE: a closed connection's server process may not have exited yet,
    // and a plain DROP then fails with "being accessed by other users".
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE {name} WITH (FORCE)"
    )))
    .execute(admin.pool())
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_database_a_later_version_migrated_is_left_alone_and_one_that_diverged_refused() {
    // Going back to an earlier release runs its `migrate` again, before its
    // server starts: a database with every migration it has, and newer ones,
    // must not stop it. One lacking a migration it has while holding one it
    // does not know is neither version's, and nothing is applied to it.
    let admin = Database::connect(&DbConfig::new(url())).await.unwrap();
    let name = format!("tangible_ahead_{}", uuid::Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(admin.pool())
        .await
        .unwrap();
    let fresh = Database::connect(&DbConfig::new(with_database(&url(), &name)))
        .await
        .unwrap();
    let every: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
    assert_eq!(
        fresh.migrate().await.unwrap(),
        Migrated::Applied(every.clone())
    );
    assert_eq!(
        fresh.migrate().await.unwrap(),
        Migrated::Applied(Vec::new())
    );

    // A migration from a later version, as it records one.
    let later = every.last().unwrap() + 1;
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
         VALUES ($1, 'from a later version', TRUE, '\\x00'::bytea, 0)",
    )
    .bind(later)
    .execute(fresh.pool())
    .await
    .unwrap();
    assert_eq!(fresh.migrate().await.unwrap(), Migrated::Ahead(vec![later]));
    assert_eq!(fresh.pending_migrations().await.unwrap(), Vec::<i64>::new());

    // And one of this version's gone from the record.
    let last = *every.last().unwrap();
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = $1")
        .bind(last)
        .execute(fresh.pool())
        .await
        .unwrap();
    match fresh.migrate().await {
        Err(DbError::MigrationsDiverged { pending, unknown }) => {
            assert_eq!(pending, vec![last]);
            assert_eq!(unknown, vec![later]);
        }
        other => panic!("expected the migrations to diverge, got {other:?}"),
    }

    fresh.pool().close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE {name} WITH (FORCE)"
    )))
    .execute(admin.pool())
    .await
    .unwrap();
}
