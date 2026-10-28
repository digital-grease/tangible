// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Signing in through an identity provider, through the router, against a
//! fake provider on this machine and a real PostgreSQL (ADR 0018,
//! docs/plans/oidc.md O3): the browser's whole round trip, with its
//! redirects and cookies, and each way it is refused.
//!
//! The database is shared, so each test's provider subjects, names and
//! groups carry a fresh id.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _};
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use tangible_api::auth::{AuthSettings, PasswordSignIn, hash_password};
use tangible_api::oidc::{Oidc, OidcSettings};
use tangible_api::{ApiState, router};
use tangible_db::accounts::create_user;
use tangible_domain::Role;
use tangible_integrations::oidc::ProviderConfig;
use tangible_integrations::outbound::{OutboundClient, OutboundPolicy};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tower::ServiceExt as _;

mod support;

// --- a fake provider -----------------------------------------------------------

struct Provider {
    address: SocketAddr,
    /// The claims of the next ID token, over the nonce.
    next: Arc<Mutex<serde_json::Value>>,
    /// The nonce of the sign-in under way, which the token carries.
    nonce: Arc<Mutex<String>>,
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
    let mut data = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = stream.read(&mut buffer).await.unwrap_or(0);
        if read == 0 {
            break;
        }
        data.extend_from_slice(&buffer[..read]);
        let text = String::from_utf8_lossy(&data).into_owned();
        if let Some(end) = text.find("\r\n\r\n") {
            let length = text[..end]
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            if data.len() >= end + 4 + length {
                break;
            }
        }
    }
    String::from_utf8_lossy(&data).into_owned()
}

