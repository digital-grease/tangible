// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Who is asking, and whether they may.
//!
//! People sign in with a username and password and get a session cookie.
//! Every request then passes through [`authorize`], which looks the route up
//! in [`ACCESS_RULES`] and refuses anything the table does not mention. That
//! is the property that matters: a route added without a rule is refused, not
//! left open, and a test checks that the table and the OpenAPI document name
//! the same operations.
//!
//! The pieces, and why each is shaped as it is:
//!
//! - **The cookie holds a random token; the database holds its SHA-256.** A
//!   copy of the sessions table signs nobody in.
//! - **Mutations carry an anti-forgery token** in `X-CSRF-Token`, compared in
//!   constant time with the one stored for the session. The cookie is also
//!   `SameSite=Lax`; the token is what does not depend on the browser.
//! - **Passwords are Argon2id**, hashed and checked on the blocking pool so a
//!   burst of sign-ins cannot stall the server's other work, and no more than
//!   a few at once so it cannot exhaust memory either.
//! - **An unknown username costs as much as a wrong password.** The check runs
//!   against a stand-in hash, so timing does not say which names exist.
//! - **Sign-in is rate limited per username.** Five failures in fifteen
//!   minutes and that name is refused until the window passes.
//! - **Workers have their own credential, not a session.** Their protocol
//!   routes authenticate it themselves. The one place a worker meets this
//!   module is downloading an artifact it is about to burn, which it may do
//!   only while it holds a live lease on that artifact.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use axum::extract::{FromRequestParts, MatchedPath, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;
use tangible_db::accounts::{SessionRecord, session_for_token};
use tangible_db::repositories::{authenticate_worker, worker_holds_artifact};
use tangible_domain::auth::Permission;
use tangible_domain::{ArtifactId, Role};
use time::OffsetDateTime;

use crate::problem::{ErrorCode, Problem};
use crate::state::ApiState;
use crate::worker_auth::{AuthRejection, Secret, bearer_worker_credential};

/// The session cookie's name.
pub const SESSION_COOKIE: &str = "tangible_session";

/// The header a mutation carries its anti-forgery token in.
pub const CSRF_HEADER: &str = "x-csrf-token";

/// What every session token starts with, so one is recognisable in a leak
/// report and can never be mistaken for a worker credential.
const SESSION_PREFIX: &str = "tgs_";

/// Failed sign-ins allowed per username within [`LOGIN_WINDOW`].
const LOGIN_FAILURES_ALLOWED: u32 = 5;

/// How long failed sign-ins are remembered.
const LOGIN_WINDOW: Duration = Duration::from_secs(15 * 60);

/// Usernames tracked before old entries are swept.
///
/// Bounds the limiter's memory: somebody guessing a million names cannot
/// make it hold a million entries for long.
const LOGIN_TRACKED_NAMES: usize = 10_000;

/// Password checks allowed to run at once.
///
/// Each one holds Argon2's working memory, 19 MiB with the default
/// parameters, so this is what bounds a flood of sign-ins.
const CONCURRENT_PASSWORD_CHECKS: usize = 4;

// --- settings ------------------------------------------------------------------

/// How sessions behave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthSettings {
    /// Whether the cookie is marked `Secure`.
    ///
    /// True whenever the public URL is https. False only for a server that
    /// is reached over plain HTTP on a LAN, where a `Secure` cookie would
    /// never be sent back and nobody could stay signed in.
    pub secure_cookie: bool,
    /// How long a session may sit unused before it ends.
    pub idle_timeout: Duration,
    /// How long a session lasts however much it is used.
    pub absolute_timeout: Duration,
}

impl Default for AuthSettings {
    fn default() -> Self {
        Self {
            secure_cookie: true,
            idle_timeout: Duration::from_secs(12 * 60 * 60),
            absolute_timeout: Duration::from_secs(7 * 24 * 60 * 60),
        }
    }
}

impl AuthSettings {
    /// Settings for a server reached at `public_url`.
    #[must_use]
    pub fn for_public_url(public_url: &str) -> Self {
        Self {
            secure_cookie: !public_url
                .trim()
                .get(..7)
                .is_some_and(|scheme| scheme.eq_ignore_ascii_case("http://")),
            ..Self::default()
        }
    }
}

// --- access rules --------------------------------------------------------------

