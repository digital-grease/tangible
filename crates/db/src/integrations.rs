// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Integrations an administrator configures, and their sealed secrets
//! (ADR 0016).
//!
//! Settings are checked by the caller before they arrive here; secrets
//! arrive sealed and leave sealed. Every change is audited, a secret's by
//! field name only. The RomM export's row is made and kept by the exporter
//! from its own setting and is not changed here.

use sqlx::{PgPool, Row};
use time::OffsetDateTime;

use crate::DbError;

/// The integration row the RomM export keeps; never changed or removed here.
pub use crate::romm::INTEGRATION_NAME as ROMM_INTEGRATION_NAME;

/// An integration, as stored, without its secrets' values.
#[derive(Debug, Clone, PartialEq)]
pub struct IntegrationRecord {
    /// Its id.
    pub id: uuid::Uuid,
    /// Such as `qbittorrent`.
    pub kind: String,
    /// Its name, unique.
    pub name: String,
    /// Whether it is in use.
    pub enabled: bool,
    /// Its settings, as checked.
    pub configuration: serde_json::Value,
    /// `unconfigured`, `ok`, `degraded`, `failing` or `disabled`.
    pub status: String,
    /// When it last worked.
    pub last_success_at: Option<OffsetDateTime>,
    /// Why it last failed, as a stable code.
    pub last_error_code: Option<String>,
    /// The client's API version, as last seen.
    pub client_version: Option<String>,
    /// The secrets it holds, by field name.
    pub secret_fields: Vec<String>,
    /// When it was made.
    pub created_at: OffsetDateTime,
    /// When it last changed.
    pub updated_at: OffsetDateTime,
}

/// A secret, sealed, as stored.
#[derive(Clone, PartialEq, Eq)]
pub struct SealedRow {
    /// The integration.
    pub integration_id: uuid::Uuid,
    /// The field.
    pub field: String,
    /// The key it was sealed with, by id.
    pub key_id: String,
    /// Its nonce.
    pub nonce: Vec<u8>,
    /// Its ciphertext and tag.
    pub ciphertext: Vec<u8>,
}

impl std::fmt::Debug for SealedRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SealedRow")
            .field("integration_id", &self.integration_id)
            .field("field", &self.field)
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

/// Why a change was not made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrationOutcome {
    /// No such integration.
    NotFound,
    /// The name is taken.
    NameTaken,
    /// It is the RomM export's, which its setting keeps.
    Managed,
    /// Something still refers to it.
    InUse,
}

const COLUMNS: &str = "i.id, i.kind, i.name, i.enabled, i.configuration_json, i.status,
     i.last_success_at, i.last_error_code, i.client_version, i.created_at, i.updated_at,
     COALESCE((SELECT array_agg(s.field ORDER BY s.field) FROM integration_secrets s
               WHERE s.integration_id = i.id), '{}') AS secret_fields";

fn from_row(row: &sqlx::postgres::PgRow) -> Result<IntegrationRecord, DbError> {
    let get = DbError::Query;
    Ok(IntegrationRecord {
        id: row.try_get("id").map_err(get)?,
        kind: row.try_get("kind").map_err(get)?,
        name: row.try_get("name").map_err(get)?,
        enabled: row.try_get("enabled").map_err(get)?,
        configuration: row.try_get("configuration_json").map_err(get)?,
        status: row.try_get("status").map_err(get)?,
        last_success_at: row.try_get("last_success_at").map_err(get)?,
        last_error_code: row.try_get("last_error_code").map_err(get)?,
        client_version: row.try_get("client_version").map_err(get)?,
        secret_fields: row.try_get("secret_fields").map_err(get)?,
        created_at: row.try_get("created_at").map_err(get)?,
        updated_at: row.try_get("updated_at").map_err(get)?,
    })
}

/// Every integration, by name.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn list_integrations(pool: &PgPool) -> Result<Vec<IntegrationRecord>, DbError> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM integrations i ORDER BY i.name"
    ))
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter().map(from_row).collect()
}

/// One integration.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn get_integration(
    pool: &PgPool,
    id: uuid::Uuid,
) -> Result<Option<IntegrationRecord>, DbError> {
    let row = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM integrations i WHERE i.id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    row.as_ref().map(from_row).transpose()
}

