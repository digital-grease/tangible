// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Provider identities and sign-ins under way, against a real PostgreSQL
//! (ADR 0018).
//!
//! The database is shared with every other test, so each identity and name
//! here carries a fresh id nothing else uses.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use sqlx::{Executor as _, PgPool};
use tangible_db::accounts::{create_user, user_for_sign_in};
use tangible_db::oidc::{
    NewLogin, begin_login, create_identity_user, follow_provider_role, identity_users, take_login,
    user_for_identity,
};
use tangible_db::{Database, DbConfig};
use tangible_domain::Role;

async fn pool() -> PgPool {
    let url = std::env::var("TANGIBLE_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TANGIBLE_DATABASE_URL"))
        .expect("set TANGIBLE_TEST_DATABASE_URL to run integration tests");
    let database = Database::connect(&DbConfig::new(url))
        .await
        .expect("connect");
    database.migrate().await.expect("migrate");
    database.pool().clone()
}

fn tag() -> String {
    uuid::Uuid::now_v7().simple().to_string()
}

/// 64 lowercase hex digits nothing else has, as a hash column holds.
fn hex64(_tag: &str, _what: &str) -> String {
    format!(
        "{}{}",
        uuid::Uuid::now_v7().simple(),
        uuid::Uuid::now_v7().simple()
    )
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_sign_in_under_way_is_taken_once_and_not_after_it_expires() {
    let pool = pool().await;
    let tag = tag();
    let state = hex64(&tag, "state");
    let login = NewLogin {
        state_hash: &state,
        browser_hash: &hex64(&tag, "browser"),
        nonce: "a-nonce-of-some-length",
        verifier: &"v".repeat(64),
        return_to: "/library",
        lifetime_seconds: 600,
    };
    begin_login(&pool, &login).await.unwrap();
    let taken = take_login(&pool, &state).await.unwrap().expect("taken");
    assert_eq!(taken.return_to, "/library");
    assert_eq!(taken.nonce, "a-nonce-of-some-length");
    assert!(take_login(&pool, &state).await.unwrap().is_none(), "once");

    let expired = hex64(&tag, "expired");
    begin_login(
        &pool,
        &NewLogin {
            state_hash: &expired,
            lifetime_seconds: 1,
            ..login
        },
    )
    .await
    .unwrap();
    pool.execute(
        format!(
            "UPDATE oidc_logins SET created_at = now() - interval '1 hour',
                                    expires_at = now() - interval '1 minute'
             WHERE state_hash = '{expired}'"
        )
        .as_str(),
    )
    .await
    .unwrap();
    assert!(
        take_login(&pool, &expired).await.unwrap().is_none(),
        "expired"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_return_path_elsewhere_is_never_kept() {
    let pool = pool().await;
    let tag = tag();
    for (n, return_to) in [
        "https://evil.example/",
        "//evil.example/",
        "/\\evil.example/",
        "/library\\..\\x",
        "/library\n",
        "library",
    ]
    .into_iter()
    .enumerate()
    {
        let state = hex64(&tag, &n.to_string());
        let result = begin_login(
            &pool,
            &NewLogin {
                state_hash: &state,
                browser_hash: &hex64(&tag, "b"),
                nonce: "a-nonce-of-some-length",
                verifier: &"v".repeat(64),
                return_to,
                lifetime_seconds: 600,
            },
        )
        .await;
        assert!(result.is_err(), "{return_to}");
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_provider_identity_gets_an_account_of_its_own_whatever_its_name() {
    let pool = pool().await;
    let tag = tag();
    let name = format!("oidc-{tag}");
    // A local account already has the name the provider gives.
    create_user(&pool, &name, "$argon2id$stand-in", Role::Viewer, "test")
        .await
        .unwrap()
        .unwrap();
    let issuer = format!("https://id.example/{tag}");
    let user = create_identity_user(&pool, &issuer, "person-1", &name, Role::Operator)
        .await
        .unwrap();
    assert_eq!(user.username, format!("{name}2"), "never the local account");
    assert_eq!(user.role, Role::Operator);
    let found = user_for_identity(&pool, &issuer, "person-1")
        .await
        .unwrap()
        .expect("found by issuer and subject");
    assert_eq!(found.id, user.id);
    assert!(
        user_for_identity(&pool, &issuer, "person-2")
            .await
            .unwrap()
            .is_none()
    );

    // It has no password to sign in with.
    let record = user_for_sign_in(&pool, &user.username)
        .await
        .unwrap()
        .unwrap();
    assert!(record.password_hash.is_none());
    assert!(!record.made_by_setup);

    // A second first sign-in for the same identity finds the same account.
    let again = create_identity_user(&pool, &issuer, "person-1", &name, Role::Operator)
        .await
        .unwrap();
    assert_eq!(again.id, user.id);
    let accounts: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE username LIKE $1")
        .bind(format!("{name}%"))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(accounts, 2, "the local one and the provider's");

    assert_eq!(
        identity_users(&pool, &[user.id, found.id])
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_accounts_role_follows_the_provider_and_the_change_is_audited() {
    let pool = pool().await;
    let tag = tag();
    let issuer = format!("https://id.example/{tag}");
    let user = create_identity_user(&pool, &issuer, "p", &format!("role-{tag}"), Role::Viewer)
        .await
        .unwrap();
    let same = follow_provider_role(&pool, user.clone(), Role::Viewer)
        .await
        .unwrap();
    assert_eq!(same.role, Role::Viewer);
    let promoted = follow_provider_role(&pool, user.clone(), Role::Administrator)
        .await
        .unwrap();
    assert_eq!(promoted.role, Role::Administrator);
    let audited: Vec<(String, String)> = sqlx::query_as(
        "SELECT action, actor_id FROM audit_events WHERE target_id = $1 ORDER BY created_at",
    )
    .bind(user.id.to_string())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        audited,
        [
            ("user.created".to_owned(), "oidc".to_owned()),
            ("user.role_changed".to_owned(), "oidc".to_owned())
        ]
    );
}
