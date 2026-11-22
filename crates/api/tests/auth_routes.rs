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
async fn the_account_list_is_paged_oldest_first() {
    let harness = harness().await;
    let admin = support::administrator(&harness.pool).await;
    let list = |uri: String| {
        let cookie = admin.cookie();
        let router = harness.router.clone();
        async move { send(&router, "GET", &uri, Some(&cookie), None, None).await }
    };

    let first = list("/api/v1/users?limit=1".to_owned()).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    assert_eq!(first.body["items"].as_array().unwrap().len(), 1);
    let cursor = first.body["next_cursor"]
        .as_str()
        .expect("more than one account, so a cursor")
        .to_owned();
    let second = list(format!("/api/v1/users?limit=1&cursor={cursor}")).await;
    assert_eq!(second.status, StatusCode::OK, "{}", second.body);
    let (a, b) = (&first.body["items"][0], &second.body["items"][0]);
    assert_ne!(a["id"], b["id"]);
    assert!(
        a["id"].as_str().unwrap() < b["id"].as_str().unwrap(),
        "oldest first"
    );

    // Following the cursors reaches every account once, and ends.
    let mut uri = "/api/v1/users?limit=500".to_owned();
    let mut seen = std::collections::HashSet::new();
    loop {
        let page = list(uri.clone()).await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.body);
        for user in page.body["items"].as_array().unwrap() {
            assert!(
                seen.insert(user["id"].as_str().unwrap().to_owned()),
                "listed twice: {user}"
            );
        }
        match page.body["next_cursor"].as_str() {
            Some(cursor) => uri = format!("/api/v1/users?limit=500&cursor={cursor}"),
            None => break,
        }
    }
    assert!(seen.contains(&admin.user.id.to_string()));

    for bad in ["not-base64!", "bm90LWEtdXVpZA"] {
        let refused = list(format!("/api/v1/users?cursor={bad}")).await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(refused.body["code"], "INVALID_CURSOR");
    }
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

    // The list is paged, oldest first, and the database is shared with every
    // earlier run: follow the cursors to the end rather than expect one page.
    let mut found = false;
    let mut uri = "/api/v1/users?limit=500".to_owned();
    loop {
        let listed = send(
            &harness.router,
            "GET",
            &uri,
            Some(&admin.cookie()),
            None,
            None,
        )
        .await;
        assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
        found |= listed.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|user| user["username"] == username);
        match listed.body["next_cursor"].as_str() {
            Some(cursor) => uri = format!("/api/v1/users?limit=500&cursor={cursor}"),
            None => break,
        }
    }
    assert!(found, "the new account is listed");

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