/// What a route requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Anybody. The probes, and the routes used to sign in at all.
    Public,
    /// A burn worker, which the route authenticates itself.
    ///
    /// Passed through untouched: a worker has no session, and a session gives
    /// nobody a worker's rights.
    Worker,
    /// A signed-in person whose role allows the permission.
    Signed(Permission),
    /// The same, or a worker holding a live lease on the artifact in the
    /// path, which needs its bytes to write them.
    ///
    /// Only the manifest and component downloads, and only for that one
    /// artifact: a worker credential is not a way to browse the library.
    SignedOrLeaseholder(Permission),
}

use Access::{Public, Signed, SignedOrLeaseholder, Worker};
use Permission::{Burn, Catalog, Erase, Import, ManageUsers, ManageWorkers, Read};

/// Every operation the server answers, and what it requires.
///
/// The paths are the router's patterns, exactly as axum reports them. Kept
/// in one table rather than spread over the handlers so the whole policy can
/// be read, reviewed, and tested against the OpenAPI document in one place.
pub const ACCESS_RULES: &[(&str, &str, Access)] = &[
    ("GET", "/livez", Public),
    ("GET", "/readyz", Public),
    // Signing in.
    ("GET", "/api/v1/setup", Public),
    ("POST", "/api/v1/setup", Public),
    ("GET", "/api/v1/session", Signed(Read)),
    ("POST", "/api/v1/session", Public),
    ("DELETE", "/api/v1/session", Signed(Read)),
    ("GET", "/api/v1/users", Signed(ManageUsers)),
    ("POST", "/api/v1/users", Signed(ManageUsers)),
    // The library.
    ("GET", "/api/v1/artifacts", Signed(Read)),
    ("GET", "/api/v1/artifacts/{artifact_id}", Signed(Read)),
    (
        "GET",
        "/api/v1/artifacts/{artifact_id}/manifest",
        SignedOrLeaseholder(Read),
    ),
    (
        "GET",
        "/api/v1/artifacts/{artifact_id}/components",
        Signed(Read),
    ),
    (
        "GET",
        "/api/v1/artifacts/{artifact_id}/components/{component_id}/content",
        SignedOrLeaseholder(Read),
    ),
    // The catalog.
    ("GET", "/api/v1/titles", Signed(Read)),
    ("POST", "/api/v1/titles", Signed(Catalog)),
    ("GET", "/api/v1/titles/{title_id}", Signed(Read)),
    ("GET", "/api/v1/titles/{title_id}/editions", Signed(Read)),
    (
        "POST",
        "/api/v1/titles/{title_id}/editions",
        Signed(Catalog),
    ),
    (
        "GET",
        "/api/v1/editions/{edition_id}/disc-sets",
        Signed(Read),
    ),
    (
        "POST",
        "/api/v1/editions/{edition_id}/disc-sets",
        Signed(Catalog),
    ),
    ("GET", "/api/v1/disc-sets/{disc_set_id}/discs", Signed(Read)),
    (
        "POST",
        "/api/v1/disc-sets/{disc_set_id}/discs",
        Signed(Catalog),
    ),
    ("GET", "/api/v1/discs/{disc_id}", Signed(Read)),
    ("GET", "/api/v1/discs/{disc_id}/artifacts", Signed(Read)),
    (
        "POST",
        "/api/v1/discs/{disc_id}/artifact-links",
        Signed(Catalog),
    ),
    // Imports.
    ("GET", "/api/v1/import-sources", Signed(Read)),
    ("GET", "/api/v1/imports", Signed(Read)),
    ("POST", "/api/v1/imports", Signed(Import)),
    ("POST", "/api/v1/imports/upload", Signed(Import)),
    ("GET", "/api/v1/imports/{import_id}", Signed(Read)),
    ("POST", "/api/v1/imports/{import_id}/cancel", Signed(Import)),
    ("POST", "/api/v1/imports/{import_id}/retry", Signed(Import)),
    // The discs that exist.
    ("GET", "/api/v1/physical-copies", Signed(Read)),
    ("GET", "/api/v1/physical-copies/{copy_id}", Signed(Read)),
    (
        "PATCH",
        "/api/v1/physical-copies/{copy_id}",
        Signed(Catalog),
    ),
    (
        "POST",
        "/api/v1/physical-copies/{copy_id}/checks",
        Signed(Catalog),
    ),
    (
        "POST",
        "/api/v1/physical-copies/{copy_id}/mark-destroyed",
        Signed(Catalog),
    ),
    // Burns.
    ("GET", "/api/v1/burn-jobs", Signed(Read)),
    ("POST", "/api/v1/burn-jobs", Signed(Burn)),
    ("GET", "/api/v1/burn-jobs/{burn_job_id}", Signed(Read)),
    (
        "GET",
        "/api/v1/burn-jobs/{burn_job_id}/attempts",
        Signed(Read),
    ),
    (
        "POST",
        "/api/v1/burn-jobs/{burn_job_id}/cancel",
        Signed(Burn),
    ),
    (
        "POST",
        "/api/v1/burn-jobs/{burn_job_id}/retry",
        Signed(Burn),
    ),
    (
        "POST",
        "/api/v1/burn-jobs/{burn_job_id}/resolve-attention",
        Signed(Burn),
    ),
    ("GET", "/api/v1/burn-attempts/{attempt_id}", Signed(Read)),
    (
        "GET",
        "/api/v1/burn-attempts/{attempt_id}/events",
        Signed(Read),
    ),
    // Workers. Issuing a token is an administrator's act; everything else is
    // the worker protocol, authenticated by the worker's own credential or,
    // for consuming a token, by the token.
    ("POST", "/api/v1/worker-enrollments", Signed(ManageWorkers)),
    ("POST", "/api/v1/worker-enrollments/consume", Worker),
    ("PUT", "/api/v1/workers/{worker_id}/capabilities", Worker),
    ("POST", "/api/v1/workers/{worker_id}/heartbeat", Worker),
    ("POST", "/api/v1/workers/{worker_id}/claims", Worker),
    ("POST", "/api/v1/workers/{worker_id}/recoveries", Worker),
    (
        "POST",
        "/api/v1/burn-attempts/{attempt_id}/lease/renew",
        Worker,
    ),
    ("POST", "/api/v1/burn-attempts/{attempt_id}/events", Worker),
    (
        "POST",
        "/api/v1/burn-attempts/{attempt_id}/complete",
        Worker,
    ),
    // Drives, and erasing the disc in one. Erasing destroys data, so it has
    // its own permission; the worker side is the drive's own worker.
    ("GET", "/api/v1/drives", Signed(Read)),
    ("POST", "/api/v1/drives/{drive_id}/erasures", Signed(Erase)),
    ("GET", "/api/v1/erasures", Signed(Read)),
    ("GET", "/api/v1/erasures/{erasure_id}", Signed(Read)),
    (
        "POST",
        "/api/v1/erasures/{erasure_id}/cancel",
        Signed(Erase),
    ),
    ("POST", "/api/v1/workers/{worker_id}/erasure-claims", Worker),
    ("POST", "/api/v1/erasures/{erasure_id}/complete", Worker),
];