async fn audit(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: &str,
    action: &str,
    id: uuid::Uuid,
    metadata: &serde_json::Value,
) -> Result<(), DbError> {
    sqlx::query(
        "INSERT INTO audit_events
             (id, actor_type, actor_id, action, target_type, target_id, outcome, metadata)
         VALUES ($1, 'user', $2, $3, 'integration', $4, 'success', $5)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(actor)
    .bind(action)
    .bind(id.to_string())
    .bind(metadata)
    .execute(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}

/// A secret to store, already sealed for its integration and field.
#[derive(Clone, Copy)]
pub struct NewSecret<'a> {
    /// The field.
    pub field: &'a str,
    /// The key's id.
    pub key_id: &'a str,
    /// The nonce.
    pub nonce: &'a [u8],
    /// The ciphertext and tag.
    pub ciphertext: &'a [u8],
}

async fn put_secret(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: uuid::Uuid,
    secret: &NewSecret<'_>,
) -> Result<(), DbError> {
    sqlx::query(
        "INSERT INTO integration_secrets (integration_id, field, key_id, nonce, ciphertext)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (integration_id, field) DO UPDATE SET
             key_id = EXCLUDED.key_id, nonce = EXCLUDED.nonce,
             ciphertext = EXCLUDED.ciphertext, updated_at = now()",
    )
    .bind(id)
    .bind(secret.field)
    .bind(secret.key_id)
    .bind(secret.nonce)
    .bind(secret.ciphertext)
    .execute(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}

/// A new integration.
pub struct NewIntegration<'a> {
    /// Its id, chosen by the caller, who sealed its secrets for it.
    pub id: uuid::Uuid,
    /// Its kind.
    pub kind: &'a str,
    /// Its name.
    pub name: &'a str,
    /// Whether it is in use.
    pub enabled: bool,
    /// Its settings, checked.
    pub configuration: &'a serde_json::Value,
    /// Its secrets, sealed.
    pub secrets: &'a [NewSecret<'a>],
    /// Who made it.
    pub actor: &'a str,
}

/// Make an integration with its secrets, audited.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure. A taken name is an
/// [`IntegrationOutcome`].
pub async fn create_integration(
    pool: &PgPool,
    new: &NewIntegration<'_>,
) -> Result<Result<IntegrationRecord, IntegrationOutcome>, DbError> {
    if new.name == ROMM_INTEGRATION_NAME {
        return Ok(Err(IntegrationOutcome::NameTaken));
    }
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let inserted = sqlx::query(
        "INSERT INTO integrations (id, kind, name, enabled, configuration_json, status)
         VALUES ($1, $2, $3, $4, $5, 'unconfigured')
         ON CONFLICT (name) DO NOTHING",
    )
    .bind(new.id)
    .bind(new.kind)
    .bind(new.name)
    .bind(new.enabled)
    .bind(new.configuration)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    if inserted.rows_affected() == 0 {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Err(IntegrationOutcome::NameTaken));
    }
    for secret in new.secrets {
        put_secret(&mut tx, new.id, secret).await?;
    }
    let fields: Vec<&str> = new.secrets.iter().map(|secret| secret.field).collect();
    audit(
        &mut tx,
        new.actor,
        "integration.created",
        new.id,
        &serde_json::json!({ "kind": new.kind, "name": new.name, "secrets": fields }),
    )
    .await?;
    tx.commit().await.map_err(DbError::Query)?;
    get_integration(pool, new.id)
        .await
        .map(|found| found.ok_or(IntegrationOutcome::NotFound))
}

/// A change to an integration; `None` leaves a field as it is.
pub struct IntegrationChange<'a> {
    /// A new name.
    pub name: Option<&'a str>,
    /// Turned on or off.
    pub enabled: Option<bool>,
    /// New settings, checked.
    pub configuration: Option<&'a serde_json::Value>,
    /// Secrets to set, sealed.
    pub set_secrets: &'a [NewSecret<'a>],
    /// Secrets to remove, by field.
    pub remove_secrets: &'a [&'a str],
    /// Who made the change.
    pub actor: &'a str,
}