/// The account `username` as the list shows it, following the cursors to
/// the end: the database is shared with every earlier run.
async fn listed(router: &axum::Router, cookie: &str, username: &str) -> Option<serde_json::Value> {
    let mut uri = "/api/v1/users?limit=500".to_owned();
    loop {
        let page = send(router, "GET", &uri, Some(cookie), None, None).await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.body);
        if let Some(user) = page.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|user| user["username"] == username)
        {
            return Some(user.clone());
        }
        match page.body["next_cursor"].as_str() {
            Some(cursor) => uri = format!("/api/v1/users?limit=500&cursor={cursor}"),
            None => return None,
        }
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn no_password_account_is_made_where_only_setup_may_use_one() {
    use tangible_api::auth::{AuthSettings, PasswordSignIn};
    let database = support::database().await;
    let pool = database.pool().clone();
    let setup_only = router(
        ApiState::new(database.clone())
            .with_auth(AuthSettings::default().with_password_sign_in(PasswordSignIn::SetupOnly)),
    );
    let all = router(ApiState::new(database));
    let admin = support::administrator(&pool).await;
    let username = format!("never-{}", uuid::Uuid::now_v7().simple());

    // Refused, saying which setting makes it so, and nothing is made.
    let refused = send(
        &setup_only,
        "POST",
        "/api/v1/users",
        Some(&admin.cookie()),
        Some(&admin.csrf_token),
        Some(json!({ "username": username, "password": PASSWORD, "role": "operator" })),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert_eq!(refused.body["code"], "PASSWORD_SIGN_IN_OFF");
    let detail = refused.body["detail"].as_str().unwrap();
    assert!(
        detail.contains("TANGIBLE_PASSWORD_SIGN_IN=setup-only"),
        "{detail}"
    );
    assert!(listed(&all, &admin.cookie(), &username).await.is_none());

    // One made before, or while passwords were for everyone, is listed as
    // unable to sign in; setup's is not, and with passwords for everyone
    // both sign in with one.
    let local = account(&pool, Role::Operator).await;
    let setup = format!("setup-{}", uuid::Uuid::now_v7().simple());
    let hash = hash_password(PASSWORD.to_owned()).await.unwrap();
    tangible_db::accounts::create_user(&pool, &setup, &hash, Role::Administrator, "setup")
        .await
        .unwrap()
        .unwrap();
    for (router, name, expected) in [
        (&setup_only, &local, "none"),
        (&setup_only, &setup, "password"),
        (&all, &local, "password"),
        (&all, &setup, "password"),
    ] {
        let user = listed(router, &admin.cookie(), name).await.unwrap();
        assert_eq!(user["signs_in_with"], expected, "{name}");
    }
}

/// The audit actions recorded against the account `id`, oldest first.
async fn audited(pool: &PgPool, id: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT action FROM audit_events WHERE target_id = $1 ORDER BY id")
        .bind(id)
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_administrator_disables_and_enables_an_account() {
    let harness = harness().await;
    let admin = support::administrator(&harness.pool).await;
    let username = account(&harness.pool, Role::Operator).await;
    let signed_in = sign_in(&harness.router, &username, PASSWORD).await;
    let id = listed(&harness.router, &admin.cookie(), &username)
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let post = |uri: String, cookie: String, csrf: String| {
        let router = harness.router.clone();
        async move { send(&router, "POST", &uri, Some(&cookie), Some(&csrf), None).await }
    };
    let as_admin = |uri: String| post(uri, admin.cookie(), admin.csrf_token.clone());

    // Disabled: its session ends at once, and it cannot sign in again.
    let disabled = as_admin(format!("/api/v1/users/{id}/disable")).await;
    assert_eq!(disabled.status, StatusCode::OK, "{}", disabled.body);
    assert_eq!(disabled.body["disabled"], true);
    assert_eq!(disabled.body["signs_in_with"], "password");
    let me = send(
        &harness.router,
        "GET",
        "/api/v1/session",
        Some(&signed_in.cookie()),
        None,
        None,
    )
    .await;
    assert_eq!(me.status, StatusCode::UNAUTHORIZED);
    let (live,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM user_sessions WHERE user_id = $1::uuid AND revoked_at IS NULL",
    )
    .bind(&id)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(live, 0, "its sessions are revoked, not merely refused");
    assert_eq!(
        sign_in(&harness.router, &username, PASSWORD).await.status,
        StatusCode::UNAUTHORIZED
    );
    // Asking again changes nothing, and is not audited again.
    let again = as_admin(format!("/api/v1/users/{id}/disable")).await;
    assert_eq!(again.status, StatusCode::OK);

    // Enabled: it signs in as before.
    let enabled = as_admin(format!("/api/v1/users/{id}/enable")).await;
    assert_eq!(enabled.status, StatusCode::OK, "{}", enabled.body);
    assert_eq!(enabled.body["disabled"], false);
    assert_eq!(
        sign_in(&harness.router, &username, PASSWORD).await.status,
        StatusCode::OK
    );
    assert_eq!(
        audited(&harness.pool, &id).await,
        ["user.created", "user.disabled", "user.enabled"]
    );

    // Not your own account; not an account that does not exist; and not by
    // anybody but an administrator.
    let own = as_admin(format!("/api/v1/users/{}/disable", admin.user.id)).await;
    assert_eq!(own.status, StatusCode::CONFLICT, "{}", own.body);
    let missing = as_admin(format!("/api/v1/users/{}/disable", uuid::Uuid::now_v7())).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    let malformed = as_admin("/api/v1/users/not-an-id/disable".to_owned()).await;
    assert_eq!(malformed.status, StatusCode::BAD_REQUEST);
    let operator = support::sign_in(&harness.pool, Role::Operator).await;
    let refused = post(
        format!("/api/v1/users/{}/disable", admin.user.id),
        operator.cookie(),
        operator.csrf_token.clone(),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn two_administrators_disabling_each_other_at_once_leave_one() {
    use tangible_db::accounts::{AccountChange, set_user_disabled};
    let fresh = FreshDatabase::create().await;
    let pool = fresh.database.pool().clone();
    let hash = hash_password(PASSWORD.to_owned()).await.unwrap();
    let mut ids = Vec::new();
    for name in ["ada", "bob"] {
        let user =
            tangible_db::accounts::create_user(&pool, name, &hash, Role::Administrator, "test")
                .await
                .unwrap()
                .unwrap();
        ids.push(user.id);
    }
    let (ada, bob) = (ids[0], ids[1]);

    // Bob disables Ada, and has not committed yet, when Ada disables Bob.
    let mut first = pool.begin().await.unwrap();
    sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1")
        .bind(ada)
        .execute(&mut *first)
        .await
        .unwrap();
    let second = tokio::spawn({
        let pool = pool.clone();
        async move { set_user_disabled(&pool, bob, true, "ada").await }
    });
    // Long enough for the second to finish, were it not made to wait.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!second.is_finished(), "it waits for the first to finish");
    first.commit().await.unwrap();

    // Once Ada is disabled, Bob is the only administrator left.
    assert_eq!(
        second.await.unwrap().unwrap(),
        AccountChange::LastAdministrator
    );
    let (enabled,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM users WHERE role = 'administrator' AND disabled_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(enabled, 1);
    fresh.drop().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn only_an_account_never_used_is_deleted_and_its_name_is_free_again() {
    let harness = harness().await;
    let admin = support::administrator(&harness.pool).await;
    let cookie = admin.cookie();
    let call = |method: &'static str, uri: String, body: Option<serde_json::Value>| {
        let router = harness.router.clone();
        let cookie = admin.cookie();
        let csrf = admin.csrf_token.clone();
        async move { send(&router, method, &uri, Some(&cookie), Some(&csrf), body).await }
    };

    // Made and never used: deleted, audited, gone, and its name free.
    let unused = account(&harness.pool, Role::Operator).await;
    let listed_unused = listed(&harness.router, &cookie, &unused).await.unwrap();
    assert_eq!(listed_unused["can_delete"], true);
    let id = listed_unused["id"].as_str().unwrap().to_owned();
    let deleted = call("DELETE", format!("/api/v1/users/{id}"), None).await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.body);
    assert!(listed(&harness.router, &cookie, &unused).await.is_none());
    assert_eq!(
        audited(&harness.pool, &id).await,
        ["user.created", "user.deleted"]
    );
    let (metadata,): (serde_json::Value,) = sqlx::query_as(
        "SELECT metadata FROM audit_events WHERE target_id = $1 AND action = 'user.deleted'",
    )
    .bind(&id)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(metadata["username"], unused.as_str());
    let again = call(
        "POST",
        "/api/v1/users".to_owned(),
        Some(json!({ "username": unused, "password": PASSWORD, "role": "viewer" })),
    )
    .await;
    assert_eq!(again.status, StatusCode::CREATED, "{}", again.body);
    assert_ne!(
        again.body["id"],
        id.as_str(),
        "a new account, not the old one"
    );
    let gone = call("DELETE", format!("/api/v1/users/{id}"), None).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);

    // Signed in once: it names someone, so it is disabled, not deleted.
    let used = account(&harness.pool, Role::Viewer).await;
    sign_in(&harness.router, &used, PASSWORD).await;
    let listed_used = listed(&harness.router, &cookie, &used).await.unwrap();
    assert_eq!(listed_used["can_delete"], false);
    let refused = call(
        "DELETE",
        format!("/api/v1/users/{}", listed_used["id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(refused.body["code"], "ACCOUNT_IN_USE");
    assert!(listed(&harness.router, &cookie, &used).await.is_some());

    // Not your own, not a malformed id, and not by an operator.
    let own = call("DELETE", format!("/api/v1/users/{}", admin.user.id), None).await;
    assert_eq!(own.status, StatusCode::CONFLICT);
    let malformed = call("DELETE", "/api/v1/users/not-an-id".to_owned(), None).await;
    assert_eq!(malformed.status, StatusCode::BAD_REQUEST);
    let operator = support::sign_in(&harness.pool, Role::Operator).await;
    let forbidden = send(
        &harness.router,
        "DELETE",
        &format!("/api/v1/users/{}", admin.user.id),
        Some(&operator.cookie()),
        Some(&operator.csrf_token),
        None,
    )
    .await;
    assert_eq!(forbidden.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_only_enabled_administrator_is_not_deleted() {
    use tangible_db::accounts::{AccountDeletion, delete_unused_user};
    let fresh = FreshDatabase::create().await;
    let pool = fresh.database.pool().clone();
    let hash = hash_password(PASSWORD.to_owned()).await.unwrap();
    let only =
        tangible_db::accounts::create_user(&pool, "only", &hash, Role::Administrator, "test")
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        delete_unused_user(&pool, only.id, "someone").await.unwrap(),
        AccountDeletion::LastAdministrator
    );
    // Beside another administrator, it goes.
    tangible_db::accounts::create_user(&pool, "other", &hash, Role::Administrator, "test")
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        delete_unused_user(&pool, only.id, "other").await.unwrap(),
        AccountDeletion::Deleted(_)
    ));
    fresh.drop().await;
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
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
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
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            self.name
        )))
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
