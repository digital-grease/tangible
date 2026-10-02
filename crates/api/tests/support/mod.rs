// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Signing a test in.
//!
//! Operator routes need a session. These helpers make an account and a
//! session straight in the database rather than through the sign-in route,
//! so a harness does not pay for a password hash on every test. The sign-in
//! route itself is tested in `auth_routes`.

#![allow(dead_code, clippy::expect_used)]

use axum::body::Body;
use axum::http::{HeaderValue, Request};
use sqlx::PgPool;
use tangible_api::auth::{CSRF_HEADER, NewSession, SESSION_COOKIE};
use tangible_db::accounts::{UserRecord, create_session, create_user};
use tangible_domain::Role;

/// A well-formed Argon2id hash nobody knows the password for. These accounts
/// are only ever used through sessions made directly.
const UNUSABLE_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$dGVzdHNhbHR0ZXN0c2FsdA$\
                             AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// A signed-in test account.
#[derive(Debug, Clone)]
pub struct Session {
    /// The account.
    pub user: UserRecord,
    /// The session cookie's value.
    pub token: String,
    /// The session's anti-forgery token.
    pub csrf_token: String,
}

impl Session {
    /// The `Cookie` header value that presents this session.
    pub fn cookie(&self) -> String {
        format!("{SESSION_COOKIE}={}", self.token)
    }

    /// Add this session's cookie and anti-forgery token to a request that
    /// does not already carry them.
    pub fn apply(&self, request: &mut Request<Body>) {
        let headers = request.headers_mut();
        if !headers.contains_key(axum::http::header::COOKIE) {
            headers.insert(
                axum::http::header::COOKIE,
                HeaderValue::from_str(&self.cookie()).expect("cookie header"),
            );
        }
        if !headers.contains_key(CSRF_HEADER) {
            headers.insert(
                CSRF_HEADER,
                HeaderValue::from_str(&self.csrf_token).expect("csrf header"),
            );
        }
    }
}

/// Make an account with `role` and sign it in.
pub async fn sign_in(pool: &PgPool, role: Role) -> Session {
    let username = format!("test-{}", uuid::Uuid::now_v7().simple());
    let user = create_user(pool, &username, UNUSABLE_HASH, role, "test")
        .await
        .expect("create a test account")
        .expect("a fresh username");
    let session = NewSession::generate().expect("entropy");
    create_session(
        pool,
        &user,
        &session.token_hash,
        &session.csrf_token,
        time::OffsetDateTime::now_utc() + time::Duration::hours(1),
    )
    .await
    .expect("start a test session");
    Session {
        user,
        token: session.token.expose().to_owned(),
        csrf_token: session.csrf_token,
    }
}

/// An administrator, which every operator route allows.
pub async fn administrator(pool: &PgPool) -> Session {
    sign_in(pool, Role::Administrator).await
}

/// Wrap a router so every request through it carries `session`, unless the
/// request already names a cookie of its own.
pub fn signed_in(router: axum::Router, session: Session) -> axum::Router {
    router.layer(axum::middleware::map_request(
        move |mut request: Request<Body>| {
            let session = session.clone();
            async move {
                session.apply(&mut request);
                request
            }
        },
    ))
}

/// Connect to the test database, migrated.
pub async fn database() -> tangible_db::Database {
    let url = std::env::var("TANGIBLE_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TANGIBLE_DATABASE_URL"))
        .expect("set TANGIBLE_TEST_DATABASE_URL to run integration tests");
    let database = tangible_db::Database::connect(&tangible_db::DbConfig::new(url))
        .await
        .expect("connect");
    database.migrate().await.expect("migrate");
    database
}