/// Change an integration, audited. Changing its settings or secrets marks it
/// unconfigured until it is next tried.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure. A missing, managed or renamed-
/// into-a-taken-name integration is an [`IntegrationOutcome`].
pub async fn update_integration(
    pool: &PgPool,
    id: uuid::Uuid,
    change: &IntegrationChange<'_>,
) -> Result<Result<IntegrationRecord, IntegrationOutcome>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let name: Option<String> =
        sqlx::query_scalar("SELECT name FROM integrations WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(DbError::Query)?;
    match name.as_deref() {
        None => return Ok(Err(IntegrationOutcome::NotFound)),
        Some(ROMM_INTEGRATION_NAME) => return Ok(Err(IntegrationOutcome::Managed)),
        Some(_) => {}
    }
    if change.name == Some(ROMM_INTEGRATION_NAME) {
        return Ok(Err(IntegrationOutcome::NameTaken));
    }
    if let Some(new_name) = change.name {
        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM integrations WHERE name = $1 AND id <> $2)",
        )
        .bind(new_name)
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::Query)?;
        if taken {
            tx.rollback().await.map_err(DbError::Query)?;
            return Ok(Err(IntegrationOutcome::NameTaken));
        }
    }
    let reconfigured = change.configuration.is_some()
        || !change.set_secrets.is_empty()
        || !change.remove_secrets.is_empty();
    sqlx::query(
        "UPDATE integrations SET
             name = COALESCE($2, name),
             enabled = COALESCE($3, enabled),
             configuration_json = COALESCE($4, configuration_json),
             status = CASE WHEN $5 THEN 'unconfigured' ELSE status END,
             last_error_code = CASE WHEN $5 THEN NULL ELSE last_error_code END,
             updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(change.name)
    .bind(change.enabled)
    .bind(change.configuration)
    .bind(reconfigured)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    for secret in change.set_secrets {
        put_secret(&mut tx, id, secret).await?;
    }
    for field in change.remove_secrets {
        sqlx::query("DELETE FROM integration_secrets WHERE integration_id = $1 AND field = $2")
            .bind(id)
            .bind(field)
            .execute(&mut *tx)
            .await
            .map_err(DbError::Query)?;
    }
    audit(
        &mut tx,
        change.actor,
        "integration.updated",
        id,
        &serde_json::json!({
            "name": change.name,
            "enabled": change.enabled,
            "settings_changed": change.configuration.is_some(),
        }),
    )
    .await?;
    if !change.set_secrets.is_empty() || !change.remove_secrets.is_empty() {
        let set: Vec<&str> = change
            .set_secrets
            .iter()
            .map(|secret| secret.field)
            .collect();
        audit(
            &mut tx,
            change.actor,
            "integration.secret_changed",
            id,
            &serde_json::json!({ "set": set, "removed": change.remove_secrets }),
        )
        .await?;
    }
    tx.commit().await.map_err(DbError::Query)?;
    get_integration(pool, id)
        .await
        .map(|found| found.ok_or(IntegrationOutcome::NotFound))
}

/// Remove an integration and its secrets, audited.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure. A missing or managed
/// integration, or one something still refers to, is an
/// [`IntegrationOutcome`].
pub async fn delete_integration(
    pool: &PgPool,
    id: uuid::Uuid,
    actor: &str,
) -> Result<Result<(), IntegrationOutcome>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let name: Option<String> =
        sqlx::query_scalar("SELECT name FROM integrations WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(DbError::Query)?;
    match name.as_deref() {
        None => return Ok(Err(IntegrationOutcome::NotFound)),
        Some(ROMM_INTEGRATION_NAME) => return Ok(Err(IntegrationOutcome::Managed)),
        Some(_) => {}
    }
    sqlx::query("DELETE FROM integration_secrets WHERE integration_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    let deleted = sqlx::query("DELETE FROM integrations WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await;
    match deleted {
        Ok(_) => {}
        // A foreign key still points at it.
        Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("23503") => {
            tx.rollback().await.map_err(DbError::Query)?;
            return Ok(Err(IntegrationOutcome::InUse));
        }
        Err(error) => return Err(DbError::Query(error)),
    }
    audit(
        &mut tx,
        actor,
        "integration.deleted",
        id,
        &serde_json::json!({ "name": name }),
    )
    .await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(Ok(()))
}

/// One sealed secret of an integration.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn sealed_secret(
    pool: &PgPool,
    id: uuid::Uuid,
    field: &str,
) -> Result<Option<SealedRow>, DbError> {
    let row = sqlx::query(
        "SELECT integration_id, field, key_id, nonce, ciphertext FROM integration_secrets
         WHERE integration_id = $1 AND field = $2",
    )
    .bind(id)
    .bind(field)
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    row.as_ref().map(sealed_from_row).transpose()
}