/// What a request needs, if the table names its route at all.
///
/// `Err(true)` when the path is known but not with this method: the router
/// answers that with 405 and no handler runs, so there is nothing to guard.
/// `Err(false)` when the path is not in the table at all, which is a missing
/// rule and is refused.
fn rule_for(method: &Method, path: &str) -> Result<Access, bool> {
    // The router answers HEAD with the GET handler.
    let method = if method == Method::HEAD {
        "GET"
    } else {
        method.as_str()
    };
    let mut path_known = false;
    for (rule_method, rule_path, access) in ACCESS_RULES {
        if *rule_path == path {
            if *rule_method == method {
                return Ok(*access);
            }
            path_known = true;
        }
    }
    Err(path_known)
}

// --- the signed-in person ------------------------------------------------------

/// The person behind a request, once [`authorize`] has found their session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentUser {
    /// The account.
    pub user_id: uuid::Uuid,
    /// Its username, which is what audit records and `created_by` columns
    /// name.
    pub username: String,
    /// What it may do.
    pub role: Role,
    /// The session the request came in on.
    pub session_id: uuid::Uuid,
    /// That session's anti-forgery token.
    pub csrf_token: String,
    /// When that session ends regardless of use.
    pub expires_at: OffsetDateTime,
}

impl From<SessionRecord> for CurrentUser {
    fn from(session: SessionRecord) -> Self {
        Self {
            user_id: session.user.id,
            username: session.user.username,
            role: session.user.role,
            session_id: session.id,
            csrf_token: session.csrf_token,
            expires_at: session.expires_at,
        }
    }
}

