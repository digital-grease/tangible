// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The OpenID Connect relying party against a fake provider on this machine
//! (ADR 0018): discovery, keys, the code exchange, and every check an ID
//! token must pass, with real signatures from keys made here.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use tangible_integrations::oidc::{OidcError, Provider, ProviderConfig, code_challenge};
use tangible_integrations::outbound::{OutboundClient, OutboundPolicy};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use url::Url;

/// A signing key and its JWK.
struct Key {
    pkcs8: Vec<u8>,
    jwk: serde_json::Value,
}

fn key(kid: &str) -> Key {
    let pkcs8 =
        EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
            .unwrap();
    let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref()).unwrap();
    let point = pair.public_key().as_ref();
    assert_eq!(point.len(), 65, "an uncompressed P-256 point");
    Key {
        pkcs8: pkcs8.as_ref().to_vec(),
        jwk: serde_json::json!({
            "kty": "EC", "crv": "P-256", "use": "sig", "alg": "ES256", "kid": kid,
            "x": URL_SAFE_NO_PAD.encode(&point[1..33]),
            "y": URL_SAFE_NO_PAD.encode(&point[33..65]),
        }),
    }
}

/// What the fake provider answers, changeable as a test goes.
#[derive(Default)]
struct State {
    issuer: String,
    discovery_issuer: Option<String>,
    jwks: Vec<serde_json::Value>,
    token_answer: String,
    token_requests: Vec<String>,
    jwks_fetches: usize,
}

type Shared = Arc<Mutex<State>>;

/// Read one HTTP request: its head, and as much body as it says.
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
                        .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            if data.len() >= end + 4 + length {
                break;
            }
        }
    }
    String::from_utf8_lossy(&data).into_owned()
}

