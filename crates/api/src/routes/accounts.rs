// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Setting up, signing in and out, and managing accounts.
//!
//! The first account is made on a setup page the first visitor sees, and it
//! is an administrator. Setup works exactly once: the moment any account
//! exists, it refuses, so a server left on the setup page cannot be claimed
//! by a second visitor after the owner has finished.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use tangible_db::accounts::{
    CreateUserOutcome, UserRecord, count_users, create_first_administrator, create_session,
    create_user, list_users, record_failed_sign_in, revoke_session, user_for_sign_in,
};
use tangible_domain::Role;
use tangible_domain::auth::{MAX_PASSWORD_BYTES, check_new_password, normalize_username};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use utoipa::ToSchema;

use crate::auth::{
    CurrentUser, NewSession, SignedIn, clear_cookie, hash_password, session_cookie_value,
    set_cookie, verify_password,
};
use crate::problem::{ErrorCode, Problem};
use crate::state::ApiState;

fn rfc3339(value: OffsetDateTime) -> String {
    value.format(&Rfc3339).unwrap_or_default()
}

fn unavailable(what: &str) -> Problem {
    Problem::new(
        ErrorCode::StorageUnavailable,
        format!("{what}; see server logs"),
    )
}

fn invalid(error: &tangible_domain::auth::AccountError) -> Problem {
    Problem::new(ErrorCode::ValidationFailed, error.to_string())
}

// --- views ---------------------------------------------------------------------

/// Whether the server still needs its first account.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SetupStatus {
    /// True until the first administrator exists.
    pub needed: bool,
}

/// An account, without anything that would sign anybody in.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UserView {
    /// Opaque identifier.
    pub id: String,
    /// The username, lowercase.
    pub username: String,
    /// `viewer`, `operator` or `administrator`.
    pub role: String,
    /// Whether the account has been disabled.
    pub disabled: bool,
    /// When it last signed in.
    pub last_signed_in_at: Option<String>,
    /// When it was created.
    pub created_at: String,
}

impl From<UserRecord> for UserView {
    fn from(user: UserRecord) -> Self {
        Self {
            id: user.id.to_string(),
            username: user.username,
            role: user.role.to_string(),
            disabled: user.disabled,
            last_signed_in_at: user.last_signed_in_at.map(rfc3339),
            created_at: rfc3339(user.created_at),
        }
    }
}

/// The signed-in session.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SessionView {
    /// Who is signed in.
    pub username: String,
    /// What they may do.
    pub role: String,
    /// The token every mutation must send in `X-CSRF-Token`.
    ///
    /// Not a credential on its own: it is useless without the session
    /// cookie, which a page's script cannot read.
    pub csrf_token: String,
    /// When the session ends however much it is used.
    pub expires_at: String,
}

impl From<&CurrentUser> for SessionView {
    fn from(user: &CurrentUser) -> Self {
        Self {
            username: user.username.clone(),
            role: user.role.to_string(),
            csrf_token: user.csrf_token.clone(),
            expires_at: rfc3339(user.expires_at),
        }
    }
}

/// A list of accounts.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UserList {
    /// Every account, oldest first.
    pub items: Vec<UserView>,
}

// --- requests ------------------------------------------------------------------

/// A username and password.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    /// The username. Case does not matter.
    pub username: String,
    /// The password.
    pub password: String,
}

/// An account to create.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateUserRequest {
    /// 1 to 64 lowercase letters, digits, `.`, `_` or `-`.
    pub username: String,
    /// At least 12 characters.
    pub password: String,
    /// `viewer`, `operator` or `administrator`.
    pub role: String,
}

// --- signing in ----------------------------------------------------------------

/// Start a session for `user` and build the response that hands it over.
async fn start_session(
    state: &ApiState,
    user: &UserRecord,
    status: StatusCode,
) -> Result<Response, Problem> {
    let session = NewSession::generate()?;
    let settings = state.auth();
    let lifetime =
        time::Duration::try_from(settings.absolute_timeout).unwrap_or(time::Duration::days(7));
    let expires_at = OffsetDateTime::now_utc() + lifetime;
    let session_id = create_session(
        state.database().pool(),
        user,
        &session.token_hash,
        &session.csrf_token,
        expires_at,
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not start a session");
        unavailable("the session could not be started")
    })?;
    tracing::info!(user = %user.username, %session_id, "signed in");

    let view = SessionView {
        username: user.username.clone(),
        role: user.role.to_string(),
        csrf_token: session.csrf_token,
        expires_at: rfc3339(expires_at),
    };
    let mut response = (status, Json(view)).into_response();
    set_cookie(
        &mut response,
        &session_cookie_value(settings, &session.token),
    );
    Ok(response)
}