/// Extracts the signed-in person in a handler.
///
/// Refuses with 401 when there is none, so a handler that records who acted
/// cannot run anonymously even if its access rule were wrong.
#[derive(Debug, Clone)]
pub struct SignedIn(pub CurrentUser);

impl<S: Send + Sync> FromRequestParts<S> for SignedIn {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<CurrentUser>()
            .cloned()
            .map(Self)
            .ok_or_else(not_signed_in)
    }
}

fn not_signed_in() -> Problem {
    Problem::new(ErrorCode::Unauthenticated, "sign in to continue")
}

// --- the middleware ------------------------------------------------------------

/// Decide whether a request may reach its handler.
///
/// Applied to every route. A route the router matched but [`ACCESS_RULES`]
/// does not mention is refused and logged as the bug it is.
pub async fn authorize(
    State(state): State<ApiState>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(path) = request
        .extensions()
        .get::<MatchedPath>()
        .map(|matched| matched.as_str().to_owned())
    else {
        // No route matched: the router's fallback answers 404, and there is
        // no handler to protect.
        return next.run(request).await;
    };

    let access = match rule_for(request.method(), &path) {
        Ok(access) => access,
        Err(true) => return next.run(request).await,
        Err(false) => {
            tracing::error!(
                method = %request.method(),
                route = %path,
                "route has no access rule; refusing it"
            );
            return Problem::new(ErrorCode::Forbidden, "this operation is not permitted")
                .into_response();
        }
    };

    let permission = match access {
        Public | Worker => return next.run(request).await,
        Signed(permission) => permission,
        SignedOrLeaseholder(permission) => {
            // A worker sends a bearer credential and no cookie. Anything with
            // a cookie is a person and is checked as one.
            if session_cookie(request.headers()).is_none()
                && request.headers().contains_key(header::AUTHORIZATION)
            {
                return match leaseholder(&state, &mut request).await {
                    Ok(()) => next.run(request).await,
                    Err(problem) => problem.into_response(),
                };
            }
            permission
        }
    };

    let user = match current_session(&state, request.headers()).await {
        Ok(Some(user)) => user,
        Ok(None) => {
            let mut response = not_signed_in().into_response();
            // A cookie was sent and is no good: tell the browser to drop it
            // rather than send it on every request until it expires.
            if session_cookie(request.headers()).is_some() {
                set_cookie(&mut response, &clear_cookie(state.auth()));
            }
            return response;
        }
        Err(problem) => return problem.into_response(),
    };

    if !user.role.allows(permission) {
        tracing::info!(
            user = %user.username,
            role = %user.role,
            permission = permission.as_str(),
            route = %path,
            "refused: role does not allow this"
        );
        return Problem::new(
            ErrorCode::Forbidden,
            format!(
                "the {} role does not allow {}",
                user.role,
                permission.as_str()
            ),
        )
        .into_response();
    }

    if !is_safe_method(request.method()) && !csrf_matches(request.headers(), &user.csrf_token) {
        return Problem::new(
            ErrorCode::CsrfTokenInvalid,
            "this request needs the session's anti-forgery token in X-CSRF-Token",
        )
        .into_response();
    }

    request.extensions_mut().insert(user);
    next.run(request).await
}

/// Admit a worker to an artifact's bytes if, and only if, it holds a live
/// lease on an attempt to burn that artifact.
async fn leaseholder(state: &ApiState, request: &mut Request) -> Result<(), Problem> {
    use axum::RequestExt as _;
    use axum::extract::RawPathParams;

    let refused = |rejection: AuthRejection| {
        tracing::warn!(rejection = ?rejection, "worker authentication failed");
        Problem::new(ErrorCode::Unauthenticated, rejection.public_reason())
    };
    let unavailable = |error: tangible_db::DbError| {
        tracing::error!(error = ?error, "could not check a worker's lease");
        Problem::new(
            ErrorCode::StorageUnavailable,
            "the credential could not be verified",
        )
    };

    let header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or(AuthRejection::Missing)
        .map_err(refused)?;
    let secret = bearer_worker_credential(header).map_err(refused)?;
    let worker_id = authenticate_worker(state.database().pool(), secret.hash().as_str())
        .await
        .map_err(unavailable)?
        .ok_or(AuthRejection::Unrecognised)
        .map_err(refused)?;

    let params = request
        .extract_parts::<RawPathParams>()
        .await
        .map_err(|_| Problem::invalid_parameter("artifact_id", "missing"))?;
    let raw = params
        .iter()
        .find_map(|(name, value)| (name == "artifact_id").then(|| value.to_owned()))
        .ok_or_else(|| Problem::invalid_parameter("artifact_id", "missing"))?;
    let artifact_id = raw
        .parse::<ArtifactId>()
        .map_err(|_| Problem::invalid_parameter("artifact_id", "not a valid identifier"))?;

    if worker_holds_artifact(state.database().pool(), worker_id, artifact_id)
        .await
        .map_err(unavailable)?
    {
        Ok(())
    } else {
        // Not found rather than forbidden, as on the other worker routes:
        // whether the artifact exists is not the worker's business.
        tracing::warn!(%worker_id, %artifact_id, "a worker asked for an artifact it holds no lease on");
        Err(Problem::not_found("artifact", &artifact_id.to_string()))
    }
}

