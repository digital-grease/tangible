// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Deleting artifacts, as tombstones (ADR 0020, docs/plans/deletion.md).
//!
//! A request marks an artifact pending deletion; keeping it undoes that;
//! once its grace period ends the deletion runner makes the tombstone, which
//! takes the artifact's references off its objects, and then collects the
//! objects nothing else refers to. The transitions are the domain's
//! ([`tangible_domain::deletion::Deletion`]); this persists them, refuses
//! what is held, and audits each step in the same transaction.

use sqlx::{PgPool, Postgres, Row as _, Transaction};
use tangible_domain::deletion::{Deletion, DeletionError};
use tangible_domain::{ArtifactId, ArtifactOrigin};
use time::OffsetDateTime;

use crate::DbError;

/// What holds an artifact so that it cannot be deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hold {
    /// A retention lock: it is pinned.
    Retained,
    /// Work that reads it is queued or under way: what, in words.
    InUse(String),
}

/// What a request or a keep came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeletionChange {
    /// Done, or already so: the artifact's deletion state as it now is.
    Done(Deletion),
    /// No such artifact.
    NotFound,
    /// The step is not one the artifact's state allows.
    Refused(DeletionError),
    /// Something holds it.
    Held(Hold),
}

/// An artifact's deletion state, as stored.
fn deletion_from_row(row: &sqlx::postgres::PgRow) -> Result<Deletion, DbError> {
    let get = DbError::Query;
    let pending: bool = row.try_get("pending_deletion").map_err(get)?;
    let requested_at: Option<OffsetDateTime> = row.try_get("deletion_requested_at").map_err(get)?;
    let delete_after: Option<OffsetDateTime> = row.try_get("delete_after").map_err(get)?;
    let deleted_at: Option<OffsetDateTime> = row.try_get("deleted_at").map_err(get)?;
    Ok(match (deleted_at, pending, requested_at, delete_after) {
        (Some(deleted_at), ..) => Deletion::Deleted { deleted_at },
        (None, true, Some(requested_at), Some(delete_after)) => Deletion::Pending {
            requested_at,
            delete_after,
        },
        _ => Deletion::Kept,
    })
}

const DELETION_COLUMNS: &str =
    "origin, retention_lock, pending_deletion, deletion_requested_at, delete_after, deleted_at";

/// The artifact's origin, retention lock and deletion state, its row locked.
async fn lock_artifact(
    tx: &mut Transaction<'_, Postgres>,
    id: ArtifactId,
) -> Result<Option<(ArtifactOrigin, bool, Deletion)>, DbError> {
    let row = sqlx::query(&format!(
        "SELECT {DELETION_COLUMNS} FROM artifacts WHERE id = $1 FOR UPDATE"
    ))
    .bind(id.as_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let origin: String = row.try_get("origin").map_err(DbError::Query)?;
    let origin = origin
        .parse::<ArtifactOrigin>()
        .map_err(|_| DbError::Enum {
            column: "origin",
            value: origin,
        })?;
    let retained: bool = row.try_get("retention_lock").map_err(DbError::Query)?;
    Ok(Some((origin, retained, deletion_from_row(&row)?)))
}

/// An artifact's deletion state.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn deletion_state(pool: &PgPool, id: ArtifactId) -> Result<Option<Deletion>, DbError> {
    let row = sqlx::query(&format!(
        "SELECT {DELETION_COLUMNS} FROM artifacts WHERE id = $1"
    ))
    .bind(id.as_uuid())
    .fetch_optional(pool)
    .await
    .map_err(DbError::Query)?;
    row.as_ref().map(deletion_from_row).transpose()
}

/// What, if anything, holds an artifact: its retention lock, or work on it
/// that is queued or under way.
async fn hold(
    tx: &mut Transaction<'_, Postgres>,
    id: ArtifactId,
    retained: bool,
) -> Result<Option<Hold>, DbError> {
    if retained {
        return Ok(Some(Hold::Retained));
    }
    let checks: [(&str, &str); 4] = [
        (
            "a burn of it is queued or under way",
            "SELECT EXISTS (SELECT 1 FROM burn_jobs WHERE artifact_id = $1
                 AND state NOT IN ('complete', 'failed', 'canceled'))",
        ),
        (
            "a derivative of it is being made",
            "SELECT EXISTS (SELECT 1 FROM derivation_jobs WHERE parent_artifact_id = $1
                 AND state IN ('queued', 'running', 'failed_retryable'))",
        ),
        (
            "its import's source is still being removed",
            "SELECT EXISTS (SELECT 1 FROM source_removals WHERE artifact_id = $1
                 AND state IN ('verifying', 'removing'))",
        ),
        (
            "a check of it is queued or under way",
            "SELECT EXISTS (SELECT 1 FROM integrity_checks WHERE artifact_id = $1
                 AND state IN ('queued', 'running'))",
        ),
    ];
    for (what, query) in checks {
        let found: bool = sqlx::query_scalar(query)
            .bind(id.as_uuid())
            .fetch_one(&mut **tx)
            .await
            .map_err(DbError::Query)?;
        if found {
            return Ok(Some(Hold::InUse(what.to_owned())));
        }
    }
    Ok(None)
}