async fn provider() -> Provider {
    let pkcs8 =
        EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
            .unwrap();
    let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref()).unwrap();
    let point = pair.public_key().as_ref().to_vec();
    let jwk = serde_json::json!({
        "kty": "EC", "crv": "P-256", "use": "sig", "alg": "ES256", "kid": "k1",
        "x": URL_SAFE_NO_PAD.encode(&point[1..33]),
        "y": URL_SAFE_NO_PAD.encode(&point[33..65]),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let issuer = format!("http://{address}");
    let next = Arc::new(Mutex::new(serde_json::json!({})));
    let nonce = Arc::new(Mutex::new(String::new()));
    let key = pkcs8.as_ref().to_vec();
    let (claims, current_nonce) = (Arc::clone(&next), Arc::clone(&nonce));
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let (issuer, jwk, key) = (issuer.clone(), jwk.clone(), key.clone());
            let (claims, current_nonce) = (Arc::clone(&claims), Arc::clone(&current_nonce));
            tokio::spawn(async move {
                let request = read_request(&mut stream).await;
                let line = request.lines().next().unwrap_or_default().to_owned();
                let body = if line.starts_with("GET /.well-known/openid-configuration") {
                    serde_json::json!({
                        "issuer": issuer,
                        "authorization_endpoint": format!("{issuer}/authorize"),
                        "token_endpoint": format!("{issuer}/token"),
                        "jwks_uri": format!("{issuer}/jwks"),
                    })
                    .to_string()
                } else if line.starts_with("GET /jwks") {
                    serde_json::json!({ "keys": [jwk] }).to_string()
                } else if line.starts_with("POST /token") {
                    let now = jsonwebtoken::get_current_timestamp();
                    let mut token = serde_json::json!({
                        "iss": issuer, "aud": "tangible", "iat": now, "exp": now + 300,
                        "nonce": current_nonce.lock().unwrap().clone(),
                    });
                    for (name, value) in claims.lock().unwrap().as_object().unwrap() {
                        token[name] = value.clone();
                    }
                    let mut header = Header::new(Algorithm::ES256);
                    header.kid = Some("k1".to_owned());
                    let id_token =
                        jsonwebtoken::encode(&header, &token, &EncodingKey::from_ec_der(&key))
                            .unwrap();
                    serde_json::json!({ "id_token": id_token, "token_type": "Bearer" }).to_string()
                } else {
                    String::new()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    Provider {
        address,
        next,
        nonce,
    }
}

// --- a server --------------------------------------------------------------------

struct Harness {
    app: axum::Router,
    provider: Provider,
    database: tangible_db::Database,
    tag: String,
}

async fn harness(password_sign_in: PasswordSignIn) -> Harness {
    let provider = provider().await;
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let settings = OidcSettings {
        provider: ProviderConfig {
            issuer: format!("http://{}", provider.address).parse().unwrap(),
            client_id: "tangible".to_owned(),
            client_secret: "secret".to_owned(),
            redirect_uri: "https://tangible.example/api/v1/session/oidc/callback"
                .parse()
                .unwrap(),
            scopes: vec!["profile".to_owned()],
            groups_claim: "groups".to_owned(),
        },
        display_name: "VoidAuth".to_owned(),
        admin_groups: vec![format!("admins-{tag}")],
        operator_groups: vec![format!("ops-{tag}")],
        viewer_groups: Vec::new(),
    };
    let client = OutboundClient::new(
        OutboundPolicy::for_integrations(vec!["127.0.0.1/32".parse().unwrap()], None),
        "tangible-test",
    )
    .unwrap();
    let database = support::database().await;
    let state = ApiState::new(database.clone())
        .with_auth(AuthSettings::default().with_password_sign_in(password_sign_in))
        .with_oidc(Arc::new(Oidc::new(settings, client)));
    Harness {
        app: router(state),
        provider,
        database,
        tag,
    }
}

fn header_value(response: &axum::response::Response, name: header::HeaderName) -> Vec<String> {
    response
        .headers()
        .get_all(name)
        .iter()
        .map(|value| value.to_str().unwrap().to_owned())
        .collect()
}

/// The `name=value` of a Set-Cookie for `name`.
fn set_cookie(response: &axum::response::Response, name: &str) -> Option<String> {
    header_value(response, header::SET_COOKIE)
        .into_iter()
        .find(|cookie| cookie.starts_with(&format!("{name}=")))
        .map(|cookie| cookie.split(';').next().unwrap().to_owned())
}

impl Harness {
    async fn get(&self, uri: &str, cookie: Option<&str>) -> axum::response::Response {
        let mut request = Request::builder().uri(uri);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        self.app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    /// Start a sign-in: the state sent to the provider and the browser
    /// cookie, the provider then answering for a person with these claims.
    async fn start(&self, return_to: &str, claims: serde_json::Value) -> (String, String) {
        let response = self
            .get(
                &format!("/api/v1/session/oidc/start?return_to={return_to}"),
                None,
            )
            .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = header_value(&response, header::LOCATION).remove(0);
        let url = url_query(&location);
        self.provider
            .nonce
            .lock()
            .unwrap()
            .clone_from(&url["nonce"]);
        *self.provider.next.lock().unwrap() = claims;
        let browser = set_cookie(&response, "tangible_oidc").expect("the browser cookie");
        (url["state"].clone(), browser)
    }

    async fn callback(&self, state: &str, browser: Option<&str>) -> axum::response::Response {
        self.get(
            &format!("/api/v1/session/oidc/callback?code=the-code&state={state}"),
            browser,
        )
        .await
    }

    fn person(&self, subject: &str, groups: &[String]) -> serde_json::Value {
        serde_json::json!({
            "sub": format!("{subject}-{}", self.tag),
            "preferred_username": format!("{subject}-{}", self.tag),
            "groups": groups,
        })
    }
}

fn url_query(location: &str) -> std::collections::HashMap<String, String> {
    let query = location.split_once('?').map(|(_, q)| q).unwrap_or_default();
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| {
            let decoded: String = url_decode(v);
            (k.to_owned(), decoded)
        })
        .collect()
}

/// Percent-encode a query value.
fn percent(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// Percent-decode a query value (`+` is a space).
fn url_decode(value: &str) -> String {
    let raw = value.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < raw.len() => {
                let hex = std::str::from_utf8(&raw[i + 1..i + 3]).unwrap();
                out.push(u8::from_str_radix(hex, 16).unwrap());
                i += 2;
            }
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8(out).unwrap()
}

async fn session(h: &Harness, cookie: &str) -> serde_json::Value {
    let response = h.get("/api/v1/session", Some(cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

// --- the tests -----------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_person_in_a_mapped_group_signs_in_and_their_role_follows_the_provider() {
    let h = harness(PasswordSignIn::All).await;
    let ops = vec![format!("ops-{}", h.tag)];
    let (state, browser) = h.start("/library", h.person("alice", &ops)).await;
    let response = h.callback(&state, Some(&browser)).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(header_value(&response, header::LOCATION), ["/library"]);
    let cookie = set_cookie(&response, "tangible_session").expect("a session");
    let who = session(&h, &cookie).await;
    assert_eq!(who["username"], format!("alice-{}", h.tag));
    assert_eq!(who["role"], "operator");
    // The browser cookie is spent.
    assert!(
        header_value(&response, header::SET_COOKIE)
            .iter()
            .any(|c| c.starts_with("tangible_oidc=;") && c.contains("Max-Age=0"))
    );

    // Promoted at the provider: the next sign-in is an administrator, the
    // same account.
    let admins = vec![format!("admins-{}", h.tag)];
    let (state, browser) = h.start("/", h.person("alice", &admins)).await;
    let response = h.callback(&state, Some(&browser)).await;
    let cookie = set_cookie(&response, "tangible_session").unwrap();
    let who = session(&h, &cookie).await;
    assert_eq!(who["username"], format!("alice-{}", h.tag));
    assert_eq!(who["role"], "administrator");
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn nobody_in_a_mapped_group_is_refused_and_audited() {
    let h = harness(PasswordSignIn::All).await;
    let (state, browser) = h
        .start("/", h.person("stranger", &["elsewhere".to_owned()]))
        .await;
    let response = h.callback(&state, Some(&browser)).await;
    assert_eq!(
        header_value(&response, header::LOCATION),
        ["/login?oidc_error=OIDC_NO_ROLE"]
    );
    assert!(set_cookie(&response, "tangible_session").is_none());
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events
         WHERE action = 'session.refused' AND actor_id = $1 AND metadata->>'reason' = 'OIDC_NO_ROLE'",
    )
    .bind(format!("stranger-{}", h.tag))
    .fetch_one(h.database.pool())
    .await
    .unwrap();
    assert_eq!(audited, 1);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_sign_in_works_once_and_only_in_the_browser_that_started_it() {
    let h = harness(PasswordSignIn::All).await;
    let ops = vec![format!("ops-{}", h.tag)];
    let (state, browser) = h.start("/", h.person("bob", &ops)).await;
    // Another browser, or none, cannot finish it, and it is then spent.
    let response = h
        .callback(&state, Some("tangible_oidc=someone-elses"))
        .await;
    assert_eq!(
        header_value(&response, header::LOCATION),
        ["/login?oidc_error=OIDC_BROWSER_MISMATCH"]
    );
    let response = h.callback(&state, Some(&browser)).await;
    assert_eq!(
        header_value(&response, header::LOCATION),
        ["/login?oidc_error=OIDC_STATE_UNKNOWN"],
        "once"
    );

    let (state, _) = h.start("/", h.person("bob", &ops)).await;
    let response = h.callback(&state, None).await;
    assert_eq!(
        header_value(&response, header::LOCATION),
        ["/login?oidc_error=OIDC_BROWSER_MISMATCH"]
    );
    let response = h.callback("never-sent", Some(&browser)).await;
    assert_eq!(
        header_value(&response, header::LOCATION),
        ["/login?oidc_error=OIDC_STATE_UNKNOWN"]
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_way_back_is_only_ever_a_path_on_tangible() {
    let h = harness(PasswordSignIn::All).await;
    let ops = vec![format!("ops-{}", h.tag)];
    for asked in ["https://evil.example/", "//evil.example", "/\\evil.example"] {
        let (state, browser) = h.start(&percent(asked), h.person("carol", &ops)).await;
        let response = h.callback(&state, Some(&browser)).await;
        assert_eq!(header_value(&response, header::LOCATION), ["/"], "{asked}");
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_token_from_another_issuer_is_refused() {
    let h = harness(PasswordSignIn::All).await;
    let ops = vec![format!("ops-{}", h.tag)];
    let (state, browser) = h.start("/", h.person("dave", &ops)).await;
    // The provider's next token names another issuer.
    h.provider.next.lock().unwrap()["iss"] = serde_json::json!("https://evil.example");
    let response = h.callback(&state, Some(&browser)).await;
    assert_eq!(
        header_value(&response, header::LOCATION),
        ["/login?oidc_error=OIDC_INVALID_TOKEN"]
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_sign_in_page_learns_what_is_offered() {
    let h = harness(PasswordSignIn::SetupOnly).await;
    let response = h.get("/api/v1/session/oidc", None).await;
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let status: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status["enabled"], true);
    assert_eq!(status["display_name"], "VoidAuth");
    assert_eq!(status["password_sign_in"], "setup-only");

    let off = router(ApiState::new(h.database.clone()));
    let response = off
        .oneshot(
            Request::builder()
                .uri("/api/v1/session/oidc/start")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        header_value(&response, header::LOCATION),
        ["/login?oidc_error=OIDC_OFF"]
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn with_passwords_for_setup_only_everyone_else_is_sent_to_the_provider() {
    let h = harness(PasswordSignIn::SetupOnly).await;
    let password = "correct horse battery staple".to_owned();
    let hash = hash_password(password.clone()).await.unwrap();
    let local = format!("local-{}", h.tag);
    let setup = format!("setup-{}", h.tag);
    create_user(h.database.pool(), &local, &hash, Role::Operator, "test")
        .await
        .unwrap()
        .unwrap();
    create_user(
        h.database.pool(),
        &setup,
        &hash,
        Role::Administrator,
        "setup",
    )
    .await
    .unwrap()
    .unwrap();
    let sign_in = |username: String| {
        let app = h.app.clone();
        let password = password.clone();
        async move {
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/session")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "username": username, "password": password })
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };
    let refused = sign_in(local.clone()).await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let bytes = axum::body::to_bytes(refused.into_body(), 1 << 20)
        .await
        .unwrap();
    let problem: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(problem["code"], "PASSWORD_SIGN_IN_OFF");
    assert_eq!(sign_in(setup).await.status(), StatusCode::OK, "break glass");
}