fn is_safe_method(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

fn csrf_matches(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(CSRF_HEADER)
        .is_some_and(|supplied| bool::from(supplied.as_bytes().ct_eq(expected.as_bytes())))
}

/// The session a request's cookie names, if it is live.
async fn current_session(
    state: &ApiState,
    headers: &HeaderMap,
) -> Result<Option<CurrentUser>, Problem> {
    let Some(token) = session_cookie(headers) else {
        return Ok(None);
    };
    if !token.has_prefix(SESSION_PREFIX) {
        return Ok(None);
    }
    let idle = i64::try_from(state.auth().idle_timeout.as_secs()).unwrap_or(i64::MAX);
    session_for_token(state.database().pool(), token.hash().as_str(), idle)
        .await
        .map(|session| session.map(CurrentUser::from))
        .map_err(|error| {
            tracing::error!(error = ?error, "could not look up a session");
            Problem::new(
                ErrorCode::StorageUnavailable,
                "sessions could not be checked; see server logs",
            )
        })
}

/// The session token from the request's cookies, if there is one.
fn session_cookie(headers: &HeaderMap) -> Option<Secret> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| {
            pair.trim()
                .strip_prefix(SESSION_COOKIE)
                .and_then(|rest| rest.strip_prefix('='))
                .filter(|token| !token.is_empty())
                .map(Secret::from_supplied)
        })
}

// --- issuing sessions ----------------------------------------------------------

/// A new session's secrets, before it is stored.
pub struct NewSession {
    /// The cookie's value. Shown to the browser once, never stored.
    pub token: Secret,
    /// What the database keeps instead of the token.
    pub token_hash: String,
    /// The anti-forgery token.
    pub csrf_token: String,
}

impl NewSession {
    /// Generate one.
    ///
    /// # Errors
    ///
    /// A problem if the operating system cannot supply randomness, in which
    /// case no session can safely be issued at all.
    pub fn generate() -> Result<Self, Problem> {
        let entropy = |_| {
            tracing::error!("the operating system could not supply randomness for a session");
            Problem::new(ErrorCode::Internal, "a session could not be started")
        };
        let token = Secret::generate(SESSION_PREFIX).map_err(entropy)?;
        let csrf = Secret::generate("").map_err(entropy)?;
        Ok(Self {
            token_hash: token.hash().as_str().to_owned(),
            csrf_token: csrf.expose().to_owned(),
            token,
        })
    }
}

/// The `Set-Cookie` value that hands a browser its session.
#[must_use]
pub fn session_cookie_value(settings: AuthSettings, token: &Secret) -> String {
    cookie(
        settings,
        token.expose(),
        settings.absolute_timeout.as_secs(),
    )
}

/// The `Set-Cookie` value that makes a browser forget its session.
#[must_use]
pub fn clear_cookie(settings: AuthSettings) -> String {
    cookie(settings, "", 0)
}

fn cookie(settings: AuthSettings, value: &str, max_age: u64) -> String {
    // Lax rather than Strict: Strict would make a link into Tangible from a
    // chat message arrive signed out. The anti-forgery token is what guards
    // mutations; this is a second layer.
    let secure = if settings.secure_cookie {
        "; Secure"
    } else {
        ""
    };
    format!("{SESSION_COOKIE}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}")
}

/// Attach a `Set-Cookie` header.
pub fn set_cookie(response: &mut Response, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
}

// --- passwords -----------------------------------------------------------------

