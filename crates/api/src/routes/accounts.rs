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

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use tangible_db::accounts::{
    AccountChange, AccountDeletion, CreateUserOutcome, UserRecord, count_users,
    create_first_administrator, create_session, create_user, delete_unused_user, list_users,
    made_by_setup, record_failed_sign_in, revoke_session, set_user_disabled, user_for_sign_in,
};
use tangible_domain::auth::{MAX_PASSWORD_BYTES, check_new_password, normalize_username};
use tangible_domain::{Role, SecretString};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use utoipa::ToSchema;

use crate::auth::{
    CurrentUser, NewSession, PasswordSignIn, SignedIn, clear_cookie, hash_password,
    session_cookie_value, set_cookie, verify_password,
};
use crate::pagination::{Page, PageQuery};
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
    /// Whether it may be deleted: it has never signed in. One that has can
    /// only be disabled, since it names the person behind what it did.
    pub can_delete: bool,
    /// How it signs in: `password`; `provider` for an account an identity
    /// provider vouches for (ADR 0018); or `none` for a password account
    /// that cannot sign in at all, because this server lets only the account
    /// setup made use a password (`TANGIBLE_PASSWORD_SIGN_IN=setup-only`).
    pub signs_in_with: String,
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
            can_delete: user.last_signed_in_at.is_none(),
            signs_in_with: "password".to_owned(),
        }
    }
}

/// The signed-in session.
// No `Debug`: it carries the anti-forgery token, and nothing needs to print it.
#[derive(Clone, Serialize, ToSchema)]
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

/// A page of accounts.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UserList {
    /// The accounts in this page, oldest first.
    pub items: Vec<UserView>,
    /// Cursor for the next page, or null when this is the last.
    pub next_cursor: Option<String>,
}

impl From<Page<UserView>> for UserList {
    fn from(page: Page<UserView>) -> Self {
        Self {
            items: page.items,
            next_cursor: page.next_cursor,
        }
    }
}

// --- requests ------------------------------------------------------------------

/// A username and password.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    /// The username. Case does not matter.
    pub username: String,
    /// The password.
    #[schema(value_type = String, format = Password)]
    pub password: SecretString,
}

/// An account to create.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateUserRequest {
    /// 1 to 64 lowercase letters, digits, `.`, `_` or `-`.
    pub username: String,
    /// At least 12 characters.
    #[schema(value_type = String, format = Password)]
    pub password: SecretString,
    /// `viewer`, `operator` or `administrator`.
    pub role: String,
}

// --- signing in ----------------------------------------------------------------

/// Start a session for `user` and build the response that hands it over.
/// Start a session for `user`, who signed in by `method` (`password`,
/// `setup` or `oidc`): the cookie to set, and what the session is.
pub(crate) async fn issue_session(
    state: &ApiState,
    user: &UserRecord,
    method: &str,
) -> Result<(String, SessionView), Problem> {
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
        method,
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not start a session");
        unavailable("the session could not be started")
    })?;
    tracing::info!(user = %user.username, %session_id, method, "signed in");

    let view = SessionView {
        username: user.username.clone(),
        role: user.role.to_string(),
        csrf_token: session.csrf_token,
        expires_at: rfc3339(expires_at),
    };
    Ok((session_cookie_value(settings, &session.token), view))
}

async fn start_session(
    state: &ApiState,
    user: &UserRecord,
    status: StatusCode,
    method: &str,
) -> Result<Response, Problem> {
    let (cookie, view) = issue_session(state, user, method).await?;
    let mut response = (status, Json(view)).into_response();
    set_cookie(&mut response, &cookie);
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
    check_new_password(request.password.expose()).map_err(|error| invalid(&error))?;

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
    start_session(&state, &user, StatusCode::CREATED, "setup").await
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

    let account = if request.password.expose().len() > MAX_PASSWORD_BYTES {
        None
    } else {
        user_for_sign_in(state.database().pool(), &name)
            .await
            .map_err(|error| {
                tracing::error!(error = ?error, "could not look up an account");
                unavailable("accounts could not be read")
            })?
    };
    // An account with no password (one a provider vouches for) checks
    // against nothing, as an unknown name does, and costs the same time.
    let (user, stored, made_by_setup) = match account {
        Some(record) => (
            Some(record.user),
            record.password_hash,
            record.made_by_setup,
        ),
        None => (None, None, false),
    };
    let password_ok = verify_password(request.password, stored).await?;

    match user {
        Some(user) if password_ok && !user.disabled => {
            limiter.record_success(&name);
            // Checked only once the password is right, so the answer says
            // nothing about an account to someone who does not know it.
            if state.auth().password_sign_in == PasswordSignIn::SetupOnly && !made_by_setup {
                tracing::info!(username = %name, "password sign-in refused: provider accounts only");
                return Err(Problem::new(
                    ErrorCode::PasswordSignInOff,
                    "this account signs in through the identity provider",
                ));
            }
            start_session(&state, &user, StatusCode::OK, "password").await
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

/// A page of accounts, oldest first.
///
/// # Errors
///
/// `INVALID_CURSOR` for a cursor this server did not issue, or
/// `STORAGE_UNAVAILABLE` if accounts cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/users",
    tag = "accounts",
    description = "List accounts, oldest first. Use `next_cursor` from the response to \
                   fetch the following page.",
    params(PageQuery),
    responses(
        (status = 200, description = "A page of accounts", body = UserList),
        (status = 400, description = "Invalid cursor", body = Problem),
        (status = 403, description = "Not an administrator", body = Problem),
    ),
)]
pub async fn list_accounts(
    State(state): State<ApiState>,
    Query(query): Query<PageQuery>,
) -> Result<Json<UserList>, Problem> {
    let limit = query.limit();
    let after = query
        .after()?
        .map(|after| {
            uuid::Uuid::parse_str(&after)
                .map_err(|_| Problem::new(ErrorCode::InvalidCursor, "the cursor is not usable"))
        })
        .transpose()?;
    let users = list_users(
        state.database().pool(),
        i64::try_from(limit).unwrap_or(i64::MAX),
        after,
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not list accounts");
        unavailable("accounts could not be read")
    })?;
    let items = account_views(&state, users).await?;
    Ok(Json(Page::new(items, limit, |user| user.id.clone()).into()))
}

