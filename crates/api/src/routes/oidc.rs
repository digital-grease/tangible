// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Signing in through an OpenID Connect provider (ADR 0018,
//! docs/plans/oidc.md O3).
//!
//! `start` sends the browser to the provider; `callback` is where the
//! provider sends it back. Both are navigations, not API calls, so they
//! answer with redirects: back to the page that asked, signed in, or to the
//! sign-in page with a stable code saying why not (`?oidc_error=...`).
//!
//! A sign-in is bound to the browser that started it by a cookie only that
//! browser holds, so nobody can hand someone else a sign-in of their own
//! (login CSRF); the state names it, once; the nonce ties the ID token to
//! it; PKCE ties the code to it.

use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;
use tangible_db::oidc::{
    NewLogin, begin_login, create_identity_user, follow_provider_role, record_refused, take_login,
    user_for_identity,
};
use tangible_integrations::oidc::OidcError;
use utoipa::{IntoParams, ToSchema};

use crate::auth::set_cookie;
use crate::oidc::{BROWSER_COOKIE, SIGN_IN_LIFETIME_SECONDS, safe_return_path, username_for};
use crate::routes::accounts::issue_session;
use crate::state::ApiState;

/// The longest state or code read from the provider's redirect.
const MAX_PARAMETER: usize = 2048;

/// Whether sign-in through a provider is offered.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct OidcStatusView {
    /// Whether a provider is configured.
    pub enabled: bool,
    /// What the sign-in page calls it.
    pub display_name: Option<String>,
    /// Who may sign in with a password: `all`, or `setup-only` (the accounts
    /// setup made).
    pub password_sign_in: String,
}

/// Where to go once signed in.
#[derive(Debug, Deserialize, IntoParams)]
pub struct StartQuery {
    /// A path on Tangible; anything else is `/`.
    pub return_to: Option<String>,
}

/// What the provider sends the browser back with.
#[derive(Debug, Deserialize, IntoParams)]
pub struct CallbackQuery {
    /// The authorization code.
    pub code: Option<String>,
    /// The state Tangible sent.
    pub state: Option<String>,
    /// The provider's error code, when it refused.
    pub error: Option<String>,
}

fn sha256_hex(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

/// A redirect the browser follows, never cached.
fn redirect(location: &str) -> Response {
    let mut response = StatusCode::SEE_OTHER.into_response();
    if let Ok(value) = HeaderValue::from_str(location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Back to the sign-in page, saying why.
fn refused(code: &str) -> Response {
    redirect(&format!("/login?oidc_error={code}"))
}

fn browser_cookie(state: &ApiState, value: &str, max_age: i64) -> String {
    let secure = if state.auth().secure_cookie {
        "; Secure"
    } else {
        ""
    };
    // Lax, not Strict: the provider sends the browser back with a top-level
    // navigation from another site, which Strict would strip it from.
    format!(
        "{BROWSER_COOKIE}={value}; Path=/api/v1/session/oidc; HttpOnly; SameSite=Lax; \
         Max-Age={max_age}{secure}"
    )
}

fn cookie_named<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| {
            pair.trim()
                .strip_prefix(name)
                .and_then(|rest| rest.strip_prefix('='))
                .filter(|value| !value.is_empty())
        })
}

/// Whether sign-in through a provider is offered, for the sign-in page.
#[utoipa::path(
    get,
    path = "/api/v1/session/oidc",
    tag = "accounts",
    description = "Whether an identity provider is configured, what to call it, and who may \
                   sign in with a password.",
    responses((status = 200, description = "How sign-in works here", body = OidcStatusView)),
)]
pub async fn status(State(state): State<ApiState>) -> Json<OidcStatusView> {
    Json(OidcStatusView {
        enabled: state.oidc().is_some(),
        display_name: state
            .oidc()
            .map(|oidc| oidc.settings().display_name.clone()),
        password_sign_in: state.auth().password_sign_in.as_str().to_owned(),
    })
}

/// Send the browser to the identity provider to sign in.
#[utoipa::path(
    get,
    path = "/api/v1/session/oidc/start",
    tag = "accounts",
    description = "A navigation, not an API call: redirects the browser to the identity \
                   provider, or to the sign-in page with `oidc_error` when it cannot.",
    params(StartQuery),
    responses((status = 303, description = "To the provider, or back with oidc_error")),
)]
pub async fn start(State(state): State<ApiState>, Query(query): Query<StartQuery>) -> Response {
    let Some(oidc) = state.oidc() else {
        return refused("OIDC_OFF");
    };
    let provider = match oidc.provider().await {
        Ok(provider) => provider,
        Err(error) => {
            tracing::warn!(error = %error, "the identity provider could not be discovered");
            return refused("OIDC_UNAVAILABLE");
        }
    };
    let Ok(begun) = provider.begin() else {
        return refused("OIDC_UNAVAILABLE");
    };
    let mut browser = [0_u8; 32];
    if getrandom::fill(&mut browser).is_err() {
        return refused("OIDC_UNAVAILABLE");
    }
    let browser = URL_SAFE_NO_PAD.encode(browser);
    let return_to = safe_return_path(query.return_to.as_deref());
    if let Err(error) = begin_login(
        state.database().pool(),
        &NewLogin {
            state_hash: &sha256_hex(&begun.state),
            browser_hash: &sha256_hex(&browser),
            nonce: &begun.nonce,
            verifier: &begun.verifier,
            return_to: &return_to,
            lifetime_seconds: SIGN_IN_LIFETIME_SECONDS,
        },
    )
    .await
    {
        tracing::error!(error = ?error, "a sign-in could not be kept");
        return refused("OIDC_UNAVAILABLE");
    }
    let mut response = redirect(begun.url.as_str());
    set_cookie(
        &mut response,
        &browser_cookie(&state, &browser, SIGN_IN_LIFETIME_SECONDS),
    );
    response
}