/// Hash a new password for storage.
///
/// # Errors
///
/// A problem if hashing fails, which it should not.
pub async fn hash_password(password: String) -> Result<String, Problem> {
    let _permit = password_permit().await?;
    tokio::task::spawn_blocking(move || {
        Argon2::default()
            .hash_password(password.as_bytes())
            .map(|hash| hash.to_string())
    })
    .await
    .map_err(|_| hashing_failed())?
    .map_err(|error| {
        tracing::error!(error = %error, "could not hash a password");
        hashing_failed()
    })
}

/// Check a password against a stored hash, or against a stand-in when there
/// is no account, so both take the same time.
///
/// # Errors
///
/// A problem if the check cannot run at all.
pub async fn verify_password(password: String, stored: Option<String>) -> Result<bool, Problem> {
    let _permit = password_permit().await?;
    tokio::task::spawn_blocking(move || {
        let hash = stored.clone().unwrap_or_else(stand_in_hash);
        let matches = PasswordHash::new(&hash).is_ok_and(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        });
        matches && stored.is_some()
    })
    .await
    .map_err(|_| hashing_failed())
}

/// A hash of nothing anyone knows, for checking a password against when the
/// username does not exist.
fn stand_in_hash() -> String {
    static STAND_IN: OnceLock<String> = OnceLock::new();
    STAND_IN
        .get_or_init(|| {
            let mut unguessable = [0_u8; 32];
            // Should randomness fail, a fixed input still costs the same to
            // check, and no password can match anyway: the caller requires a
            // stored hash for success.
            let _ = getrandom::fill(&mut unguessable);
            Argon2::default()
                .hash_password(&unguessable)
                .map(|hash| hash.to_string())
                .unwrap_or_default()
        })
        .clone()
}

async fn password_permit() -> Result<tokio::sync::SemaphorePermit<'static>, Problem> {
    static CHECKS: tokio::sync::Semaphore =
        tokio::sync::Semaphore::const_new(CONCURRENT_PASSWORD_CHECKS);
    CHECKS.acquire().await.map_err(|_| hashing_failed())
}

fn hashing_failed() -> Problem {
    Problem::new(ErrorCode::Internal, "the password could not be checked")
}

// --- rate limiting -------------------------------------------------------------

/// Remembers failed sign-ins per username.
///
/// In memory: the server runs as one instance, and a restart forgetting a
/// few failures costs an attacker a restart they cannot cause.
#[derive(Debug, Clone, Default)]
pub struct LoginLimiter {
    failures: Arc<Mutex<HashMap<String, (u32, Instant)>>>,
}

impl LoginLimiter {
    /// How long until `username` may try again, if it is locked out.
    #[must_use]
    pub fn retry_after(&self, username: &str) -> Option<Duration> {
        let failures = self.failures.lock().ok()?;
        let (count, since) = failures.get(username)?;
        let elapsed = since.elapsed();
        (*count >= LOGIN_FAILURES_ALLOWED && elapsed < LOGIN_WINDOW)
            .then(|| LOGIN_WINDOW.saturating_sub(elapsed))
    }

    /// Count a failure.
    pub fn record_failure(&self, username: &str) {
        let Ok(mut failures) = self.failures.lock() else {
            return;
        };
        if failures.len() >= LOGIN_TRACKED_NAMES {
            failures.retain(|_, (_, since)| since.elapsed() < LOGIN_WINDOW);
        }
        if failures.len() >= LOGIN_TRACKED_NAMES && !failures.contains_key(username) {
            // Still full of live entries: somebody is spraying names. Drop
            // the oldest rather than grow without bound.
            if let Some(oldest) = failures
                .iter()
                .min_by_key(|(_, (_, since))| *since)
                .map(|(name, _)| name.clone())
            {
                failures.remove(&oldest);
            }
        }
        let entry = failures
            .entry(username.to_owned())
            .or_insert((0, Instant::now()));
        if entry.1.elapsed() >= LOGIN_WINDOW {
            *entry = (0, Instant::now());
        }
        entry.0 = entry.0.saturating_add(1);
    }

    /// Forget a username's failures after it signs in.
    pub fn record_success(&self, username: &str) {
        if let Ok(mut failures) = self.failures.lock() {
            failures.remove(username);
        }
    }
}

// --- response headers ----------------------------------------------------------

