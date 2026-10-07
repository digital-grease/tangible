// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! A server can tell when its schema is behind.
//!
//! Production migrations are a deliberate step, so a fresh or upgraded
//! deployment that skipped it has to be told so, by the healthcheck and at
//! start, rather than serving every request into missing tables.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use tangible_db::{Database, DbConfig, MIGRATOR};

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
    sqlx::query(&format!("CREATE DATABASE {name}"))
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
    sqlx::query(&format!("DROP DATABASE {name}"))
        .execute(admin.pool())
        .await
        .unwrap();
}
