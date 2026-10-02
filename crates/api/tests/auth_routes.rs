// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Setting up, signing in, and what a session does and does not allow.
//!
//! Through the real router and a real PostgreSQL, because each property here
//! spans the middleware, the handlers and the session table: a refused
//! request must be refused before the handler, an ended session must stay
//! ended, and setup must work exactly once.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use serde_json::json;
use sqlx::PgPool;
use tangible_api::auth::hash_password;
use tangible_api::{ApiState, router};
use tangible_db::{Database, DbConfig};
use tangible_domain::Role;
use tower::ServiceExt as _;

mod support;

const PASSWORD: &str = "a long enough passphrase";

struct Harness {
    router: axum::Router,
    pool: PgPool,
}

async fn harness() -> Harness {
    let database = support::database().await;
    let pool = database.pool().clone();
    Harness {
        router: router(ApiState::new(database)),
        pool,
    }
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: serde_json::Value,
}

impl Reply {
    /// The session cookie a response set, as `name=value`.
    fn cookie(&self) -> String {
        let set = self
            .headers
            .get(header::SET_COOKIE)
            .expect("a Set-Cookie header")
            .to_str()
            .expect("ascii");
        set.split(';').next().expect("a pair").to_owned()
    }

    fn set_cookie(&self) -> Option<String> {
        self.headers
            .get(header::SET_COOKIE)
            .map(|value| value.to_str().expect("ascii").to_owned())
    }
}

