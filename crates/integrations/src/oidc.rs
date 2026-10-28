// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Signing in through an OpenID Connect provider (ADR 0018,
//! docs/plans/oidc.md): the relying party's half of the authorization code
//! flow with PKCE, as OpenID Connect Core 1.0 3.1 describes it.
//!
//! [`Provider::discover`] reads the provider's discovery document and keys;
//! [`Provider::begin`] makes the URL to send a browser to, with the state,
//! nonce and PKCE verifier the server keeps; [`Provider::finish`] exchanges
//! the code the provider sent back for an ID token and checks it: its
//! signature by one of the provider's keys, its issuer, audience, authorized
//! party, times and nonce. Only then is [`Identity`] believed.
//!
//! Every request goes through the outbound client an administrator's
//! integrations use, so the provider's address is held to the same policy.
//! What the provider answers is untrusted until checked, and bounded.

use std::fmt;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use jsonwebtoken::jwk::{Jwk, JwkSet, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use url::Url;

use crate::download::{ClientError, read_text_within};
use crate::outbound::OutboundClient;

/// Who the provider is, in errors and logs.
const PROVIDER: &str = "the identity provider";

/// The most of a discovery document, JWKS or token response read.
const MAX_DOCUMENT_BYTES: usize = 256 * 1024;

/// Leeway for clocks that disagree, on every time in an ID token.
const LEEWAY_SECONDS: u64 = 60;

/// How often an unknown key id may make the keys be fetched again.
const KEY_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// Most groups taken from a token, and the longest kept.
const MAX_GROUPS: usize = 256;
const MAX_GROUP_CHARS: usize = 256;

/// The signature algorithms an ID token may use. Never `none` and never an
/// HMAC, which would make the client secret a signing key.
const ALLOWED_ALGORITHMS: [Algorithm; 9] = [
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

/// How Tangible is registered with the provider.
#[derive(Clone)]
pub struct ProviderConfig {
    /// The issuer, exactly as the provider names itself.
    pub issuer: Url,
    /// Tangible's client id there.
    pub client_id: String,
    /// Tangible's client secret there.
    pub client_secret: String,
    /// Where the provider sends the browser back to:
    /// `<public URL>/api/v1/session/oidc/callback`.
    pub redirect_uri: Url,
    /// The scopes asked for; `openid` is always among them.
    pub scopes: Vec<String>,
    /// The claim that lists a person's groups.
    pub groups_claim: String,
}

impl fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("issuer", &self.issuer.as_str())
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("redirect_uri", &self.redirect_uri.as_str())
            .field("scopes", &self.scopes)
            .field("groups_claim", &self.groups_claim)
            .finish()
    }
}

/// Why signing in through the provider failed.
#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    /// The provider could not be reached, or answered something unusable.
    #[error(transparent)]
    Provider(#[from] ClientError),
    /// The discovery document names another issuer than the one configured.
    #[error(
        "the provider's discovery document names the issuer {found:?}, not the configured {expected:?}"
    )]
    IssuerMismatch {
        /// Configured.
        expected: String,
        /// Found.
        found: String,
    },
    /// The provider cannot do what this flow needs.
    #[error("the provider cannot be used: {0}")]
    Unsupported(String),
    /// The token endpoint refused the code.
    #[error("the provider refused the sign-in: {error}{}", description.as_deref().map(|d| format!(" ({d})")).unwrap_or_default())]
    Refused {
        /// Its error code, such as `invalid_grant`.
        error: String,
        /// Its explanation, cut short.
        description: Option<String>,
    },
    /// The ID token did not check out.
    #[error("the ID token was not accepted: {0}")]
    InvalidToken(String),
    /// No randomness for the state, nonce or verifier.
    #[error("no randomness is available")]
    Entropy,
}

impl OidcError {
    /// The stable code an API answer carries.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Provider(error) => error.code(),
            Self::IssuerMismatch { .. } | Self::Unsupported(_) => "OIDC_MISCONFIGURED",
            Self::Refused { .. } => "OIDC_REFUSED",
            Self::InvalidToken(_) => "OIDC_INVALID_TOKEN",
            Self::Entropy => "INTERNAL",
        }
    }
}

