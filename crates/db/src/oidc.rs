// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Accounts an OpenID Connect provider vouches for, and sign-ins through it
//! under way (ADR 0018, docs/plans/oidc.md O2).
//!
//! An account is found by the provider's issuer and its subject for the
//! person, never by name or email, which a provider may reuse. Its role
//! follows the provider's groups at every sign-in, so a change there takes
//! effect at the next one.

use std::collections::HashSet;

use sqlx::{PgPool, Row};
use tangible_domain::Role;

use crate::DbError;
use crate::accounts::{USER_COLUMNS, UserRecord, audit, user_from_row};

/// Who an account created or changed through the provider is recorded as
/// created or changed by.
const ACTOR: &str = "oidc";

/// A sign-in to keep until the provider sends the browser back.
#[derive(Debug, Clone, Copy)]
pub struct NewLogin<'a> {
    /// SHA-256 of the state sent to the provider, lowercase hex.
    pub state_hash: &'a str,
    /// SHA-256 of the cookie that binds it to the browser that started it.
    pub browser_hash: &'a str,
    /// The nonce the ID token must carry.
    pub nonce: &'a str,
    /// The PKCE verifier the code exchange presents.
    pub verifier: &'a str,
    /// The path on Tangible to return to.
    pub return_to: &'a str,
    /// How long it may wait.
    pub lifetime_seconds: i64,
}

/// A sign-in the provider sent the browser back for, taken once.
#[derive(Clone, PartialEq, Eq)]
pub struct PendingLogin {
    /// SHA-256 of the cookie that must come back with it.
    pub browser_hash: String,
    /// The nonce.
    pub nonce: String,
    /// The PKCE verifier.
    pub verifier: String,
    /// Where to go once signed in.
    pub return_to: String,
}

impl std::fmt::Debug for PendingLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingLogin")
            .field("return_to", &self.return_to)
            .finish_non_exhaustive()
    }
}

/// Keep a sign-in under way, and forget the ones that expired.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn begin_login(pool: &PgPool, login: &NewLogin<'_>) -> Result<(), DbError> {
    sqlx::query("DELETE FROM oidc_logins WHERE expires_at < now()")
        .execute(pool)
        .await
        .map_err(DbError::Query)?;
    sqlx::query(
        "INSERT INTO oidc_logins (id, state_hash, browser_hash, nonce, verifier, return_to, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, now() + ($7::bigint * interval '1 second'))",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(login.state_hash)
    .bind(login.browser_hash)
    .bind(login.nonce)
    .bind(login.verifier)
    .bind(login.return_to)
    .bind(login.lifetime_seconds.max(1))
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}

/// Take the sign-in a state names, once, if it has not expired.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn take_login(pool: &PgPool, state_hash: &str) -> Result<Option<PendingLogin>, DbError> {
    let row = sqlx::query(
        "DELETE FROM oidc_logins WHERE state_hash = $1 AND expires_at > now()
         RETURNING browser_hash, nonce, verifier, return_to",
    )
    .bind(state_hash)
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    let get = DbError::Query;
    row.map(|row| {
        Ok(PendingLogin {
            browser_hash: row.try_get("browser_hash").map_err(get)?,
            nonce: row.try_get("nonce").map_err(get)?,
            verifier: row.try_get("verifier").map_err(get)?,
            return_to: row.try_get("return_to").map_err(get)?,
        })
    })
    .transpose()
}

/// The account the provider's `(issuer, subject)` belongs to.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn user_for_identity(
    pool: &PgPool,
    issuer: &str,
    subject: &str,
) -> Result<Option<UserRecord>, DbError> {
    let columns = USER_COLUMNS
        .split(", ")
        .map(|column| format!("u.{column}"))
        .collect::<Vec<_>>()
        .join(", ");
    let row = sqlx::query(&format!(
        "SELECT {columns} FROM users u
         JOIN user_identities i ON i.user_id = u.id
         WHERE i.issuer = $1 AND i.subject = $2"
    ))
    .bind(issuer)
    .bind(subject)
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    row.as_ref().map(user_from_row).transpose()
}