async fn send(
    router: &axum::Router,
    method: &str,
    uri: &str,
    cookie: Option<&str>,
    csrf: Option<&str>,
    body: Option<serde_json::Value>,
) -> Reply {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    if let Some(csrf) = csrf {
        request = request.header("x-csrf-token", csrf);
    }
    let body = match body {
        Some(body) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    let response = router
        .clone()
        .oneshot(request.body(body).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    let body = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("a JSON body")
    };
    Reply {
        status,
        headers,
        body,
    }
}

/// Make an account that can actually sign in with [`PASSWORD`].
async fn account(pool: &PgPool, role: Role) -> String {
    let username = format!("auth-{}", uuid::Uuid::now_v7().simple());
    let hash = hash_password(PASSWORD.to_owned()).await.expect("hash");
    tangible_db::accounts::create_user(pool, &username, &hash, role, "test")
        .await
        .expect("create")
        .expect("fresh name");
    username
}

async fn sign_in(router: &axum::Router, username: &str, password: &str) -> Reply {
    send(
        router,
        "POST",
        "/api/v1/session",
        None,
        None,
        Some(json!({ "username": username, "password": password })),
    )
    .await
}

// --- signing in ----------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn signing_in_sets_a_hardened_cookie_and_returns_the_csrf_token() {
    let harness = harness().await;
    let username = account(&harness.pool, Role::Operator).await;

    // Case does not matter on the way in.
    let reply = sign_in(&harness.router, &username.to_uppercase(), PASSWORD).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["username"], username);
    assert_eq!(reply.body["role"], "operator");
    let csrf = reply.body["csrf_token"].as_str().expect("csrf").to_owned();
    assert!(csrf.len() >= 32);

    let set = reply.set_cookie().expect("a cookie");
    for part in [
        "tangible_session=tgs_",
        "HttpOnly",
        "SameSite=Lax",
        "Secure",
    ] {
        assert!(set.contains(part), "{set} lacks {part}");
    }
    assert_eq!(
        reply.headers.get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );

    let cookie = reply.cookie();
    let me = send(
        &harness.router,
        "GET",
        "/api/v1/session",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.body["username"], username);
    assert_eq!(me.body["csrf_token"], csrf);

    let last: Option<time::OffsetDateTime> =
        sqlx::query_scalar("SELECT last_signed_in_at FROM users WHERE username = $1")
            .bind(&username)
            .fetch_one(&harness.pool)
            .await
            .expect("row");
    assert!(last.is_some(), "signing in is recorded on the account");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_wrong_password_and_an_unknown_name_are_indistinguishable() {
    let harness = harness().await;
    let username = account(&harness.pool, Role::Viewer).await;

    let wrong = sign_in(&harness.router, &username, "not the passphrase").await;
    let unknown = sign_in(
        &harness.router,
        &format!("nobody-{}", uuid::Uuid::now_v7().simple()),
        PASSWORD,
    )
    .await;

    for reply in [&wrong, &unknown] {
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(reply.body["code"], "UNAUTHENTICATED");
        assert!(reply.set_cookie().is_none(), "no session for a failure");
    }
    assert_eq!(wrong.body["detail"], unknown.body["detail"]);

    let refused: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action = 'session.refused' AND actor_id = $1",
    )
    .bind(&username)
    .fetch_one(&harness.pool)
    .await
    .expect("count");
    assert_eq!(refused, 1, "the failure is audited");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_disabled_account_cannot_sign_in_and_loses_its_sessions() {
    let harness = harness().await;
    let username = account(&harness.pool, Role::Operator).await;
    let cookie = sign_in(&harness.router, &username, PASSWORD).await.cookie();

    sqlx::query("UPDATE users SET disabled_at = now() WHERE username = $1")
        .bind(&username)
        .execute(&harness.pool)
        .await
        .expect("disable");

    let reply = sign_in(&harness.router, &username, PASSWORD).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

    let me = send(
        &harness.router,
        "GET",
        "/api/v1/session",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(
        me.status,
        StatusCode::UNAUTHORIZED,
        "an existing session ends too"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn repeated_failures_lock_the_name_even_against_the_right_password() {
    let harness = harness().await;
    let username = account(&harness.pool, Role::Viewer).await;

    for _ in 0..5 {
        let reply = sign_in(&harness.router, &username, "a wrong guess entirely").await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }
    let locked = sign_in(&harness.router, &username, PASSWORD).await;
    assert_eq!(locked.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(locked.body["code"], "RATE_LIMITED");
    let retry: u64 = locked
        .headers
        .get(header::RETRY_AFTER)
        .expect("Retry-After")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=900).contains(&retry), "{retry}");

    // Another name is unaffected.
    let other = account(&harness.pool, Role::Viewer).await;
    assert_eq!(
        sign_in(&harness.router, &other, PASSWORD).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn signing_out_ends_the_session_for_good() {
    let harness = harness().await;
    let username = account(&harness.pool, Role::Viewer).await;
    let reply = sign_in(&harness.router, &username, PASSWORD).await;
    let cookie = reply.cookie();
    let csrf = reply.body["csrf_token"].as_str().unwrap().to_owned();

    // Signing out is a mutation like any other.
    let refused = send(
        &harness.router,
        "DELETE",
        "/api/v1/session",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(refused.body["code"], "CSRF_TOKEN_INVALID");

    let out = send(
        &harness.router,
        "DELETE",
        "/api/v1/session",
        Some(&cookie),
        Some(&csrf),
        None,
    )
    .await;
    assert_eq!(out.status, StatusCode::NO_CONTENT);
    assert!(out.set_cookie().expect("cleared").contains("Max-Age=0"));

    let after = send(
        &harness.router,
        "GET",
        "/api/v1/session",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED);
    assert!(
        after.set_cookie().expect("cleared").contains("Max-Age=0"),
        "a dead cookie is cleared rather than resent forever"
    );

    let ended: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action = 'session.ended' AND actor_id = $1",
    )
    .bind(&username)
    .fetch_one(&harness.pool)
    .await
    .expect("count");
    assert_eq!(ended, 1);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_session_ends_when_it_expires_or_sits_idle() {
    let harness = harness().await;

    let expired = support::sign_in(&harness.pool, Role::Viewer).await;
    sqlx::query(
        "UPDATE user_sessions SET expires_at = now() - interval '1 second' WHERE user_id = $1",
    )
    .bind(expired.user.id)
    .execute(&harness.pool)
    .await
    .expect("expire");
    let reply = send(
        &harness.router,
        "GET",
        "/api/v1/session",
        Some(&expired.cookie()),
        None,
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

    let idle = support::sign_in(&harness.pool, Role::Viewer).await;
    sqlx::query(
        "UPDATE user_sessions SET last_seen_at = now() - interval '13 hours' WHERE user_id = $1",
    )
    .bind(idle.user.id)
    .execute(&harness.pool)
    .await
    .expect("idle");
    let reply = send(
        &harness.router,
        "GET",
        "/api/v1/session",
        Some(&idle.cookie()),
        None,
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

    let active = support::sign_in(&harness.pool, Role::Viewer).await;
    let reply = send(
        &harness.router,
        "GET",
        "/api/v1/session",
        Some(&active.cookie()),
        None,
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_forged_or_worker_shaped_cookie_is_nobody() {
    let harness = harness().await;
    for cookie in [
        "tangible_session=tgs_0000000000000000000000000000000000000000000000000000000000000000",
        "tangible_session=tgw_0000000000000000000000000000000000000000000000000000000000000000",
        "tangible_session=garbage",
    ] {
        let reply = send(
            &harness.router,
            "GET",
            "/api/v1/artifacts",
            Some(cookie),
            None,
            None,
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{cookie}");
    }
}

// --- what a session allows -------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn mutations_need_the_sessions_own_csrf_token() {
    let harness = harness().await;
    let mine = support::sign_in(&harness.pool, Role::Operator).await;
    let theirs = support::sign_in(&harness.pool, Role::Operator).await;
    let title = json!({ "display_title": "CSRF Test" });

    for csrf in [None, Some("wrong"), Some(theirs.csrf_token.as_str())] {
        let reply = send(
            &harness.router,
            "POST",
            "/api/v1/titles",
            Some(&mine.cookie()),
            csrf,
            Some(title.clone()),
        )
        .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{csrf:?}");
        assert_eq!(reply.body["code"], "CSRF_TOKEN_INVALID");
    }

    let reply = send(
        &harness.router,
        "POST",
        "/api/v1/titles",
        Some(&mine.cookie()),
        Some(&mine.csrf_token),
        Some(title),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn each_role_gets_what_it_should_and_no_more() {
    let harness = harness().await;
    let viewer = support::sign_in(&harness.pool, Role::Viewer).await;
    let operator = support::sign_in(&harness.pool, Role::Operator).await;
    let title = json!({ "display_title": "Role Test" });

    let call = |session: &support::Session,
                method: &'static str,
                uri: &'static str,
                body: Option<serde_json::Value>| {
        let cookie = session.cookie();
        let csrf = session.csrf_token.clone();
        let router = harness.router.clone();
        async move { send(&router, method, uri, Some(&cookie), Some(&csrf), body).await }
    };

    // A viewer reads and changes nothing.
    assert_eq!(
        call(&viewer, "GET", "/api/v1/titles", None).await.status,
        StatusCode::OK
    );
    let refused = call(&viewer, "POST", "/api/v1/titles", Some(title.clone())).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(refused.body["code"], "FORBIDDEN");
    assert_eq!(
        call(&viewer, "POST", "/api/v1/burn-jobs", Some(json!({})))
            .await
            .status,
        StatusCode::FORBIDDEN,
        "refused before the body is even read"
    );

    // An operator works the library but runs neither workers nor accounts.
    assert_eq!(
        call(&operator, "POST", "/api/v1/titles", Some(title))
            .await
            .status,
        StatusCode::CREATED
    );
    assert_eq!(
        call(
            &operator,
            "POST",
            "/api/v1/worker-enrollments",
            Some(json!({}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&operator, "GET", "/api/v1/users", None).await.status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_administrator_creates_accounts_that_can_sign_in() {
    let harness = harness().await;
    let admin = support::administrator(&harness.pool).await;
    let username = format!("made-{}", uuid::Uuid::now_v7().simple());
    let create = |body: serde_json::Value| {
        let cookie = admin.cookie();
        let csrf = admin.csrf_token.clone();
        let router = harness.router.clone();
        async move {
            send(
                &router,
                "POST",
                "/api/v1/users",
                Some(&cookie),
                Some(&csrf),
                Some(body),
            )
            .await
        }
    };

    let made =
        create(json!({ "username": username, "password": PASSWORD, "role": "operator" })).await;
    assert_eq!(made.status, StatusCode::CREATED, "{}", made.body);
    assert_eq!(made.body["role"], "operator");
    assert!(made.body.get("password").is_none() && made.body.get("password_hash").is_none());

    let again =
        create(json!({ "username": username, "password": PASSWORD, "role": "viewer" })).await;
    assert_eq!(again.status, StatusCode::CONFLICT);

    for bad in [
        json!({ "username": "Has Spaces", "password": PASSWORD, "role": "viewer" }),
        json!({ "username": "shortpw", "password": "too short", "role": "viewer" }),
        json!({ "username": "badrole", "password": PASSWORD, "role": "root" }),
    ] {
        let reply = create(bad.clone()).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }

    let listed = send(
        &harness.router,
        "GET",
        "/api/v1/users",
        Some(&admin.cookie()),
        None,
        None,
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK);
    assert!(
        listed.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|user| user["username"] == username)
    );

    let (created_by,): (String,) =
        sqlx::query_as("SELECT created_by FROM users WHERE username = $1")
            .bind(&username)
            .fetch_one(&harness.pool)
            .await
            .expect("row");
    assert_eq!(created_by, admin.user.username);

    assert_eq!(
        sign_in(&harness.router, &username, PASSWORD).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_worker_credential_opens_no_operator_route() {
    let harness = harness().await;
    for uri in ["/api/v1/artifacts", "/api/v1/burn-jobs", "/api/v1/users"] {
        let response = harness
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::AUTHORIZATION, "Bearer tgw_live_0000000000000000000000000000000000000000000000000000000000000000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn every_response_carries_the_security_headers() {
    let harness = harness().await;
    for uri in ["/livez", "/api/v1/setup", "/api/v1/artifacts"] {
        let reply = send(&harness.router, "GET", uri, None, None, None).await;
        for (name, value) in [
            ("x-content-type-options", "nosniff"),
            ("x-frame-options", "DENY"),
            ("referrer-policy", "no-referrer"),
            (
                "content-security-policy",
                "default-src 'none'; frame-ancestors 'none'",
            ),
        ] {
            assert_eq!(
                reply.headers.get(name).map(|v| v.to_str().unwrap()),
                Some(value),
                "{uri} {name}"
            );
        }
    }
}

// --- setup ---------------------------------------------------------------------

/// A database of its own, since setup only works while no account exists and
/// the shared test database has many.
struct FreshDatabase {
    admin_url: String,
    name: String,
    database: Database,
}

impl FreshDatabase {
    async fn create() -> Self {
        let admin_url = std::env::var("TANGIBLE_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("TANGIBLE_DATABASE_URL"))
            .expect("set TANGIBLE_TEST_DATABASE_URL to run integration tests");
        let name = format!("tangible_setup_{}", uuid::Uuid::now_v7().simple());
        let admin = sqlx::PgPool::connect(&admin_url).await.expect("connect");
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&admin)
            .await
            .expect("create a scratch database (the test role needs CREATEDB)");
        admin.close().await;

        let (base, _) = admin_url.rsplit_once('/').expect("a database in the URL");
        let database = Database::connect(&DbConfig::new(format!("{base}/{name}")))
            .await
            .expect("connect to the scratch database");
        database.migrate().await.expect("migrate");
        Self {
            admin_url,
            name,
            database,
        }
    }

    async fn drop(self) {
        self.database.pool().close().await;
        let admin = sqlx::PgPool::connect(&self.admin_url)
            .await
            .expect("connect");
        sqlx::query(&format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            self.name
        ))
        .execute(&admin)
        .await
        .expect("drop the scratch database");
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn setup_makes_one_administrator_and_then_closes() {
    let fresh = FreshDatabase::create().await;
    let router = router(ApiState::new(fresh.database.clone()));

    let status = send(&router, "GET", "/api/v1/setup", None, None, None).await;
    assert_eq!(status.body["needed"], true);

    let short = send(
        &router,
        "POST",
        "/api/v1/setup",
        None,
        None,
        Some(json!({ "username": "owner", "password": "short" })),
    )
    .await;
    assert_eq!(short.status, StatusCode::UNPROCESSABLE_ENTITY);

    // Two visitors at once: the lock lets exactly one through.
    let attempt = |username: &'static str| {
        let router = router.clone();
        async move {
            send(
                &router,
                "POST",
                "/api/v1/setup",
                None,
                None,
                Some(json!({ "username": username, "password": PASSWORD })),
            )
            .await
        }
    };
    let (first, second) = tokio::join!(attempt("Owner"), attempt("intruder"));
    let mut statuses = [first.status, second.status];
    statuses.sort();
    assert_eq!(statuses, [StatusCode::CREATED, StatusCode::CONFLICT]);

    let winner = if first.status == StatusCode::CREATED {
        &first
    } else {
        &second
    };
    assert_eq!(winner.body["role"], "administrator");
    let cookie = winner.cookie();
    let me = send(&router, "GET", "/api/v1/session", Some(&cookie), None, None).await;
    assert_eq!(me.status, StatusCode::OK, "setup signs the owner in");

    let status = send(&router, "GET", "/api/v1/setup", None, None, None).await;
    assert_eq!(status.body["needed"], false);
    let late = attempt("latecomer").await;
    assert_eq!(late.status, StatusCode::CONFLICT);

    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(fresh.database.pool())
        .await
        .expect("count");
    assert_eq!(users, 1);

    fresh.drop().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_plain_http_server_sets_a_cookie_its_browser_will_send_back() {
    let harness = harness().await;
    let router = router(
        ApiState::new(
            Database::connect_lazy(&DbConfig::new(
                std::env::var("TANGIBLE_TEST_DATABASE_URL")
                    .or_else(|_| std::env::var("TANGIBLE_DATABASE_URL"))
                    .unwrap(),
            ))
            .unwrap(),
        )
        .with_auth(tangible_api::AuthSettings::for_public_url(
            "http://tangible.lan:8080",
        )),
    );
    let username = account(&harness.pool, Role::Viewer).await;
    let reply = sign_in(&router, &username, PASSWORD).await;
    assert_eq!(reply.status, StatusCode::OK);
    let set = reply.set_cookie().unwrap();
    assert!(!set.contains("Secure"), "{set}");
    assert!(set.contains("HttpOnly"), "{set}");
}