/// Whether the server still needs its first account.
///
/// # Errors
///
/// `STORAGE_UNAVAILABLE` if accounts cannot be counted.
#[utoipa::path(
    get,
    path = "/api/v1/setup",
    tag = "accounts",
    responses((status = 200, description = "Whether setup is needed", body = SetupStatus)),
)]
pub async fn setup_status(State(state): State<ApiState>) -> Result<Json<SetupStatus>, Problem> {
    let users = count_users(state.database().pool())
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not count accounts");
            unavailable("accounts could not be read")
        })?;
    Ok(Json(SetupStatus { needed: users == 0 }))
}

/// Create the first administrator and sign them in.
///
/// # Errors
///
/// `CONFLICT` once any account exists; `VALIDATION_FAILED` for a username or
/// password that will not do.
#[utoipa::path(
    post,
    path = "/api/v1/setup",
    tag = "accounts",
    description = "Create the first account, an administrator, and sign it in. \
                   Works only while no account exists.",
    request_body = Credentials,
    responses(
        (status = 201, description = "Created and signed in", body = SessionView),
        (status = 409, description = "Setup is already complete", body = Problem),
        (status = 422, description = "The username or password will not do", body = Problem),
    ),
)]
pub async fn complete_setup(
    State(state): State<ApiState>,
    Json(request): Json<Credentials>,
) -> Result<Response, Problem> {
    let username = normalize_username(&request.username).map_err(|error| invalid(&error))?;
    check_new_password(&request.password).map_err(|error| invalid(&error))?;

    // Checked before hashing so a finished server does not spend a hash on
    // every visit to its setup route. The insert checks again under a lock.
    let already = Problem::new(ErrorCode::Conflict, "setup is already complete; sign in");
    if count_users(state.database().pool())
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not count accounts");
            unavailable("accounts could not be read")
        })?
        > 0
    {
        return Err(already);
    }

    let hash = hash_password(request.password).await?;
    let user = create_first_administrator(state.database().pool(), &username, &hash)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not create the first account");
            unavailable("the account could not be created")
        })?
        .ok_or(already)?;
    tracing::warn!(user = %user.username, "first administrator created; setup is closed");
    start_session(&state, &user, StatusCode::CREATED).await
}

/// Sign in.
///
/// # Errors
///
/// `UNAUTHENTICATED` for a wrong username or password, without saying
/// which; `RATE_LIMITED` after repeated failures for one username.
#[utoipa::path(
    post,
    path = "/api/v1/session",
    tag = "accounts",
    description = "Sign in with a username and password. Sets the session cookie \
                   and returns the anti-forgery token mutations must send.",
    request_body = Credentials,
    responses(
        (status = 200, description = "Signed in", body = SessionView),
        (status = 401, description = "The username or password is wrong", body = Problem),
        (status = 429, description = "Too many failures for this username", body = Problem),
    ),
)]
pub async fn sign_in(
    State(state): State<ApiState>,
    Json(request): Json<Credentials>,
) -> Result<Response, Problem> {
    let limiter = state.login_limiter();
    // The name as the limiter and the audit log see it. An invalid name is
    // still counted, under what was typed, so it cannot dodge the limit.
    let name = normalize_username(&request.username).unwrap_or_else(|_| {
        request
            .username
            .trim()
            .to_ascii_lowercase()
            .chars()
            .take(64)
            .collect()
    });

    if let Some(wait) = limiter.retry_after(&name) {
        let mut response = Problem::new(
            ErrorCode::RateLimited,
            "too many failed sign-ins for this username; try again later",
        )
        .into_response();
        if let Ok(value) = axum::http::HeaderValue::from_str(&wait.as_secs().max(1).to_string()) {
            response
                .headers_mut()
                .insert(axum::http::header::RETRY_AFTER, value);
        }
        return Ok(response);
    }

    let account = if request.password.len() > MAX_PASSWORD_BYTES {
        None
    } else {
        user_for_sign_in(state.database().pool(), &name)
            .await
            .map_err(|error| {
                tracing::error!(error = ?error, "could not look up an account");
                unavailable("accounts could not be read")
            })?
    };
    let (user, stored) = match account {
        Some((user, hash)) => (Some(user), Some(hash)),
        None => (None, None),
    };
    let password_ok = verify_password(request.password, stored).await?;

    match user {
        Some(user) if password_ok && !user.disabled => {
            limiter.record_success(&name);
            start_session(&state, &user, StatusCode::OK).await
        }
        _ => {
            limiter.record_failure(&name);
            tracing::warn!(username = %name, "sign-in refused");
            if let Err(error) = record_failed_sign_in(state.database().pool(), &name).await {
                tracing::error!(error = ?error, "could not audit a refused sign-in");
            }
            Err(Problem::new(
                ErrorCode::Unauthenticated,
                "the username or password is incorrect",
            ))
        }
    }
}