async fn provider_server() -> (SocketAddr, Shared) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let state: Shared = Arc::new(Mutex::new(State {
        issuer: format!("http://{address}"),
        ..State::default()
    }));
    let shared = Arc::clone(&state);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let state = Arc::clone(&shared);
            tokio::spawn(async move {
                let request = read_request(&mut stream).await;
                let line = request.lines().next().unwrap_or_default().to_owned();
                let body = {
                    let mut state = state.lock().unwrap();
                    if line.starts_with("GET /.well-known/openid-configuration") {
                        let issuer = state.issuer.clone();
                        serde_json::json!({
                            "issuer": state.discovery_issuer.clone().unwrap_or_else(|| issuer.clone()),
                            "authorization_endpoint": format!("{issuer}/authorize"),
                            "token_endpoint": format!("{issuer}/token"),
                            "jwks_uri": format!("{issuer}/jwks"),
                            "code_challenge_methods_supported": ["S256"],
                        })
                        .to_string()
                    } else if line.starts_with("GET /jwks") {
                        state.jwks_fetches += 1;
                        serde_json::json!({ "keys": state.jwks }).to_string()
                    } else if line.starts_with("POST /token") {
                        state.token_requests.push(request.clone());
                        state.token_answer.clone()
                    } else {
                        String::new()
                    }
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
    (address, state)
}

fn client() -> OutboundClient {
    OutboundClient::new(
        OutboundPolicy::for_integrations(vec!["127.0.0.1/32".parse().unwrap()], None),
        "tangible-test",
    )
    .unwrap()
}

fn config(address: SocketAddr) -> ProviderConfig {
    ProviderConfig {
        issuer: Url::parse(&format!("http://{address}")).unwrap(),
        client_id: "tangible".to_owned(),
        client_secret: "s3cret value".to_owned(),
        redirect_uri: Url::parse("https://tangible.example/api/v1/session/oidc/callback").unwrap(),
        scopes: vec!["profile".to_owned(), "groups".to_owned()],
        groups_claim: "groups".to_owned(),
    }
}

fn now() -> u64 {
    jsonwebtoken::get_current_timestamp()
}

/// An ID token signed by `key`, with these claims over a good set.
fn token(key: &Key, kid: &str, issuer: &str, nonce: &str, extra: &serde_json::Value) -> String {
    let mut claims = serde_json::json!({
        "iss": issuer, "sub": "person-1", "aud": "tangible", "nonce": nonce,
        "iat": now(), "exp": now() + 300,
        "preferred_username": "alice", "groups": ["tangible-admins"],
    });
    for (name, value) in extra.as_object().unwrap() {
        claims[name] = value.clone();
    }
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(kid.to_owned());
    jsonwebtoken::encode(&header, &claims, &EncodingKey::from_ec_der(&key.pkcs8)).unwrap()
}

/// A provider, discovered, with one key published.
async fn discovered(key: &Key) -> (Provider, Shared) {
    let (address, state) = provider_server().await;
    state.lock().unwrap().jwks = vec![key.jwk.clone()];
    let provider = Provider::discover(client(), config(address)).await.unwrap();
    (provider, state)
}

fn answer(state: &Shared, id_token: &str) {
    state.lock().unwrap().token_answer = serde_json::json!({
        "access_token": "unused", "token_type": "Bearer", "id_token": id_token,
    })
    .to_string();
}

#[tokio::test]
async fn a_good_sign_in_gives_the_providers_identity() {
    let k1 = key("k1");
    let (provider, state) = discovered(&k1).await;
    let begun = provider.begin().unwrap();
    let query: std::collections::HashMap<_, _> = begun.url.query_pairs().into_owned().collect();
    assert_eq!(query["response_type"], "code");
    assert_eq!(query["client_id"], "tangible");
    assert_eq!(query["scope"], "openid profile groups");
    assert_eq!(query["state"], begun.state);
    assert_eq!(query["nonce"], begun.nonce);
    assert_eq!(query["code_challenge"], code_challenge(&begun.verifier));
    assert_eq!(query["code_challenge_method"], "S256");
    assert!(begun.url.path().ends_with("/authorize"));

    let issuer = provider.issuer().to_owned();
    answer(
        &state,
        &token(&k1, "k1", &issuer, &begun.nonce, &serde_json::json!({})),
    );
    let identity = provider
        .finish("the-code", &begun.verifier, &begun.nonce)
        .await
        .unwrap();
    assert_eq!(identity.subject, "person-1");
    assert_eq!(identity.issuer, issuer);
    assert_eq!(identity.preferred_username.as_deref(), Some("alice"));
    assert_eq!(identity.groups, ["tangible-admins"]);

    // The exchange sent the code, the verifier and the redirect, and the
    // client's credentials in Basic, each form-encoded first.
    let request = state.lock().unwrap().token_requests[0].clone();
    assert!(
        request.contains("grant_type=authorization_code"),
        "{request}"
    );
    assert!(request.contains("code=the-code"), "{request}");
    assert!(
        request.contains(&format!("code_verifier={}", begun.verifier)),
        "{request}"
    );
    let basic = base64::engine::general_purpose::STANDARD.encode("tangible:s3cret+value");
    assert!(
        request.to_ascii_lowercase().contains(&format!(
            "authorization: basic {}",
            basic.to_ascii_lowercase()
        )),
        "{request}"
    );
}

#[tokio::test]
async fn a_token_that_does_not_check_out_is_refused() {
    let k1 = key("k1");
    let impostor = key("k1");
    let (provider, state) = discovered(&k1).await;
    let issuer = provider.issuer().to_owned();
    let cases: Vec<(&str, String)> = vec![
        (
            "signed by another key",
            token(&impostor, "k1", &issuer, "n", &serde_json::json!({})),
        ),
        (
            "expired",
            token(
                &k1,
                "k1",
                &issuer,
                "n",
                &serde_json::json!({"exp": now() - 3600, "iat": now() - 7200}),
            ),
        ),
        (
            "another audience",
            token(
                &k1,
                "k1",
                &issuer,
                "n",
                &serde_json::json!({"aud": "someone-else"}),
            ),
        ),
        (
            "another issuer",
            token(
                &k1,
                "k1",
                "https://evil.example",
                "n",
                &serde_json::json!({}),
            ),
        ),
        (
            "another nonce",
            token(&k1, "k1", &issuer, "not-n", &serde_json::json!({})),
        ),
        (
            "another client's",
            token(
                &k1,
                "k1",
                &issuer,
                "n",
                &serde_json::json!({"aud": ["tangible", "x"], "azp": "x"}),
            ),
        ),
    ];
    for (why, id_token) in cases {
        answer(&state, &id_token);
        let result = provider.finish("code", "verifier", "n").await;
        assert!(
            matches!(result, Err(OidcError::InvalidToken(_))),
            "{why}: {result:?}"
        );
    }

    // Unsigned, and signed with the client secret as an HMAC key.
    let unsigned = format!(
        "{}.{}.",
        URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#),
        URL_SAFE_NO_PAD.encode(r#"{"sub":"x"}"#)
    );
    answer(&state, &unsigned);
    assert!(provider.finish("code", "verifier", "n").await.is_err());
    let hmac = jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &serde_json::json!({"iss": issuer, "sub": "x", "aud": "tangible", "nonce": "n", "iat": now(), "exp": now() + 300}),
        &EncodingKey::from_secret(b"s3cret value"),
    )
    .unwrap();
    answer(&state, &hmac);
    assert!(matches!(
        provider.finish("code", "verifier", "n").await,
        Err(OidcError::InvalidToken(detail)) if detail.contains("not accepted")
    ));
}

#[tokio::test]
async fn a_new_signing_key_is_fetched_once_and_then_trusted() {
    let k1 = key("k1");
    let k2 = key("k2");
    let (provider, state) = discovered(&k1).await;
    let provider = provider.with_key_refresh_interval(Duration::ZERO);
    let issuer = provider.issuer().to_owned();
    // The provider rotates to k2.
    state.lock().unwrap().jwks = vec![k1.jwk.clone(), k2.jwk.clone()];
    answer(
        &state,
        &token(&k2, "k2", &issuer, "n", &serde_json::json!({})),
    );
    assert!(provider.finish("code", "verifier", "n").await.is_ok());
    assert_eq!(
        state.lock().unwrap().jwks_fetches,
        2,
        "discovery's, and one for k2"
    );
    // A key id nobody publishes is refused after one look.
    answer(
        &state,
        &token(&k2, "k9", &issuer, "n", &serde_json::json!({})),
    );
    assert!(matches!(
        provider.finish("code", "verifier", "n").await,
        Err(OidcError::InvalidToken(detail)) if detail.contains("does not publish")
    ));
}

#[tokio::test]
async fn the_provider_refusing_the_code_says_why() {
    let k1 = key("k1");
    let (provider, state) = discovered(&k1).await;
    state.lock().unwrap().token_answer =
        serde_json::json!({"error": "invalid_grant", "error_description": "code expired"})
            .to_string();
    let error = provider.finish("code", "verifier", "n").await.unwrap_err();
    assert!(
        matches!(&error, OidcError::Refused { error, description } if error == "invalid_grant" && description.as_deref() == Some("code expired")),
        "{error:?}"
    );
    assert_eq!(error.code(), "OIDC_REFUSED");
}

#[tokio::test]
async fn a_discovery_document_naming_another_issuer_is_refused() {
    let k1 = key("k1");
    let (address, state) = provider_server().await;
    {
        let mut state = state.lock().unwrap();
        state.jwks = vec![k1.jwk.clone()];
        state.discovery_issuer = Some("https://elsewhere.example".to_owned());
    }
    let error = Provider::discover(client(), config(address))
        .await
        .unwrap_err();
    assert!(
        matches!(error, OidcError::IssuerMismatch { .. }),
        "{error:?}"
    );
    assert_eq!(error.code(), "OIDC_MISCONFIGURED");
}

#[tokio::test]
async fn this_machine_is_refused_unless_an_administrator_allowed_it() {
    let (address, _) = provider_server().await;
    let strict =
        OutboundClient::new(OutboundPolicy::for_integrations(Vec::new(), None), "t").unwrap();
    let error = Provider::discover(strict, config(address))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "INTEGRATION_ADDRESS_REFUSED", "{error:?}");
}