/// Create the account for a provider identity seen for the first time, with
/// no password, named `username` or, when that is taken, the same with the
/// lowest free number after it (`alice2`), so it never takes over an
/// account by name. Audited as created by `oidc`.
///
/// When the identity was created meanwhile (two first sign-ins at once),
/// answers the account it belongs to.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn create_identity_user(
    pool: &PgPool,
    issuer: &str,
    subject: &str,
    username: &str,
    role: Role,
) -> Result<UserRecord, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let mut created = None;
    for n in 1..=1000_u32 {
        let candidate = if n == 1 {
            username.to_owned()
        } else {
            let suffix = n.to_string();
            let keep = 64_usize.saturating_sub(suffix.len());
            format!("{}{suffix}", &username[..username.len().min(keep)])
        };
        let row = sqlx::query(&format!(
            "INSERT INTO users (id, username, password_hash, role, created_by)
             VALUES ($1, $2, NULL, $3, $4)
             ON CONFLICT (username) DO NOTHING
             RETURNING {USER_COLUMNS}"
        ))
        .bind(uuid::Uuid::now_v7())
        .bind(&candidate)
        .bind(role.as_str())
        .bind(ACTOR)
        .fetch_optional(&mut *tx)
        .await
        .map_err(DbError::Query)?;
        if let Some(row) = row {
            created = Some(user_from_row(&row)?);
            break;
        }
    }
    let Some(user) = created else {
        return Err(DbError::Enum {
            column: "users.username",
            value: format!("{username}: no free name with a number after it"),
        });
    };
    let linked = sqlx::query(
        "INSERT INTO user_identities (id, user_id, issuer, subject)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (issuer, subject) DO NOTHING",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(user.id)
    .bind(issuer)
    .bind(subject)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    if linked.rows_affected() == 0 {
        // Another first sign-in won; this account was never needed.
        tx.rollback().await.map_err(DbError::Query)?;
        return user_for_identity(pool, issuer, subject)
            .await?
            .ok_or(DbError::Enum {
                column: "user_identities",
                value: "an identity both present and absent".to_owned(),
            });
    }
    audit(
        &mut tx,
        ACTOR,
        "user.created",
        &user.id.to_string(),
        serde_json::json!({ "role": role.as_str(), "via": "oidc", "issuer": issuer }),
    )
    .await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(user)
}

/// Give an account the role the provider's groups map to now, if it has
/// another. Audited as changed by `oidc`. Answers the account as it is now.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn follow_provider_role(
    pool: &PgPool,
    user: UserRecord,
    role: Role,
) -> Result<UserRecord, DbError> {
    if user.role == role {
        return Ok(user);
    }
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    sqlx::query("UPDATE users SET role = $2, updated_at = now() WHERE id = $1")
        .bind(user.id)
        .bind(role.as_str())
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    audit(
        &mut tx,
        ACTOR,
        "user.role_changed",
        &user.id.to_string(),
        serde_json::json!({ "from": user.role.as_str(), "to": role.as_str() }),
    )
    .await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(UserRecord { role, ..user })
}

/// Which of `users` sign in through a provider.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn identity_users(
    pool: &PgPool,
    users: &[uuid::Uuid],
) -> Result<HashSet<uuid::Uuid>, DbError> {
    let found: Vec<uuid::Uuid> =
        sqlx::query_scalar("SELECT user_id FROM user_identities WHERE user_id = ANY($1)")
            .bind(users)
            .fetch_all(pool)
            .await
            .map_err(DbError::Query)?;
    Ok(found.into_iter().collect())
}

/// Record a sign-in through the provider that was refused, and why (a
/// stable code, such as `OIDC_NO_ROLE`), as a refused password sign-in is.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn record_refused(
    pool: &PgPool,
    issuer: &str,
    subject: Option<&str>,
    reason: &str,
) -> Result<(), DbError> {
    let actor: String = subject.unwrap_or("-").chars().take(200).collect();
    sqlx::query(
        "INSERT INTO audit_events (id, actor_type, actor_id, action, target_type, outcome, metadata)
         VALUES ($1, 'user', $2, 'session.refused', 'session', 'denied',
                 jsonb_build_object('via', 'oidc', 'issuer', $3::text, 'reason', $4::text))",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(actor)
    .bind(issuer)
    .bind(reason)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}