/// The parts of the discovery document this flow uses.
#[derive(Debug, Clone)]
pub struct Discovery {
    /// The provider's issuer.
    pub issuer: String,
    /// Where browsers are sent to sign in.
    pub authorization_endpoint: Url,
    /// Where codes are exchanged.
    pub token_endpoint: Url,
    /// The provider's public keys.
    pub jwks_uri: Url,
    /// How the token endpoint accepts a client's credentials.
    pub token_endpoint_auth_methods_supported: Option<Vec<String>>,
    /// The PKCE methods the provider accepts.
    pub code_challenge_methods_supported: Option<Vec<String>>,
}

/// The discovery document as sent, before its addresses are parsed.
#[derive(Deserialize)]
struct RawDiscovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    #[serde(default)]
    token_endpoint_auth_methods_supported: Option<Vec<String>>,
    #[serde(default)]
    code_challenge_methods_supported: Option<Vec<String>>,
}

impl RawDiscovery {
    fn parsed(self) -> Result<Discovery, ClientError> {
        let url = |name: &str, value: &str| {
            Url::parse(value).map_err(|_| {
                ClientError::bad(
                    PROVIDER,
                    format!("its discovery document's {name} is not a URL"),
                )
            })
        };
        Ok(Discovery {
            authorization_endpoint: url("authorization_endpoint", &self.authorization_endpoint)?,
            token_endpoint: url("token_endpoint", &self.token_endpoint)?,
            jwks_uri: url("jwks_uri", &self.jwks_uri)?,
            issuer: self.issuer,
            token_endpoint_auth_methods_supported: self.token_endpoint_auth_methods_supported,
            code_challenge_methods_supported: self.code_challenge_methods_supported,
        })
    }
}

/// A sign-in under way: the URL to send the browser to, and what the server
/// keeps until the browser comes back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Begun {
    /// Where to send the browser.
    pub url: Url,
    /// The state the provider will send back, which names this sign-in.
    pub state: String,
    /// The nonce the ID token must carry.
    pub nonce: String,
    /// The PKCE verifier the code exchange must present.
    pub verifier: String,
}

/// Who the provider says signed in, once its ID token has checked out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The issuer.
    pub issuer: String,
    /// The provider's stable identifier for the person.
    pub subject: String,
    /// The username the person goes by there, if given.
    pub preferred_username: Option<String>,
    /// Their email, if given.
    pub email: Option<String>,
    /// Their groups, from the configured claim.
    pub groups: Vec<String>,
}

/// How Tangible authenticates to the token endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientAuth {
    Basic,
    Post,
}

/// What the token endpoint answers: an ID token, or an error.
#[derive(Deserialize)]
struct TokenResponse {
    id_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// The provider's keys, and when they were fetched.
struct Keys {
    set: JwkSet,
    fetched: Instant,
}

/// A provider, discovered.
pub struct Provider {
    client: OutboundClient,
    config: ProviderConfig,
    discovery: Discovery,
    auth: ClientAuth,
    keys: RwLock<Keys>,
    key_refresh: Duration,
}

impl fmt::Debug for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Provider")
            .field("config", &self.config)
            .field("discovery", &self.discovery)
            .finish_non_exhaustive()
    }
}

async fn get_document(client: &OutboundClient, url: &Url) -> Result<String, ClientError> {
    let request = client
        .get(url)
        .map_err(|error| ClientError::from_outbound(PROVIDER, error))?
        .header(reqwest::header::ACCEPT, "application/json");
    let response = client
        .send(request)
        .await
        .map_err(|error| ClientError::from_outbound(PROVIDER, error))?;
    if !response.status().is_success() {
        return Err(ClientError::bad(
            PROVIDER,
            format!(
                "{} answered {}",
                crate::outbound::redacted(url),
                response.status()
            ),
        ));
    }
    read_text_within(PROVIDER, response, MAX_DOCUMENT_BYTES).await
}