fn sealed_from_row(row: &sqlx::postgres::PgRow) -> Result<SealedRow, DbError> {
    let get = DbError::Query;
    Ok(SealedRow {
        integration_id: row.try_get("integration_id").map_err(get)?,
        field: row.try_get("field").map_err(get)?,
        key_id: row.try_get("key_id").map_err(get)?,
        nonce: row.try_get("nonce").map_err(get)?,
        ciphertext: row.try_get("ciphertext").map_err(get)?,
    })
}

/// Re-seal every secret: `reseal` turns each stored row into its new sealed
/// form, and all are written in one transaction, or none.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure, or `reseal`'s error.
pub async fn reseal_all<E>(
    pool: &PgPool,
    mut reseal: impl FnMut(&SealedRow) -> Result<SealedRow, E>,
) -> Result<Result<usize, E>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let rows = sqlx::query(
        "SELECT integration_id, field, key_id, nonce, ciphertext FROM integration_secrets
         FOR UPDATE",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let mut count = 0;
    for row in &rows {
        let old = sealed_from_row(row)?;
        let new = match reseal(&old) {
            Ok(new) => new,
            Err(error) => {
                tx.rollback().await.map_err(DbError::Query)?;
                return Ok(Err(error));
            }
        };
        put_secret(
            &mut tx,
            old.integration_id,
            &NewSecret {
                field: &old.field,
                key_id: &new.key_id,
                nonce: &new.nonce,
                ciphertext: &new.ciphertext,
            },
        )
        .await?;
        count += 1;
    }
    tx.commit().await.map_err(DbError::Query)?;
    Ok(Ok(count))
}

/// How many sealed secrets each key holds, by key id, for diagnostics.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn secret_counts_by_key(pool: &PgPool) -> Result<Vec<(String, i64)>, DbError> {
    let rows = sqlx::query(
        "SELECT key_id, count(*) AS n FROM integration_secrets GROUP BY key_id ORDER BY key_id",
    )
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    rows.iter()
        .map(|row| {
            Ok((
                row.try_get("key_id").map_err(DbError::Query)?,
                row.try_get("n").map_err(DbError::Query)?,
            ))
        })
        .collect()
}

/// A job to hand a download client: the import it feeds is made with it.
pub struct NewIntegrationJob<'a> {
    /// The integration.
    pub integration_id: uuid::Uuid,
    /// The client's identity for it, such as a torrent's infohash.
    pub client_job_id: &'a str,
    /// The import's source kind, such as `qbittorrent_job`.
    pub source_kind: &'a str,
    /// The import's descriptor.
    pub descriptor: &'a serde_json::Value,
    /// Who submitted it.
    pub actor: &'a str,
}

/// What a submission came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submitted {
    /// A new job, and its import.
    Created {
        /// The import.
        import_id: uuid::Uuid,
    },
    /// Submitted before, and its import ended without an artifact: a new
    /// import takes over.
    Resubmitted {
        /// The new import.
        import_id: uuid::Uuid,
    },
    /// Submitted before, and its import is still going.
    AlreadyActive {
        /// That import.
        import_id: uuid::Uuid,
    },
    /// Submitted before, and imported.
    AlreadyImported {
        /// That import.
        import_id: uuid::Uuid,
        /// What it made.
        artifact_id: Option<uuid::Uuid>,
    },
}

