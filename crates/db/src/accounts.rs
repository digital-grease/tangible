// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Accounts and sessions.
//!
//! Free functions over a pool, like the rest of the repositories. Nothing here
//! sees a password or a session cookie: callers hash both before they arrive,
//! so the database holds nothing that would sign anybody in.

use sqlx::{PgPool, Row};
use std::str::FromStr;
use tangible_domain::Role;
use time::OffsetDateTime;

use crate::DbError;

/// An account as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRecord {
    /// Its identifier.
    pub id: uuid::Uuid,
    /// The username, lowercase.
    pub username: String,
    /// What it may do.
    pub role: Role,
    /// Whether it has been disabled.
    pub disabled: bool,
    /// When it last signed in.
    pub last_signed_in_at: Option<OffsetDateTime>,
    /// When it was created.
    pub created_at: OffsetDateTime,
}

/// A live session and the account it belongs to.
///
/// `Debug` is written by hand so the anti-forgery token never prints.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionRecord {
    /// The session's identifier.
    pub id: uuid::Uuid,
    /// The anti-forgery token every mutation must carry.
    pub csrf_token: String,
    /// When it ends regardless of activity.
    pub expires_at: OffsetDateTime,
    /// Whose it is.
    pub user: UserRecord,
}

impl std::fmt::Debug for SessionRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionRecord")
            .field("id", &self.id)
            .field("csrf_token", &tangible_domain::redacted(&self.csrf_token))
            .field("expires_at", &self.expires_at)
            .field("user", &self.user)
            .finish()
    }
}

/// Why an account could not be created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateUserOutcome {
    /// Another account already has that username.
    NameTaken,
}

fn user_from_row(row: &sqlx::postgres::PgRow) -> Result<UserRecord, DbError> {
    let role: String = row.try_get("role").map_err(DbError::Query)?;
    Ok(UserRecord {
        id: row.try_get("id").map_err(DbError::Query)?,
        username: row.try_get("username").map_err(DbError::Query)?,
        role: Role::from_str(&role).map_err(|_| DbError::Enum {
            column: "users.role",
            value: role.clone(),
        })?,
        disabled: row
            .try_get::<Option<OffsetDateTime>, _>("disabled_at")
            .map_err(DbError::Query)?
            .is_some(),
        last_signed_in_at: row.try_get("last_signed_in_at").map_err(DbError::Query)?,
        created_at: row.try_get("created_at").map_err(DbError::Query)?,
    })
}

const USER_COLUMNS: &str = "id, username, role, disabled_at, last_signed_in_at, created_at";

/// How many accounts exist, disabled ones included.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn count_users(pool: &PgPool) -> Result<i64, DbError> {
    sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(pool)
        .await
        .map_err(DbError::Query)
}

/// Create the first administrator, if and only if no account exists yet.
///
/// The check and the insert happen under an exclusive lock on the table, so
/// two people opening the setup page at once cannot both become the owner:
/// the second waits for the first to commit, then finds an account and gets
/// nothing. Returns `None` when an account already existed.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn create_first_administrator(
    pool: &PgPool,
    username: &str,
    password_hash: &str,
) -> Result<Option<UserRecord>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    sqlx::query("LOCK TABLE users IN EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    let existing: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    if existing > 0 {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(None);
    }

    let row = sqlx::query(&format!(
        "INSERT INTO users (id, username, password_hash, role, created_by)
         VALUES ($1, $2, $3, 'administrator', 'setup')
         RETURNING {USER_COLUMNS}"
    ))
    .bind(uuid::Uuid::now_v7())
    .bind(username)
    .bind(password_hash)
    .fetch_one(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let user = user_from_row(&row)?;
    audit(
        &mut tx,
        username,
        "user.created",
        &user.id.to_string(),
        serde_json::json!({ "role": "administrator", "via": "setup" }),
    )
    .await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(Some(user))
}

/// Create an account.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure. A username already in use is
/// [`CreateUserOutcome::NameTaken`], not an error.
pub async fn create_user(
    pool: &PgPool,
    username: &str,
    password_hash: &str,
    role: Role,
    created_by: &str,
) -> Result<Result<UserRecord, CreateUserOutcome>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let inserted = sqlx::query(&format!(
        "INSERT INTO users (id, username, password_hash, role, created_by)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (username) DO NOTHING
         RETURNING {USER_COLUMNS}"
    ))
    .bind(uuid::Uuid::now_v7())
    .bind(username)
    .bind(password_hash)
    .bind(role.as_str())
    .bind(created_by)
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let Some(row) = inserted else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(CreateUserOutcome::NameTaken));
    };
    let user = user_from_row(&row)?;
    audit(
        &mut tx,
        created_by,
        "user.created",
        &user.id.to_string(),
        serde_json::json!({ "role": role.as_str() }),
    )
    .await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(Ok(user))
}