fn parse<T: serde::de::DeserializeOwned>(text: &str, what: &str) -> Result<T, ClientError> {
    serde_json::from_str(text).map_err(|error| {
        ClientError::bad(PROVIDER, format!("its {what} could not be read: {error}"))
    })
}

/// `n` random bytes, base64url without padding.
fn random(n: usize) -> Result<String, OidcError> {
    let mut bytes = vec![0_u8; n];
    getrandom::fill(&mut bytes).map_err(|_| OidcError::Entropy)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// The S256 code challenge for a verifier (RFC 7636 4.2).
#[must_use]
pub fn code_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

impl Provider {
    /// Read the provider's discovery document and keys.
    ///
    /// # Errors
    ///
    /// [`OidcError`]: the provider unreachable or unusable, its issuer not
    /// the configured one, or no PKCE S256 or usable client authentication.
    pub async fn discover(
        client: OutboundClient,
        config: ProviderConfig,
    ) -> Result<Self, OidcError> {
        let mut url = config.issuer.clone();
        // Discovery is the issuer with the well-known path appended
        // (OpenID Connect Discovery 1.0, 4), its own path kept.
        let path = format!(
            "{}/.well-known/openid-configuration",
            url.path().trim_end_matches('/')
        );
        url.set_path(&path);
        let discovery =
            parse::<RawDiscovery>(&get_document(&client, &url).await?, "discovery document")?
                .parsed()?;
        if discovery.issuer != config.issuer.as_str()
            && discovery.issuer != config.issuer.as_str().trim_end_matches('/')
        {
            return Err(OidcError::IssuerMismatch {
                expected: config.issuer.to_string(),
                found: discovery.issuer.chars().take(256).collect(),
            });
        }
        if let Some(methods) = &discovery.code_challenge_methods_supported
            && !methods.iter().any(|method| method == "S256")
        {
            return Err(OidcError::Unsupported(
                "it does not offer PKCE with S256".to_owned(),
            ));
        }
        let auth = match &discovery.token_endpoint_auth_methods_supported {
            // The default when unsaid (Discovery 1.0, 3).
            None => ClientAuth::Basic,
            Some(methods) if methods.iter().any(|m| m == "client_secret_basic") => {
                ClientAuth::Basic
            }
            Some(methods) if methods.iter().any(|m| m == "client_secret_post") => ClientAuth::Post,
            Some(_) => {
                return Err(OidcError::Unsupported(
                    "its token endpoint takes neither client_secret_basic nor \
                     client_secret_post"
                        .to_owned(),
                ));
            }
        };
        let set: JwkSet = parse(&get_document(&client, &discovery.jwks_uri).await?, "keys")?;
        Ok(Self {
            client,
            config,
            discovery,
            auth,
            keys: RwLock::new(Keys {
                set,
                fetched: Instant::now(),
            }),
            key_refresh: KEY_REFRESH_INTERVAL,
        })
    }

    /// The same provider, fetching its keys again for an unknown key id at
    /// most once per `interval` rather than once a minute. For tests.
    #[must_use]
    pub fn with_key_refresh_interval(mut self, interval: Duration) -> Self {
        self.key_refresh = interval;
        self
    }

    /// The issuer, as configured.
    #[must_use]
    pub fn issuer(&self) -> &str {
        self.discovery.issuer.as_str()
    }

    /// Start a sign-in: the URL to send the browser to, and what to keep.
    ///
    /// # Errors
    ///
    /// [`OidcError::Entropy`] when no randomness is available.
    pub fn begin(&self) -> Result<Begun, OidcError> {
        let state = random(32)?;
        let nonce = random(32)?;
        let verifier = random(48)?;
        let mut scopes = vec!["openid".to_owned()];
        scopes.extend(
            self.config
                .scopes
                .iter()
                .filter(|scope| scope.as_str() != "openid")
                .cloned(),
        );
        let mut url = self.discovery.authorization_endpoint.clone();
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.config.client_id)
            .append_pair("redirect_uri", self.config.redirect_uri.as_str())
            .append_pair("scope", &scopes.join(" "))
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge", &code_challenge(&verifier))
            .append_pair("code_challenge_method", "S256");
        Ok(Begun {
            url,
            state,
            nonce,
            verifier,
        })
    }

    /// Exchange the code the provider sent back, and check the ID token it
    /// answers with against the nonce this sign-in sent.
    ///
    /// # Errors
    ///
    /// [`OidcError`]: the exchange refused or failed, or the token not
    /// accepted.
    pub async fn finish(
        &self,
        code: &str,
        verifier: &str,
        nonce: &str,
    ) -> Result<Identity, OidcError> {
        let token = self.exchange(code, verifier).await?;
        self.check(&token, nonce).await
    }

    async fn exchange(&self, code: &str, verifier: &str) -> Result<String, OidcError> {
        // Written out before anything awaits: the serializer is not Send.
        let body = {
            let mut form = url::form_urlencoded::Serializer::new(String::new());
            form.append_pair("grant_type", "authorization_code")
                .append_pair("code", code)
                .append_pair("redirect_uri", self.config.redirect_uri.as_str())
                .append_pair("code_verifier", verifier);
            if self.auth == ClientAuth::Post {
                form.append_pair("client_id", &self.config.client_id)
                    .append_pair("client_secret", &self.config.client_secret);
            }
            form.finish()
        };
        let mut request = self
            .client
            .request(reqwest::Method::POST, &self.discovery.token_endpoint)
            .map_err(|error| ClientError::from_outbound(PROVIDER, error))?
            .header(reqwest::header::ACCEPT, "application/json")
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            );
        if self.auth == ClientAuth::Basic {
            // RFC 6749 2.3.1: each form-encoded before joining.
            let encode = |value: &str| {
                url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>()
            };
            let pair = format!(
                "{}:{}",
                encode(&self.config.client_id),
                encode(&self.config.client_secret)
            );
            request = request.header(
                reqwest::header::AUTHORIZATION,
                format!("Basic {}", STANDARD.encode(pair)),
            );
        }
        let response = self
            .client
            .send(request.body(body))
            .await
            .map_err(|error| ClientError::from_outbound(PROVIDER, error))?;
        let status = response.status();
        let text = read_text_within(PROVIDER, response, MAX_DOCUMENT_BYTES).await?;
        let answer: TokenResponse = parse(&text, "token response")?;
        if let Some(error) = answer.error {
            return Err(OidcError::Refused {
                error: error.chars().take(64).collect(),
                description: answer
                    .error_description
                    .map(|description| description.chars().take(256).collect()),
            });
        }
        if !status.is_success() {
            return Err(ClientError::bad(
                PROVIDER,
                format!("its token endpoint answered {status}"),
            )
            .into());
        }
        answer.id_token.ok_or_else(|| {
            OidcError::InvalidToken("the token response holds no ID token".to_owned())
        })
    }

    /// The key that signed a token: by its key id, refreshing the keys once
    /// when it is unknown and they were not fetched in the last minute.
    async fn key_for(&self, kid: Option<&str>, alg: Algorithm) -> Result<DecodingKey, OidcError> {
        if let Some(key) = self.find_key(kid, alg)? {
            return Ok(key);
        }
        let stale = self
            .keys
            .read()
            .map_or(true, |keys| keys.fetched.elapsed() >= self.key_refresh);
        if stale {
            let set: JwkSet = parse(
                &get_document(&self.client, &self.discovery.jwks_uri).await?,
                "keys",
            )?;
            if let Ok(mut keys) = self.keys.write() {
                *keys = Keys {
                    set,
                    fetched: Instant::now(),
                };
            }
            if let Some(key) = self.find_key(kid, alg)? {
                return Ok(key);
            }
        }
        Err(OidcError::InvalidToken(
            "it is signed by a key the provider does not publish".to_owned(),
        ))
    }

    fn find_key(
        &self,
        kid: Option<&str>,
        alg: Algorithm,
    ) -> Result<Option<DecodingKey>, OidcError> {
        let keys = self
            .keys
            .read()
            .map_err(|_| OidcError::InvalidToken("the keys could not be read".to_owned()))?;
        let usable = |jwk: &&Jwk| {
            jwk.common
                .public_key_use
                .as_ref()
                .is_none_or(|u| *u == PublicKeyUse::Signature)
                && jwk
                    .common
                    .key_algorithm
                    .is_none_or(|key_alg| key_alg.to_string() == format!("{alg:?}"))
        };
        let candidates: Vec<&Jwk> = keys.set.keys.iter().filter(usable).collect();
        let jwk = match kid {
            Some(kid) => candidates
                .into_iter()
                .find(|jwk| jwk.common.key_id.as_deref() == Some(kid)),
            // Without a key id, only an unambiguous key is used.
            None if candidates.len() == 1 => candidates.into_iter().next(),
            None => None,
        };
        jwk.map(DecodingKey::from_jwk)
            .transpose()
            .map_err(|error| OidcError::InvalidToken(format!("its key could not be used: {error}")))
    }

    /// Check an ID token and take the identity from it.
    async fn check(&self, token: &str, nonce: &str) -> Result<Identity, OidcError> {
        let invalid = |detail: String| OidcError::InvalidToken(detail);
        let header = jsonwebtoken::decode_header(token)
            .map_err(|error| invalid(format!("its header could not be read: {error}")))?;
        if !ALLOWED_ALGORITHMS.contains(&header.alg) {
            return Err(invalid(format!(
                "it is signed with {:?}, which is not accepted",
                header.alg
            )));
        }
        let key = self.key_for(header.kid.as_deref(), header.alg).await?;
        let mut validation = Validation::new(header.alg);
        validation.leeway = LEEWAY_SECONDS;
        validation.set_issuer(&[self.discovery.issuer.as_str()]);
        validation.set_audience(&[self.config.client_id.as_str()]);
        validation.set_required_spec_claims(&["exp", "iat", "iss", "aud", "sub"]);
        let claims = jsonwebtoken::decode::<serde_json::Value>(token, &key, &validation)
            .map_err(|error| invalid(format!("{error}")))?
            .claims;
        identity_from_claims(
            &claims,
            &self.discovery.issuer,
            &self.config.client_id,
            nonce,
            &self.config.groups_claim,
            jsonwebtoken::get_current_timestamp(),
        )
    }
}