/// The signed-in session.
#[utoipa::path(
    get,
    path = "/api/v1/session",
    tag = "accounts",
    responses(
        (status = 200, description = "The session", body = SessionView),
        (status = 401, description = "Not signed in", body = Problem),
    ),
)]
pub async fn current_session(SignedIn(user): SignedIn) -> Json<SessionView> {
    Json(SessionView::from(&user))
}

/// Sign out.
///
/// # Errors
///
/// `STORAGE_UNAVAILABLE` if the session cannot be ended.
#[utoipa::path(
    delete,
    path = "/api/v1/session",
    tag = "accounts",
    responses(
        (status = 204, description = "Signed out"),
        (status = 401, description = "Not signed in", body = Problem),
    ),
)]
pub async fn sign_out(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
) -> Result<Response, Problem> {
    revoke_session(state.database().pool(), user.session_id, &user.username)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not end a session");
            unavailable("the session could not be ended")
        })?;
    tracing::info!(user = %user.username, session_id = %user.session_id, "signed out");
    let mut response = StatusCode::NO_CONTENT.into_response();
    set_cookie(&mut response, &clear_cookie(state.auth()));
    Ok(response)
}

// --- accounts ------------------------------------------------------------------

/// Every account.
///
/// # Errors
///
/// `STORAGE_UNAVAILABLE` if accounts cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/users",
    tag = "accounts",
    responses(
        (status = 200, description = "Every account", body = UserList),
        (status = 403, description = "Not an administrator", body = Problem),
    ),
)]
pub async fn list_accounts(State(state): State<ApiState>) -> Result<Json<UserList>, Problem> {
    let users = list_users(state.database().pool()).await.map_err(|error| {
        tracing::error!(error = ?error, "could not list accounts");
        unavailable("accounts could not be read")
    })?;
    Ok(Json(UserList {
        items: users.into_iter().map(UserView::from).collect(),
    }))
}

/// Create an account.
///
/// # Errors
///
/// `VALIDATION_FAILED` for a username, password or role that will not do;
/// `CONFLICT` for a username already in use.
#[utoipa::path(
    post,
    path = "/api/v1/users",
    tag = "accounts",
    request_body = CreateUserRequest,
    responses(
        (status = 201, description = "Created", body = UserView),
        (status = 403, description = "Not an administrator", body = Problem),
        (status = 409, description = "The username is taken", body = Problem),
        (status = 422, description = "The username, password or role will not do", body = Problem),
    ),
)]
pub async fn create_account(
    State(state): State<ApiState>,
    SignedIn(actor): SignedIn,
    Json(request): Json<CreateUserRequest>,
) -> Result<(StatusCode, Json<UserView>), Problem> {
    let username = normalize_username(&request.username).map_err(|error| invalid(&error))?;
    check_new_password(&request.password).map_err(|error| invalid(&error))?;
    let role = Role::from_str(&request.role).map_err(|_| {
        Problem::new(
            ErrorCode::ValidationFailed,
            "role must be viewer, operator or administrator",
        )
    })?;

    let hash = hash_password(request.password).await?;
    let user = create_user(
        state.database().pool(),
        &username,
        &hash,
        role,
        &actor.username,
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not create an account");
        unavailable("the account could not be created")
    })?
    .map_err(|CreateUserOutcome::NameTaken| {
        Problem::new(ErrorCode::Conflict, "that username is already in use")
    })?;
    tracing::info!(user = %user.username, role = %user.role, by = %actor.username, "account created");
    Ok((StatusCode::CREATED, Json(UserView::from(user))))
}

/// The account routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/setup", get(setup_status).post(complete_setup))
        .route(
            "/session",
            get(current_session).post(sign_in).delete(sign_out),
        )
        .route("/users", get(list_accounts).post(create_account))
}