/// Ask for an artifact to be deleted after `grace`, by `actor`.
///
/// Asking again while it is pending changes nothing.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn request_deletion(
    pool: &PgPool,
    id: ArtifactId,
    actor: &str,
    confirm_original: bool,
    grace: time::Duration,
) -> Result<DeletionChange, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let Some((origin, retained, current)) = lock_artifact(&mut tx, id).await? else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(DeletionChange::NotFound);
    };
    let next = match current.request(origin, confirm_original, OffsetDateTime::now_utc(), grace) {
        Ok(next) => next,
        Err(error) => {
            tx.rollback().await.map_err(DbError::Query)?;
            return Ok(DeletionChange::Refused(error));
        }
    };
    if next == current {
        tx.commit().await.map_err(DbError::Query)?;
        return Ok(DeletionChange::Done(current));
    }
    if let Some(held) = hold(&mut tx, id, retained).await? {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(DeletionChange::Held(held));
    }
    let Deletion::Pending {
        requested_at,
        delete_after,
    } = next
    else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(DeletionChange::Done(current));
    };
    sqlx::query(
        "UPDATE artifacts
         SET pending_deletion = TRUE, deletion_requested_at = $2, deletion_requested_by = $3,
             delete_after = $4, state_changed_at = now()
         WHERE id = $1",
    )
    .bind(id.as_uuid())
    .bind(requested_at)
    .bind(actor)
    .bind(delete_after)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    crate::accounts::audit(
        &mut tx,
        actor,
        "artifact.deletion_requested",
        &id.to_string(),
        serde_json::json!({
            "origin": origin.as_str(),
            "delete_after": delete_after
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
        }),
    )
    .await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(DeletionChange::Done(next))
}

/// Keep an artifact that was pending deletion.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn keep_artifact(
    pool: &PgPool,
    id: ArtifactId,
    actor: &str,
) -> Result<DeletionChange, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let Some((_, _, current)) = lock_artifact(&mut tx, id).await? else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(DeletionChange::NotFound);
    };
    let next = match current.keep() {
        Ok(next) => next,
        Err(error) => {
            tx.rollback().await.map_err(DbError::Query)?;
            return Ok(DeletionChange::Refused(error));
        }
    };
    sqlx::query(
        "UPDATE artifacts
         SET pending_deletion = FALSE, deletion_requested_at = NULL,
             deletion_requested_by = NULL, delete_after = NULL, state_changed_at = now()
         WHERE id = $1",
    )
    .bind(id.as_uuid())
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    crate::accounts::audit(
        &mut tx,
        actor,
        "artifact.deletion_canceled",
        &id.to_string(),
        serde_json::json!({}),
    )
    .await?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(DeletionChange::Done(next))
}

/// Artifacts whose grace period has ended, oldest due first.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn due_for_deletion(pool: &PgPool, limit: i64) -> Result<Vec<ArtifactId>, DbError> {
    let ids: Vec<uuid::Uuid> = sqlx::query_scalar(
        "SELECT id FROM artifacts WHERE pending_deletion AND delete_after <= now()
         ORDER BY delete_after, id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(ids.into_iter().map(ArtifactId::from_uuid).collect())
}

/// What making a tombstone came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tombstone {
    /// Made: the artifact is deleted, its references taken off its objects.
    Made,
    /// Not made this time: not due, kept meanwhile, or held; why.
    NotNow(String),
}