/// Headers every response carries.
///
/// The API serves JSON and file bytes, never a page, so its policy is the
/// strictest there is: nothing may be loaded, framed, or sniffed into
/// something else. A component download a browser rendered as HTML would
/// otherwise be script from an imported file running on this origin.
///
/// The web UI's files get the policy the UI's build declares, which allows
/// its own scripts by hash and nothing else, and caching suited to them:
/// hashed assets forever, the page itself revalidated every time so a new
/// release is picked up.
pub async fn security_headers(
    State(state): State<ApiState>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_owned();
    let mut response = next.run(request).await;
    let ui = state.web().filter(|_| !crate::web::is_api_path(&path));

    let (csp, cache) = match ui {
        Some(web) => (
            HeaderValue::from_str(web.csp()).ok(),
            if crate::web::is_immutable_asset(&path) {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            },
        ),
        // Library data is per account; no shared cache should keep it.
        None => (
            Some(HeaderValue::from_static(
                "default-src 'none'; frame-ancestors 'none'",
            )),
            "no-store",
        ),
    };

    let headers = response.headers_mut();
    if let Some(csp) = csp
        && !headers.contains_key(header::CONTENT_SECURITY_POLICY)
    {
        headers.insert(header::CONTENT_SECURITY_POLICY, csp);
    }
    for (name, value) in [
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (
            header::HeaderName::from_static("cross-origin-resource-policy"),
            "same-origin",
        ),
        (header::CACHE_CONTROL, cache),
    ] {
        if !headers.contains_key(&name) {
            headers.insert(name, HeaderValue::from_static(value));
        }
    }
    response
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_http_public_url_drops_the_secure_flag_and_nothing_else_does() {
        assert!(!AuthSettings::for_public_url("http://tangible.lan:8080").secure_cookie);
        assert!(!AuthSettings::for_public_url("HTTP://10.0.0.5").secure_cookie);
        assert!(AuthSettings::for_public_url("https://tangible.example").secure_cookie);
        // Anything unrecognised gets the safe answer.
        assert!(AuthSettings::for_public_url("tangible.lan").secure_cookie);
        assert!(AuthSettings::for_public_url("").secure_cookie);
    }

    #[test]
    fn the_cookie_is_http_only_lax_and_secure_when_asked() {
        let token = Secret::from_supplied("tgs_abc");
        let secure = session_cookie_value(AuthSettings::default(), &token);
        assert!(secure.starts_with("tangible_session=tgs_abc;"));
        for part in [
            "HttpOnly",
            "SameSite=Lax",
            "Path=/",
            "Secure",
            "Max-Age=604800",
        ] {
            assert!(secure.contains(part), "{secure} lacks {part}");
        }
        let plain = session_cookie_value(
            AuthSettings {
                secure_cookie: false,
                ..AuthSettings::default()
            },
            &token,
        );
        assert!(!plain.contains("Secure"));
        assert!(clear_cookie(AuthSettings::default()).contains("Max-Age=0"));
    }

    #[test]
    fn the_session_cookie_is_found_among_others() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("theme=dark; tangible_session=tgs_xyz; other=1"),
        );
        assert_eq!(
            session_cookie(&headers).map(|token| token.expose().to_owned()),
            Some("tgs_xyz".to_owned())
        );

        let mut lookalike = HeaderMap::new();
        lookalike.insert(
            header::COOKIE,
            HeaderValue::from_static("tangible_session_old=tgs_xyz; tangible_session="),
        );
        assert!(session_cookie(&lookalike).is_none());
    }

    #[test]
    fn new_sessions_are_unpredictable_and_fit_the_columns() {
        let one = NewSession::generate().expect("entropy");
        let two = NewSession::generate().expect("entropy");
        assert_ne!(one.token.expose(), two.token.expose());
        assert_ne!(one.csrf_token, two.csrf_token);
        assert!(one.token.has_prefix(SESSION_PREFIX));
        assert_eq!(one.token_hash.len(), 64);
        assert_ne!(one.token_hash, one.token.expose());
        assert!((32..=128).contains(&one.csrf_token.len()));
    }

    #[test]
    fn the_csrf_header_must_match_exactly() {
        let mut headers = HeaderMap::new();
        assert!(!csrf_matches(&headers, "token"));
        headers.insert(CSRF_HEADER, HeaderValue::from_static("toke"));
        assert!(!csrf_matches(&headers, "token"));
        headers.insert(CSRF_HEADER, HeaderValue::from_static("token"));
        assert!(csrf_matches(&headers, "token"));
    }

    #[test]
    fn rules_are_found_by_method_and_pattern() {
        assert_eq!(
            rule_for(&Method::GET, "/api/v1/burn-attempts/{attempt_id}/events"),
            Ok(Signed(Read))
        );
        assert_eq!(
            rule_for(&Method::POST, "/api/v1/burn-attempts/{attempt_id}/events"),
            Ok(Worker)
        );
        assert_eq!(rule_for(&Method::HEAD, "/livez"), Ok(Public));
        // A known path with an unrouted method: the router's 405.
        assert_eq!(rule_for(&Method::DELETE, "/api/v1/burn-jobs"), Err(true));
        // An unknown path: a missing rule.
        assert_eq!(rule_for(&Method::GET, "/api/v1/secrets"), Err(false));
    }

    #[test]
    fn no_rule_is_listed_twice() {
        let mut seen = std::collections::BTreeSet::new();
        for (method, path, _) in ACCESS_RULES {
            assert!(seen.insert((method, path)), "{method} {path} listed twice");
        }
    }

    #[test]
    fn every_documented_operation_has_a_rule_and_every_rule_is_documented() {
        use utoipa::OpenApi;
        let document = serde_json::to_value(crate::ApiDoc::openapi()).expect("serialize");
        let mut documented = std::collections::BTreeSet::new();
        for (path, item) in document["paths"].as_object().expect("paths") {
            for method in item.as_object().expect("operations").keys() {
                documented.insert((method.to_uppercase(), path.clone()));
            }
        }
        let ruled: std::collections::BTreeSet<_> = ACCESS_RULES
            .iter()
            .map(|(method, path, _)| ((*method).to_owned(), (*path).to_owned()))
            .collect();
        let unruled: Vec<_> = documented.difference(&ruled).collect();
        let undocumented: Vec<_> = ruled.difference(&documented).collect();
        assert!(
            unruled.is_empty(),
            "operations with no access rule: {unruled:?}"
        );
        assert!(
            undocumented.is_empty(),
            "rules for operations the document does not have: {undocumented:?}"
        );
    }

    #[test]
    fn mutations_need_more_than_read() {
        for (method, path, access) in ACCESS_RULES {
            if *method != "GET" && *access == Signed(Read) {
                assert_eq!(
                    (*method, *path),
                    ("DELETE", "/api/v1/session"),
                    "{method} {path} lets a viewer change something"
                );
            }
        }
    }

    #[test]
    fn the_limiter_locks_a_name_after_five_failures_and_forgets_on_success() {
        let limiter = LoginLimiter::default();
        for _ in 0..4 {
            limiter.record_failure("alice");
        }
        assert!(limiter.retry_after("alice").is_none());
        limiter.record_failure("alice");
        let wait = limiter.retry_after("alice").expect("locked");
        assert!(wait <= LOGIN_WINDOW);
        assert!(
            limiter.retry_after("bob").is_none(),
            "other names unaffected"
        );
        limiter.record_success("alice");
        assert!(limiter.retry_after("alice").is_none());
    }

    #[test]
    fn the_limiter_stays_bounded() {
        let limiter = LoginLimiter::default();
        for n in 0..(LOGIN_TRACKED_NAMES + 50) {
            limiter.record_failure(&format!("user{n}"));
        }
        assert!(limiter.failures.lock().unwrap().len() <= LOGIN_TRACKED_NAMES);
    }

    #[tokio::test]
    async fn a_password_verifies_against_its_own_hash_only() {
        let hash = hash_password("correct horse battery".to_owned())
            .await
            .expect("hash");
        assert!(hash.starts_with("$argon2id$"), "{hash}");
        assert!(
            verify_password("correct horse battery".to_owned(), Some(hash.clone()))
                .await
                .expect("verify")
        );
        assert!(
            !verify_password("correct horse batterz".to_owned(), Some(hash))
                .await
                .expect("verify")
        );
    }

    #[tokio::test]
    async fn no_password_verifies_without_an_account() {
        assert!(!verify_password(String::new(), None).await.expect("verify"));
        assert!(
            !verify_password("anything".to_owned(), None)
                .await
                .expect("verify")
        );
    }

    #[tokio::test]
    async fn a_malformed_stored_hash_verifies_nothing() {
        assert!(
            !verify_password("x".to_owned(), Some("$argon2id$garbage".to_owned()))
                .await
                .expect("verify")
        );
    }
}