/// `users` as the list shows them, each saying how it signs in.
async fn account_views(state: &ApiState, users: Vec<UserRecord>) -> Result<Vec<UserView>, Problem> {
    let ids: Vec<uuid::Uuid> = users.iter().map(|user| user.id).collect();
    let read_failed = |error: tangible_db::DbError| {
        tracing::error!(error = ?error, "could not read how accounts sign in");
        unavailable("accounts could not be read")
    };
    let through_provider = tangible_db::oidc::identity_users(state.database().pool(), &ids)
        .await
        .map_err(read_failed)?;
    // Where passwords are for setup's account alone, any other password
    // account cannot sign in at all, and the list says so.
    let password_allowed = match state.auth().password_sign_in {
        PasswordSignIn::All => None,
        PasswordSignIn::SetupOnly => Some(
            made_by_setup(state.database().pool(), &ids)
                .await
                .map_err(read_failed)?,
        ),
    };
    Ok(users
        .into_iter()
        .map(|user| {
            let signs_in_with = if through_provider.contains(&user.id) {
                "provider"
            } else if password_allowed
                .as_ref()
                .is_some_and(|allowed| !allowed.contains(&user.id))
            {
                "none"
            } else {
                "password"
            };
            let mut view = UserView::from(user);
            signs_in_with.clone_into(&mut view.signs_in_with);
            view
        })
        .collect())
}

fn account_id(raw: &str) -> Result<uuid::Uuid, Problem> {
    uuid::Uuid::parse_str(raw)
        .map_err(|_| Problem::invalid_parameter("user_id", "not a valid identifier"))
}

/// Disable or enable `raw` as `actor`, answering the account as it now is.
async fn set_disabled(
    state: &ApiState,
    actor: &CurrentUser,
    raw: &str,
    disabled: bool,
) -> Result<Json<UserView>, Problem> {
    let id = account_id(raw)?;
    if disabled && id == actor.user_id {
        return Err(Problem::new(
            ErrorCode::Conflict,
            "your own account cannot be disabled; another administrator can disable it",
        ));
    }
    let change = set_user_disabled(state.database().pool(), id, disabled, &actor.username)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not change an account");
            unavailable("the account could not be changed")
        })?;
    match change {
        AccountChange::Done(user) => {
            tracing::info!(
                user = %user.username,
                disabled,
                by = %actor.username,
                "account changed"
            );
            let mut views = account_views(state, vec![user]).await?;
            views
                .pop()
                .map(Json)
                .ok_or_else(|| unavailable("the account could not be read"))
        }
        AccountChange::NoSuchAccount => Err(Problem::not_found("account", raw)),
        AccountChange::LastAdministrator => Err(Problem::new(
            ErrorCode::LastAdministrator,
            "it is the only enabled administrator, and nobody could manage accounts or workers \
             without one: make or enable another administrator first",
        )),
    }
}