/// Every account, oldest first.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn list_users(pool: &PgPool) -> Result<Vec<UserRecord>, DbError> {
    let rows = sqlx::query(&format!(
        "SELECT {USER_COLUMNS} FROM users ORDER BY created_at, id"
    ))
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter().map(user_from_row).collect()
}

/// An account and its password hash, for checking a sign-in.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn user_for_sign_in(
    pool: &PgPool,
    username: &str,
) -> Result<Option<(UserRecord, String)>, DbError> {
    let row = sqlx::query(&format!(
        "SELECT {USER_COLUMNS}, password_hash FROM users WHERE username = $1"
    ))
    .bind(username)
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    row.map(|row| {
        let hash: String = row.try_get("password_hash").map_err(DbError::Query)?;
        Ok((user_from_row(&row)?, hash))
    })
    .transpose()
}

/// Start a session for an account that has just proved who it is.
///
/// Always a new session, never a reused one: signing in rotates the session,
/// so an identifier somebody planted before sign-in is worth nothing after.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn create_session(
    pool: &PgPool,
    user: &UserRecord,
    token_hash: &str,
    csrf_token: &str,
    expires_at: OffsetDateTime,
) -> Result<uuid::Uuid, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO user_sessions (id, token_hash, user_id, csrf_token, expires_at)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(token_hash)
    .bind(user.id)
    .bind(csrf_token)
    .bind(expires_at)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    sqlx::query("UPDATE users SET last_signed_in_at = now() WHERE id = $1")
        .bind(user.id)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    audit(
        &mut tx,
        &user.username,
        "session.started",
        &id.to_string(),
        serde_json::json!({}),
    )
    .await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(id)
}

/// The live session a cookie names, marking it as active.
///
/// Live means not revoked, not past its absolute end, active within
/// `idle_seconds`, and belonging to an account that is not disabled. Anything
/// else is `None`, without saying which: a caller that could tell an expired
/// session from an unknown one learns something about cookies it was never
/// given.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn session_for_token(
    pool: &PgPool,
    token_hash: &str,
    idle_seconds: i64,
) -> Result<Option<SessionRecord>, DbError> {
    let row = sqlx::query(
        "WITH live AS (
             UPDATE user_sessions s SET last_seen_at = now()
             FROM users u
             WHERE s.token_hash = $1
               AND s.user_id = u.id
               AND s.revoked_at IS NULL
               AND s.expires_at > now()
               AND s.last_seen_at > now() - ($2::bigint * interval '1 second')
               AND u.disabled_at IS NULL
             RETURNING s.id AS session_id, s.csrf_token, s.expires_at, s.user_id
         )
         SELECT live.session_id, live.csrf_token, live.expires_at,
                u.id, u.username, u.role, u.disabled_at, u.last_signed_in_at, u.created_at
         FROM live JOIN users u ON u.id = live.user_id",
    )
    .bind(token_hash)
    .bind(idle_seconds)
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    row.map(|row| {
        Ok(SessionRecord {
            id: row.try_get("session_id").map_err(DbError::Query)?,
            csrf_token: row.try_get("csrf_token").map_err(DbError::Query)?,
            expires_at: row.try_get("expires_at").map_err(DbError::Query)?,
            user: user_from_row(&row)?,
        })
    })
    .transpose()
}

/// End a session.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn revoke_session(
    pool: &PgPool,
    session_id: uuid::Uuid,
    actor: &str,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    sqlx::query("UPDATE user_sessions SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL")
        .bind(session_id)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    audit(
        &mut tx,
        actor,
        "session.ended",
        &session_id.to_string(),
        serde_json::json!({}),
    )
    .await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(())
}

/// Record that somebody failed to sign in.
///
/// The username as typed, bounded, and never the password. Whether the
/// account exists is not recorded either: the log is for spotting a guessing
/// run, not for confirming which names are real.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn record_failed_sign_in(pool: &PgPool, username: &str) -> Result<(), DbError> {
    let actor: String = username.chars().take(200).collect();
    let actor = if actor.is_empty() {
        "-".to_owned()
    } else {
        actor
    };
    sqlx::query(
        "INSERT INTO audit_events
             (id, actor_type, actor_id, action, target_type, outcome)
         VALUES ($1, 'user', $2, 'session.refused', 'session', 'denied')",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(actor)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}

/// Write an audit row for a person's action, inside the caller's transaction.
async fn audit(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: &str,
    action: &str,
    target_id: &str,
    metadata: serde_json::Value,
) -> Result<(), DbError> {
    let target_type = action.split('.').next().unwrap_or("user");
    sqlx::query(
        "INSERT INTO audit_events
             (id, actor_type, actor_id, action, target_type, target_id, outcome, metadata)
         VALUES ($1, 'user', $2, $3, $4, $5, 'success', $6)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(actor)
    .bind(action)
    .bind(target_type)
    .bind(target_id)
    .bind(metadata)
    .execute(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}