/// Where the identity provider sends the browser back.
#[utoipa::path(
    get,
    path = "/api/v1/session/oidc/callback",
    tag = "accounts",
    description = "A navigation, not an API call: the provider's redirect back. Starts a \
                   session and redirects to the page that asked, or to the sign-in page with \
                   `oidc_error`.",
    params(CallbackQuery),
    responses((status = 303, description = "Signed in, or back with oidc_error")),
)]
pub async fn callback(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let mut response = match finish(&state, &headers, &query).await {
        Ok((session_cookie, return_to)) => {
            let mut response = redirect(&return_to);
            set_cookie(&mut response, &session_cookie);
            response
        }
        Err(code) => refused(code),
    };
    // Spent either way.
    set_cookie(&mut response, &browser_cookie(&state, "", 0));
    response
}

/// Audit a refused sign-in, and answer its code.
async fn refuse(
    database: &tangible_db::Database,
    issuer: String,
    subject: Option<String>,
    reason: &'static str,
) -> &'static str {
    tracing::warn!(reason, "sign-in through the identity provider refused");
    if let Err(error) = record_refused(database.pool(), &issuer, subject.as_deref(), reason).await {
        tracing::error!(error = ?error, "a refused sign-in could not be audited");
    }
    reason
}

/// Check the provider's redirect and start a session: the session cookie and
/// where to go, or the code saying why not.
async fn finish(
    state: &ApiState,
    headers: &HeaderMap,
    query: &CallbackQuery,
) -> Result<(String, String), &'static str> {
    let oidc = state.oidc().ok_or("OIDC_OFF")?;
    let pool = state.database().pool();
    let issuer = oidc.settings().provider.issuer.to_string();
    let refuse = |subject: Option<String>, reason: &'static str| {
        refuse(state.database(), issuer.clone(), subject, reason)
    };

    let sent_state = query
        .state
        .as_deref()
        .filter(|value| value.len() <= MAX_PARAMETER)
        .ok_or("OIDC_STATE_UNKNOWN")?;
    let Some(pending) = take_login(pool, &sha256_hex(sent_state))
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "a sign-in could not be read");
            "OIDC_UNAVAILABLE"
        })?
    else {
        // Expired, already used, or never sent by this server.
        return Err(refuse(None, "OIDC_STATE_UNKNOWN").await);
    };
    let bound = cookie_named(headers, BROWSER_COOKIE).is_some_and(|browser| {
        bool::from(
            sha256_hex(browser)
                .as_bytes()
                .ct_eq(pending.browser_hash.as_bytes()),
        )
    });
    if !bound {
        return Err(refuse(None, "OIDC_BROWSER_MISMATCH").await);
    }
    if query.error.is_some() {
        return Err(refuse(None, "OIDC_REFUSED").await);
    }
    let code = query
        .code
        .as_deref()
        .filter(|code| (1..=MAX_PARAMETER).contains(&code.len()))
        .ok_or("OIDC_REFUSED")?;

    let provider = oidc.provider().await.map_err(|error| {
        tracing::warn!(error = %error, "the identity provider could not be discovered");
        "OIDC_UNAVAILABLE"
    })?;
    let identity = match provider
        .finish(code, &pending.verifier, &pending.nonce)
        .await
    {
        Ok(identity) => identity,
        Err(error) => {
            tracing::warn!(error = %error, "the identity provider's answer was not accepted");
            let reason = match error {
                OidcError::InvalidToken(_) => "OIDC_INVALID_TOKEN",
                OidcError::Refused { .. } => "OIDC_REFUSED",
                _ => "OIDC_UNAVAILABLE",
            };
            return Err(refuse(None, reason).await);
        }
    };
    let subject = Some(identity.subject.clone());
    let Some(role) = oidc.settings().role_for(&identity.groups) else {
        return Err(refuse(subject, "OIDC_NO_ROLE").await);
    };
    let unavailable = |error: tangible_db::DbError| {
        tracing::error!(error = ?error, "an account could not be read or made");
        "OIDC_UNAVAILABLE"
    };
    let user = match user_for_identity(pool, &identity.issuer, &identity.subject)
        .await
        .map_err(unavailable)?
    {
        Some(user) if user.disabled => {
            return Err(refuse(subject, "ACCOUNT_DISABLED").await);
        }
        Some(user) => follow_provider_role(pool, user, role)
            .await
            .map_err(unavailable)?,
        None => create_identity_user(
            pool,
            &identity.issuer,
            &identity.subject,
            &username_for(&identity),
            role,
        )
        .await
        .map_err(unavailable)?,
    };
    let (session_cookie, _) = issue_session(state, &user, "oidc")
        .await
        .map_err(|_| "OIDC_UNAVAILABLE")?;
    Ok((session_cookie, pending.return_to))
}

/// The routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/session/oidc", get(status))
        .route("/session/oidc/start", get(start))
        .route("/session/oidc/callback", get(callback))
}