/// Make an artifact that is due into a tombstone: deleted, its references
/// taken off its objects, audited as the system's doing. Its holds are
/// checked again first: anything that started using it meanwhile defers it.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn make_tombstone(pool: &PgPool, id: ArtifactId) -> Result<Tombstone, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let Some((_, retained, current)) = lock_artifact(&mut tx, id).await? else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Tombstone::NotNow("it does not exist".to_owned()));
    };
    let now = OffsetDateTime::now_utc();
    let Deletion::Deleted { deleted_at } = (match current.complete(now) {
        Ok(next) => next,
        Err(error) => {
            tx.rollback().await.map_err(DbError::Query)?;
            return Ok(Tombstone::NotNow(error.to_string()));
        }
    }) else {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Tombstone::NotNow("it is not due".to_owned()));
    };
    if let Some(held) = hold(&mut tx, id, retained).await? {
        tx.rollback().await.map_err(DbError::Query)?;
        return Ok(Tombstone::NotNow(match held {
            Hold::Retained => "it has a retention lock".to_owned(),
            Hold::InUse(what) => what,
        }));
    }
    sqlx::query(
        "UPDATE artifacts SET pending_deletion = FALSE, deleted_at = $2, state_changed_at = now()
         WHERE id = $1",
    )
    .bind(id.as_uuid())
    .bind(deleted_at)
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    // One reference per distinct object, as registration added them.
    let released = sqlx::query(
        "UPDATE cas_objects SET reference_count = reference_count - 1, updated_at = now()
         WHERE reference_count > 0
           AND sha256 IN (SELECT DISTINCT sha256 FROM artifact_components WHERE artifact_id = $1)",
    )
    .bind(id.as_uuid())
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?
    .rows_affected();
    sqlx::query(
        "INSERT INTO audit_events
             (id, actor_type, actor_id, action, target_type, target_id, outcome, metadata)
         VALUES ($1, 'system', 'deletion', 'artifact.deleted', 'artifact', $2, 'success', $3)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(id.to_string())
    .bind(serde_json::json!({ "objects_released": released }))
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(Tombstone::Made)
}

/// An object a deleted artifact had that nothing else counts, with its size:
/// what collection may remove, its other marks permitting.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn collectable_objects(pool: &PgPool) -> Result<Vec<(String, i64)>, DbError> {
    sqlx::query_as(
        "SELECT o.sha256, o.size_bytes FROM cas_objects o
         WHERE o.reference_count = 0 AND NOT o.retention_lock AND o.removed_at IS NULL
           AND EXISTS (SELECT 1 FROM artifact_components c JOIN artifacts a ON a.id = c.artifact_id
                       WHERE c.sha256 = o.sha256 AND a.deleted_at IS NOT NULL)
           AND NOT EXISTS (SELECT 1 FROM artifact_components c JOIN artifacts a ON a.id = c.artifact_id
                           WHERE c.sha256 = o.sha256 AND a.deleted_at IS NULL)
         ORDER BY o.sha256",
    )
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)
}

/// One object's removal, its row locked until it is finished.
pub struct Removal {
    tx: Transaction<'static, Postgres>,
    sha256: String,
}

impl std::fmt::Debug for Removal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Removal")
            .field("sha256", &self.sha256)
            .finish_non_exhaustive()
    }
}

/// Lock an object's row and confirm it may still be removed: nothing counts
/// it, it is not locked, not removed already. `None` if it may not. The
/// caller removes the bytes, then [`Removal::finish`]es; an artifact being
/// registered that would count it waits on the row meanwhile.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn begin_removal(pool: &PgPool, sha256: &str) -> Result<Option<Removal>, DbError> {
    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    let removable: Option<bool> = sqlx::query_scalar(
        "SELECT reference_count = 0 AND NOT retention_lock AND removed_at IS NULL
         FROM cas_objects WHERE sha256 = $1 FOR UPDATE",
    )
    .bind(sha256)
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    if removable == Some(true) {
        Ok(Some(Removal {
            tx,
            sha256: sha256.to_owned(),
        }))
    } else {
        tx.rollback().await.map_err(DbError::Query)?;
        Ok(None)
    }
}

impl Removal {
    /// Record the object as removed.
    ///
    /// # Errors
    ///
    /// [`DbError::Query`] on a database failure.
    pub async fn finish(mut self) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE cas_objects SET removed_at = now(), updated_at = now() WHERE sha256 = $1",
        )
        .bind(&self.sha256)
        .execute(&mut *self.tx)
        .await
        .map_err(DbError::Query)?;
        self.tx.commit().await.map_err(DbError::Query)
    }

    /// Leave the object as it was: its bytes could not be removed.
    ///
    /// # Errors
    ///
    /// [`DbError::Query`] on a database failure.
    pub async fn abandon(self) -> Result<(), DbError> {
        self.tx.rollback().await.map_err(DbError::Query)
    }
}

/// Audit a collection pass that removed something.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn audit_collection(pool: &PgPool, objects: u64, bytes: u64) -> Result<(), DbError> {
    sqlx::query(
        "INSERT INTO audit_events
             (id, actor_type, actor_id, action, target_type, target_id, outcome, metadata)
         VALUES ($1, 'system', 'deletion', 'storage.objects_removed', 'storage', NULL, 'success', $2)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(serde_json::json!({ "objects": objects, "bytes": bytes }))
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}
