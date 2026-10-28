// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The pool never lends a connection that is inside a transaction, against
//! a real PostgreSQL.
//!
//! sqlx 0.8 gives a connection back inside a transaction when a `begin()` is
//! dropped between sending `BEGIN` and reading its answer; the next borrower's
//! statements then join it and are never committed. These tests write no
//! rows: they ask the server about the connection itself.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::time::Duration;

use sqlx::{Executor as _, PgPool};
use tangible_db::{Database, DbConfig};

/// A pool of one connection, so the next borrower gets the connection the
/// last one gave back, if it was kept.
async fn pool_of_one() -> PgPool {
    let url = std::env::var("TANGIBLE_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TANGIBLE_DATABASE_URL"))
        .expect("set TANGIBLE_TEST_DATABASE_URL to run integration tests");
    let mut config = DbConfig::new(url);
    config.max_connections = 1;
    Database::connect(&config)
        .await
        .expect("connect")
        .pool()
        .clone()
}

async fn backend(pool: &PgPool) -> i32 {
    sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(pool)
        .await
        .expect("pid")
}

/// As the pool asks: a simple query, whose transaction starts with it
/// unless an earlier `BEGIN` started one.
async fn in_a_transaction(pool: &PgPool) -> bool {
    use sqlx::Row as _;
    pool.fetch_one("SELECT transaction_timestamp() <> statement_timestamp()")
        .await
        .expect("ask")
        .get(0)
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_connection_given_back_inside_a_transaction_is_closed_not_lent() {
    let pool = pool_of_one().await;
    let first = backend(&pool).await;
    {
        // What a dropped begin() leaves: the server in a transaction, and
        // sqlx not counting it.
        let mut connection = pool.acquire().await.expect("acquire");
        connection.execute("BEGIN").await.expect("begin");
    }
    assert!(
        !in_a_transaction(&pool).await,
        "the next borrower is not inside the abandoned transaction"
    );
    assert_ne!(
        backend(&pool).await,
        first,
        "the connection was closed, which rolled its transaction back"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_connection_given_back_cleanly_is_kept() {
    let pool = pool_of_one().await;
    let first = backend(&pool).await;
    let mut transaction = pool.begin().await.expect("begin");
    sqlx::query("SELECT 1")
        .execute(&mut *transaction)
        .await
        .expect("select");
    transaction.commit().await.expect("commit");
    // A transaction dropped without committing is rolled back by sqlx
    // itself, which counted it.
    let transaction = pool.begin().await.expect("begin");
    drop(transaction);
    assert!(!in_a_transaction(&pool).await);
    assert_eq!(backend(&pool).await, first, "the same connection, kept");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn begins_cut_short_at_any_point_never_leave_a_transaction_open() {
    let pool = pool_of_one().await;
    for micros in (0..400).step_by(2) {
        let _ = tokio::time::timeout(Duration::from_micros(micros), async {
            let transaction = pool.begin().await;
            tokio::task::yield_now().await;
            transaction
        })
        .await;
        assert!(
            !in_a_transaction(&pool).await,
            "a begin() cut short after {micros} microseconds left a transaction open"
        );
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_server_ends_a_transaction_left_idle() {
    let pool = pool_of_one().await;
    let timeout: String = sqlx::query_scalar("SHOW idle_in_transaction_session_timeout")
        .fetch_one(&pool)
        .await
        .expect("show");
    assert_eq!(timeout, "5min");
}