/// Record a job handed to a download client, with the import that follows
/// it and an audit event, together. A job the client already had from an
/// earlier submission is found, not doubled.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn submit_integration_job(
    pool: &PgPool,
    new: &NewIntegrationJob<'_>,
) -> Result<Submitted, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let existing = sqlx::query(
        "SELECT j.id, j.import_job_id, i.state, i.artifact_id
         FROM integration_jobs j JOIN import_jobs i ON i.id = j.import_job_id
         WHERE j.integration_id = $1 AND j.client_job_id = $2
         FOR UPDATE OF j",
    )
    .bind(new.integration_id)
    .bind(new.client_job_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    let replacing = match existing {
        None => None,
        Some(row) => {
            let job_id: uuid::Uuid = row.try_get("id").map_err(DbError::Query)?;
            let import_id: uuid::Uuid = row.try_get("import_job_id").map_err(DbError::Query)?;
            let state: String = row.try_get("state").map_err(DbError::Query)?;
            match state.as_str() {
                "complete" => {
                    let artifact_id = row.try_get("artifact_id").map_err(DbError::Query)?;
                    tx.rollback().await.map_err(DbError::Query)?;
                    return Ok(Submitted::AlreadyImported {
                        import_id,
                        artifact_id,
                    });
                }
                "failed_terminal" | "canceled" | "quarantined" => Some(job_id),
                _ => {
                    tx.rollback().await.map_err(DbError::Query)?;
                    return Ok(Submitted::AlreadyActive { import_id });
                }
            }
        }
    };
    let import_id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO import_jobs (id, source_kind, source_descriptor_json, state, created_by)
         VALUES ($1, $2, $3, 'requested', $4)",
    )
    .bind(import_id)
    .bind(new.source_kind)
    .bind(new.descriptor)
    .bind(new.actor)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    match replacing {
        None => {
            sqlx::query(
                "INSERT INTO integration_jobs
                     (id, integration_id, client_job_id, import_job_id, created_by)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(uuid::Uuid::now_v7())
            .bind(new.integration_id)
            .bind(new.client_job_id)
            .bind(import_id)
            .bind(new.actor)
            .execute(&mut *tx)
            .await
            .map_err(DbError::Query)?;
        }
        Some(job_id) => {
            sqlx::query(
                "UPDATE integration_jobs SET import_job_id = $2, client_state = NULL
                 WHERE id = $1",
            )
            .bind(job_id)
            .bind(import_id)
            .execute(&mut *tx)
            .await
            .map_err(DbError::Query)?;
        }
    }
    audit(
        &mut tx,
        new.actor,
        "integration.job_submitted",
        new.integration_id,
        &serde_json::json!({
            "client_job_id": new.client_job_id,
            "import_id": import_id,
            "resubmitted": replacing.is_some(),
        }),
    )
    .await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(if replacing.is_some() {
        Submitted::Resubmitted { import_id }
    } else {
        Submitted::Created { import_id }
    })
}

/// Record how talking to an integration's client went: working, with the
/// version it gave, or failing with a stable code.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn record_integration_health(
    pool: &PgPool,
    id: uuid::Uuid,
    outcome: Result<Option<&str>, &str>,
) -> Result<(), DbError> {
    let query = match outcome {
        Ok(version) => sqlx::query(
            "UPDATE integrations SET status = 'ok', last_success_at = now(),
                 last_error_code = NULL, client_version = COALESCE($2, client_version),
                 updated_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .bind(version),
        Err(code) => sqlx::query(
            "UPDATE integrations SET status = 'failing', last_error_code = $2,
                 updated_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .bind(code),
    };
    query.execute(pool).await.map_err(DbError::Query)?;
    Ok(())
}

/// Record the client's last word on the job feeding `import_id`.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn record_integration_job_state(
    pool: &PgPool,
    import_id: uuid::Uuid,
    client_state: &str,
) -> Result<(), DbError> {
    let state: String = client_state.chars().take(64).collect();
    sqlx::query(
        "UPDATE integration_jobs SET client_state = $2
         WHERE import_job_id = $1 AND client_state IS DISTINCT FROM $2",
    )
    .bind(import_id)
    .bind(state)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}