/// What the signature and registered-claim checks leave: the nonce, the
/// authorized party, the issue time, and the claims taken. Separate, so it
/// can be tested without signing.
fn identity_from_claims(
    claims: &serde_json::Value,
    issuer: &str,
    client_id: &str,
    nonce: &str,
    groups_claim: &str,
    now: u64,
) -> Result<Identity, OidcError> {
    let invalid = |detail: &str| OidcError::InvalidToken(detail.to_owned());
    let text = |name: &str| claims.get(name).and_then(serde_json::Value::as_str);

    let sent = text("nonce").ok_or_else(|| invalid("it carries no nonce"))?;
    if !constant_time_eq(sent.as_bytes(), nonce.as_bytes()) {
        return Err(invalid("its nonce is not the one this sign-in sent"));
    }
    let audiences = match claims.get("aud") {
        Some(serde_json::Value::Array(list)) => list.len(),
        Some(_) => 1,
        None => 0,
    };
    match text("azp") {
        Some(azp) if azp != client_id => {
            return Err(invalid("it was issued to another client (azp)"));
        }
        None if audiences > 1 => {
            return Err(invalid(
                "it names several audiences and no authorized party",
            ));
        }
        _ => {}
    }
    let iat = claims
        .get("iat")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| invalid("its issue time is missing"))?;
    if iat > now + LEEWAY_SECONDS {
        return Err(invalid("it was issued in the future"));
    }
    let subject = text("sub")
        .filter(|sub| (1..=255).contains(&sub.len()))
        .ok_or_else(|| invalid("its subject is missing or too long"))?;
    let groups = match claims.get(groups_claim) {
        Some(serde_json::Value::Array(list)) => list
            .iter()
            .filter_map(serde_json::Value::as_str)
            .take(MAX_GROUPS)
            .map(|group| group.chars().take(MAX_GROUP_CHARS).collect())
            .collect(),
        Some(serde_json::Value::String(group)) => {
            vec![group.chars().take(MAX_GROUP_CHARS).collect()]
        }
        _ => Vec::new(),
    };
    let bounded = |value: Option<&str>| {
        value
            .map(|value| {
                value
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(256)
                    .collect::<String>()
            })
            .filter(|value| !value.is_empty())
    };
    Ok(Identity {
        issuer: issuer.to_owned(),
        subject: subject.to_owned(),
        preferred_username: bounded(text("preferred_username")),
        email: bounded(text("email")),
        groups,
    })
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn claims(extra: &serde_json::Value) -> serde_json::Value {
        let mut base = serde_json::json!({
            "iss": "https://id.example",
            "sub": "user-1",
            "aud": "tangible",
            "exp": 2_000_000_000_u64,
            "iat": 1_700_000_000_u64,
            "nonce": "n-1",
            "preferred_username": "Alice",
            "email": "alice@example.invalid",
            "groups": ["tangible-admins", "other"],
        });
        for (key, value) in extra.as_object().unwrap() {
            if value.is_null() {
                base.as_object_mut().unwrap().remove(key);
            } else {
                base[key] = value.clone();
            }
        }
        base
    }

    // By value, so each test reads `take(json!({...}))`.
    #[allow(clippy::needless_pass_by_value)]
    fn take(extra: serde_json::Value) -> Result<Identity, OidcError> {
        identity_from_claims(
            &claims(&extra),
            "https://id.example",
            "tangible",
            "n-1",
            "groups",
            1_700_000_000,
        )
    }

    #[test]
    fn takes_the_subject_names_and_groups() {
        let identity = take(serde_json::json!({})).unwrap();
        assert_eq!(identity.subject, "user-1");
        assert_eq!(identity.preferred_username.as_deref(), Some("Alice"));
        assert_eq!(identity.groups, ["tangible-admins", "other"]);
        let one = take(serde_json::json!({"groups": "solo"})).unwrap();
        assert_eq!(one.groups, ["solo"]);
        let none = take(serde_json::json!({"groups": null})).unwrap();
        assert!(none.groups.is_empty());
    }

    #[test]
    fn refuses_a_wrong_or_missing_nonce() {
        assert!(matches!(
            take(serde_json::json!({"nonce": "other"})),
            Err(OidcError::InvalidToken(detail)) if detail.contains("nonce")
        ));
        assert!(take(serde_json::json!({"nonce": null})).is_err());
    }

    #[test]
    fn refuses_another_clients_token_and_an_ambiguous_audience() {
        assert!(take(serde_json::json!({"azp": "someone-else"})).is_err());
        assert!(take(serde_json::json!({"aud": ["tangible", "other"]})).is_err());
        assert!(take(serde_json::json!({"aud": ["tangible", "other"], "azp": "tangible"})).is_ok());
    }

    #[test]
    fn refuses_a_token_from_the_future_or_without_a_subject() {
        assert!(take(serde_json::json!({"iat": 1_700_000_100_u64})).is_err());
        assert!(
            take(serde_json::json!({"iat": 1_700_000_030_u64})).is_ok(),
            "within leeway"
        );
        assert!(take(serde_json::json!({"sub": ""})).is_err());
        assert!(take(serde_json::json!({"sub": null})).is_err());
    }

    #[test]
    fn bounds_what_it_keeps() {
        let many: Vec<String> = (0..1000).map(|n| format!("g{n}")).collect();
        let identity = take(serde_json::json!({
            "groups": many,
            "preferred_username": format!("a\u{1b}[31m{}", "x".repeat(1000)),
        }))
        .unwrap();
        assert_eq!(identity.groups.len(), MAX_GROUPS);
        let name = identity.preferred_username.unwrap();
        assert!(!name.contains('\u{1b}'));
        assert_eq!(name.chars().count(), 256);
    }

    #[test]
    fn the_code_challenge_is_rfc_7636s() {
        // RFC 7636 appendix B.
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