/// Delete an account that has never been used.
///
/// # Errors
///
/// `NOT_FOUND`; `CONFLICT` for your own account; `ACCOUNT_IN_USE`;
/// `LAST_ADMINISTRATOR`.
#[utoipa::path(
    delete,
    path = "/api/v1/users/{user_id}",
    tag = "accounts",
    description = "Delete an account that has never been used: it never signed in, and has no \
                   session, request or identity provider's link (`can_delete` says so). It goes \
                   entirely, and its name may be given again; the audit log keeps `user.deleted` \
                   with its username. An account that has been used names the person behind \
                   what it did, and is disabled instead. Audited.",
    params(("user_id" = String, Path, description = "Account identifier")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, description = "No such account", body = Problem),
        (status = 409, description = "Your own account, one that has been used, or the only \
                                      enabled administrator", body = Problem),
    ),
)]
pub async fn delete_account(
    State(state): State<ApiState>,
    SignedIn(actor): SignedIn,
    Path(raw): Path<String>,
) -> Result<StatusCode, Problem> {
    let id = account_id(&raw)?;
    if id == actor.user_id {
        return Err(Problem::new(
            ErrorCode::Conflict,
            "your own account cannot be deleted",
        ));
    }
    let deletion = delete_unused_user(state.database().pool(), id, &actor.username)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not delete an account");
            unavailable("the account could not be deleted")
        })?;
    match deletion {
        AccountDeletion::Deleted(user) => {
            tracing::info!(user = %user.username, by = %actor.username, "account deleted");
            Ok(StatusCode::NO_CONTENT)
        }
        AccountDeletion::NoSuchAccount => Err(Problem::not_found("account", &raw)),
        AccountDeletion::InUse => Err(Problem::new(
            ErrorCode::AccountInUse,
            "it has been used (it signed in, or asked for something), so it names someone in \
             the audit log and on what they did: disable it instead",
        )),
        AccountDeletion::LastAdministrator => Err(Problem::new(
            ErrorCode::LastAdministrator,
            "it is the only enabled administrator: make or enable another administrator first",
        )),
    }
}

/// Disable an account.
///
/// # Errors
///
/// `NOT_FOUND`; `CONFLICT` for your own account; `LAST_ADMINISTRATOR`.
#[utoipa::path(
    post,
    path = "/api/v1/users/{user_id}/disable",
    tag = "accounts",
    description = "Disable an account: its sessions end, and it cannot sign in, by password or \
                   through the identity provider, until it is enabled again. What it did stays \
                   in the audit log and on the requests and burns that name it. Not your own \
                   account, and not the only enabled administrator. Audited.",
    params(("user_id" = String, Path, description = "Account identifier")),
    responses(
        (status = 200, description = "Disabled", body = UserView),
        (status = 404, description = "No such account", body = Problem),
        (status = 409, description = "Your own account, or the only enabled administrator",
         body = Problem),
    ),
)]
pub async fn disable_account(
    State(state): State<ApiState>,
    SignedIn(actor): SignedIn,
    Path(raw): Path<String>,
) -> Result<Json<UserView>, Problem> {
    set_disabled(&state, &actor, &raw, true).await
}

/// Enable an account again.
///
/// # Errors
///
/// `NOT_FOUND`.
#[utoipa::path(
    post,
    path = "/api/v1/users/{user_id}/enable",
    tag = "accounts",
    description = "Enable a disabled account: it may sign in again as it did before. Audited.",
    params(("user_id" = String, Path, description = "Account identifier")),
    responses(
        (status = 200, description = "Enabled", body = UserView),
        (status = 404, description = "No such account", body = Problem),
    ),
)]
pub async fn enable_account(
    State(state): State<ApiState>,
    SignedIn(actor): SignedIn,
    Path(raw): Path<String>,
) -> Result<Json<UserView>, Problem> {
    set_disabled(&state, &actor, &raw, false).await
}

/// Create an account.
///
/// # Errors
///
/// `PASSWORD_SIGN_IN_OFF` where only setup's account may use a password;
/// `VALIDATION_FAILED` for a username, password or role that will not do;
/// `CONFLICT` for a username already in use.
#[utoipa::path(
    post,
    path = "/api/v1/users",
    tag = "accounts",
    description = "Create an account that signs in with a password. Refused where only the \
                   account setup made may use a password (`TANGIBLE_PASSWORD_SIGN_IN=setup-only`): \
                   there, an account is made the first time its person signs in through the \
                   identity provider.",
    request_body = CreateUserRequest,
    responses(
        (status = 201, description = "Created", body = UserView),
        (status = 403, description = "Not an administrator, or password accounts are off here \
                                      (`TANGIBLE_PASSWORD_SIGN_IN=setup-only`)", body = Problem),
        (status = 409, description = "The username is taken", body = Problem),
        (status = 422, description = "The username, password or role will not do", body = Problem),
    ),
)]
pub async fn create_account(
    State(state): State<ApiState>,
    SignedIn(actor): SignedIn,
    Json(request): Json<CreateUserRequest>,
) -> Result<(StatusCode, Json<UserView>), Problem> {
    // An account made here signs in with a password, which this server
    // would refuse it: it would be an account nobody can use.
    if state.auth().password_sign_in == PasswordSignIn::SetupOnly {
        return Err(Problem::new(
            ErrorCode::PasswordSignInOff,
            "only the account setup made may sign in with a password here \
             (TANGIBLE_PASSWORD_SIGN_IN=setup-only), so an account made with one could never \
             sign in. Each person's account is made the first time they sign in through the \
             identity provider, with the role their group gives; set \
             TANGIBLE_PASSWORD_SIGN_IN=all and restart the server to make password accounts",
        ));
    }
    let username = normalize_username(&request.username).map_err(|error| invalid(&error))?;
    check_new_password(request.password.expose()).map_err(|error| invalid(&error))?;
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
        .route("/users/{user_id}", delete(delete_account))
        .route("/users/{user_id}/disable", post(disable_account))
        .route("/users/{user_id}/enable", post(enable_account))
}
