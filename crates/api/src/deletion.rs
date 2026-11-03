// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Deleting artifacts, as tombstones (ADR 0020, docs/plans/deletion.md).
//!
//! An administrator asks for an artifact to be deleted
//! (`POST /artifacts/{id}/deletion-requests`); it is pending for the grace period,
//! during which `DELETE /artifacts/{id}/deletion-requests` keeps it. A background
//! runner then makes it a tombstone, which takes its references off its
//! objects, and collects the objects nothing else refers to, holding the
//! store's collection lock so that no ingest can count an object as it goes.
//! The record (row, components, manifest, history) stays.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use tangible_db::Database;
use tangible_db::deletions::{
    DeletionChange, Due, Hold, Tombstone, audit_collection, begin_removal, collectable_objects,
    deletion_state, due_for_deletion, keep_artifact, make_tombstone, request_deletion,
};
use tangible_domain::ArtifactId;
use tangible_domain::deletion::{Deletion, DeletionError};
use tangible_storage::ManifestStore;
use tokio::sync::Notify;
use utoipa::ToSchema;

use crate::auth::SignedIn;
use crate::problem::{ErrorCode, Problem};
use crate::routes::artifacts::ArtifactDeletionView;
use crate::state::ApiState;

/// How long a deleted artifact's bytes are kept, unless configured.
pub const DEFAULT_GRACE_DAYS: u32 = 7;

/// How often the runner looks for artifacts due, when nothing prompts it.
const INTERVAL: Duration = Duration::from_secs(15 * 60);

/// How many artifacts due are read at a time.
const BATCH: i64 = 100;

/// How long collection waits for a moment when nothing is ingesting before
/// it waits for the ingests under way instead, holding new ones back: a
/// library imported into without pause would otherwise never be collected.
const MAX_DEFERRAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Refuse to use an artifact's bytes when it is deleted or, unless
/// `pending_is_usable`, pending deletion.
///
/// # Errors
///
/// `ARTIFACT_DELETED` when it is, `STORAGE_UNAVAILABLE` when its state
/// cannot be read.
pub async fn refuse_if_deleted(
    state: &ApiState,
    id: ArtifactId,
    pending_is_usable: bool,
) -> Result<(), Problem> {
    let deletion = deletion_state(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(artifact_id = %id, error = ?error, "could not read an artifact's deletion");
            Problem::storage_unavailable()
        })?;
    match deletion {
        Some(Deletion::Deleted { .. }) => Err(Problem::new(
            ErrorCode::ArtifactDeleted,
            "this artifact is deleted: its bytes are gone, and its record is all that is left",
        )),
        Some(Deletion::Pending { .. }) if !pending_is_usable => Err(Problem::new(
            ErrorCode::ArtifactDeleted,
            "this artifact is pending deletion; an administrator can keep it",
        )),
        _ => Ok(()),
    }
}

/// A request to delete an artifact.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct RequestDeletion {
    /// Required, true, to delete an original: an imported original or a
    /// disc dump, which may be somebody's only copy.
    #[serde(default)]
    pub confirm_original: bool,
}

fn parse_artifact(raw: &str) -> Result<ArtifactId, Problem> {
    raw.parse::<ArtifactId>()
        .map_err(|_| Problem::invalid_parameter("artifact_id", "not a valid identifier"))
}

fn unavailable(error: &tangible_db::DbError) -> Problem {
    tracing::error!(error = ?error, "could not change an artifact's deletion");
    Problem::storage_unavailable()
}

/// The deletion of `id` as it now is.
async fn view(state: &ApiState, id: ArtifactId) -> Result<Option<ArtifactDeletionView>, Problem> {
    let mut found = tangible_db::library::deletions_of(state.database().pool(), &[*id.as_uuid()])
        .await
        .map_err(|error| unavailable(&error))?;
    Ok(found.remove(id.as_uuid()).map(ArtifactDeletionView::from))
}

/// Answer a change that did not go through.
fn refused(change: DeletionChange, artifact_id: &str) -> Problem {
    match change {
        DeletionChange::NotFound => Problem::not_found("artifact", artifact_id),
        DeletionChange::Refused(DeletionError::OriginalNotConfirmed) => Problem::new(
            ErrorCode::ValidationFailed,
            "this artifact is an original, which may be somebody's only copy; set \
             confirm_original to true to say deleting it is intended",
        ),
        DeletionChange::Refused(DeletionError::AlreadyDeleted) => Problem::new(
            ErrorCode::ArtifactDeleted,
            "this artifact is already deleted",
        ),
        DeletionChange::Refused(error) => Problem::new(ErrorCode::Conflict, error.to_string()),
        DeletionChange::Held(Hold::Retained) => Problem::new(
            ErrorCode::ArtifactRetained,
            "this artifact has a retention lock, so it is not deleted",
        ),
        DeletionChange::Held(Hold::InUse(what)) => Problem::new(
            ErrorCode::ArtifactInUse,
            format!("{what}, so it cannot be deleted yet"),
        ),
        DeletionChange::Done(_) => Problem::new(ErrorCode::Internal, "nothing was refused"),
    }
}

/// Ask for an artifact to be deleted after the grace period.
///
/// # Errors
///
/// `NOT_FOUND`, `VALIDATION_FAILED` for an original without
/// `confirm_original`, `ARTIFACT_DELETED` for one already deleted,
/// `ARTIFACT_RETAINED` for one with a retention lock, `ARTIFACT_IN_USE`
/// while work on it is queued or under way.
#[utoipa::path(
    post,
    path = "/api/v1/artifacts/{artifact_id}/deletion-requests",
    tag = "library",
    description = "Ask for an artifact to be deleted: it is pending until its grace period \
                   (TANGIBLE_DELETION_GRACE_DAYS, 7 by default) ends, when its bytes are \
                   removed and its record kept, as a tombstone. Until then it can be kept. \
                   New burns, derivations and checks of it are refused meanwhile, and the \
                   RomM export stops using it. An original needs confirm_original. Refused \
                   with a retention lock, or while a burn, a derivation, a check or its \
                   import's source removal is queued or under way. Asking again while it is \
                   pending changes nothing. Needs an administrator, and is audited.",
    params(("artifact_id" = String, Path, description = "Artifact identifier")),
    request_body = RequestDeletion,
    responses(
        (status = 202, description = "Pending deletion", body = ArtifactDeletionView),
        (status = 404, description = "No such artifact", body = Problem),
        (status = 409, description = "Deleted already, retained, or in use", body = Problem),
        (status = 422, description = "An original, without confirm_original", body = Problem),
    ),
)]
pub async fn request_artifact_deletion(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(artifact_id): Path<String>,
    Json(request): Json<RequestDeletion>,
) -> Result<(StatusCode, Json<ArtifactDeletionView>), Problem> {
    let id = parse_artifact(&artifact_id)?;
    let grace = time::Duration::days(i64::from(state.deletion_grace_days()));
    let change = request_deletion(
        state.database().pool(),
        id,
        &user.username,
        request.confirm_original,
        grace,
    )
    .await
    .map_err(|error| unavailable(&error))?;
    if !matches!(change, DeletionChange::Done(Deletion::Pending { .. })) {
        return Err(refused(change, &artifact_id));
    }
    tracing::warn!(artifact_id = %id, by = %user.username, "artifact deletion requested");
    if let Some(romm) = state.romm() {
        // It is no longer the image a disc exports as.
        romm.nudge();
    }
    let view = view(&state, id)
        .await?
        .ok_or_else(|| Problem::not_found("artifact", &artifact_id))?;
    Ok((StatusCode::ACCEPTED, Json(view)))
}

/// Keep an artifact that was pending deletion.
///
/// # Errors
///
/// `NOT_FOUND`, `ARTIFACT_DELETED` once its bytes are gone, `CONFLICT` for
/// one not pending.
#[utoipa::path(
    delete,
    path = "/api/v1/artifacts/{artifact_id}/deletion-requests",
    tag = "library",
    description = "Keep an artifact that was pending deletion: the request is withdrawn and \
                   the artifact is as it was. Not possible once it is deleted. Needs an \
                   administrator, and is audited.",
    params(("artifact_id" = String, Path, description = "Artifact identifier")),
    responses(
        (status = 204, description = "Kept"),
        (status = 404, description = "No such artifact", body = Problem),
        (status = 409, description = "Deleted already, or not pending", body = Problem),
    ),
)]
pub async fn keep_pending_artifact(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(artifact_id): Path<String>,
) -> Result<StatusCode, Problem> {
    let id = parse_artifact(&artifact_id)?;
    let change = keep_artifact(state.database().pool(), id, &user.username)
        .await
        .map_err(|error| unavailable(&error))?;
    if !matches!(change, DeletionChange::Done(Deletion::Kept)) {
        return Err(refused(change, &artifact_id));
    }
    tracing::info!(artifact_id = %id, by = %user.username, "artifact kept");
    if let Some(romm) = state.romm() {
        romm.nudge();
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The deletion routes.
pub fn router() -> Router<ApiState> {
    Router::new().route(
        "/artifacts/{artifact_id}/deletion-requests",
        post(request_artifact_deletion).delete(keep_pending_artifact),
    )
}

/// What one pass of the runner did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeletionPass {
    /// Artifacts made tombstones.
    pub deleted: u64,
    /// Artifacts due but held, left pending for another pass.
    pub deferred: u64,
    /// Objects removed.
    pub objects_removed: u64,
    /// Their bytes.
    pub bytes_freed: u64,
    /// Objects that could be collected but were kept: something still
    /// refers to them, or the marks could not all be read.
    pub objects_kept: u64,
    /// Whether collection waited for another pass because an import or a
    /// derivation was under way.
    pub collection_deferred: bool,
    /// Whether collection, deferred too long, waited for the ingests under
    /// way and held new ones back.
    pub collection_waited: bool,
}

/// Makes due artifacts tombstones and collects their objects.
#[derive(Debug, Clone)]
pub struct DeletionRunner {
    database: Database,
    manifests: ManifestStore,
    nudge: Arc<Notify>,
    /// Since when collection has found ingests under way, each time it
    /// looked.
    deferred_since: Arc<std::sync::Mutex<Option<Instant>>>,
    max_deferral: Duration,
    batch: i64,
}

impl DeletionRunner {
    /// A runner over `database` and the library `manifests` belongs to.
    #[must_use]
    pub fn new(database: Database, manifests: ManifestStore) -> Self {
        Self {
            database,
            manifests,
            nudge: Arc::new(Notify::new()),
            deferred_since: Arc::default(),
            max_deferral: MAX_DEFERRAL,
            batch: BATCH,
        }
    }

    /// How many artifacts due are read at a time.
    #[must_use]
    pub const fn with_batch(mut self, batch: i64) -> Self {
        self.batch = if batch < 1 { 1 } else { batch };
        self
    }

    /// How long collection defers to ingests before it waits for them.
    #[must_use]
    pub const fn with_max_deferral(mut self, max_deferral: Duration) -> Self {
        self.max_deferral = max_deferral;
        self
    }

    /// When collection first deferred, of the deferrals since it last ran.
    fn deferred_since(&self) -> std::sync::MutexGuard<'_, Option<Instant>> {
        self.deferred_since
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Run until `shutdown` resolves.
    pub async fn run<S>(self, shutdown: S)
    where
        S: Future<Output = ()> + Send,
    {
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            match self.pass().await {
                Ok(pass) if pass != DeletionPass::default() => {
                    tracing::info!(
                        deleted = pass.deleted,
                        deferred = pass.deferred,
                        objects_removed = pass.objects_removed,
                        bytes_freed = pass.bytes_freed,
                        objects_kept = pass.objects_kept,
                        "deletion pass"
                    );
                }
                Ok(_) => {}
                Err(error) => tracing::error!(error = ?error, "a deletion pass failed"),
            }
            tokio::select! {
                () = &mut shutdown => return,
                () = self.nudge.notified() => {}
                () = tokio::time::sleep(INTERVAL) => {}
            }
        }
    }

    /// One pass: make every artifact due into a tombstone, then collect.
    ///
    /// # Errors
    ///
    /// A database failure, or marks that could not be read at all.
    pub async fn pass(&self) -> Result<DeletionPass, crate::gc::GcError> {
        let pool = self.database.pool();
        let mut pass = DeletionPass::default();
        let mut after = None;
        loop {
            let due = due_for_deletion(pool, after, self.batch).await?;
            for &Due { id, .. } in &due {
                match make_tombstone(pool, id).await? {
                    Tombstone::Made => {
                        tracing::warn!(artifact_id = %id, "artifact deleted: its bytes are being removed");
                        pass.deleted += 1;
                    }
                    Tombstone::NotNow(why) => {
                        tracing::info!(artifact_id = %id, %why, "a deletion is deferred");
                        pass.deferred += 1;
                    }
                }
            }
            match due.last() {
                Some(last) if i64::try_from(due.len()).unwrap_or(i64::MAX) >= self.batch => {
                    after = Some(*last);
                }
                _ => break,
            }
        }
        self.collect(&mut pass).await?;
        Ok(pass)
    }

    /// The store's collection lock: at once if nothing is ingesting; after
    /// waiting for what is, once collection has deferred for too long; or
    /// `None`, deferred to another pass.
    async fn collection_lock(
        &self,
        objects: &tangible_storage::FilesystemStore,
        pass: &mut DeletionPass,
    ) -> Option<tokio::sync::OwnedRwLockWriteGuard<()>> {
        if let Some(lock) = objects.try_collecting() {
            *self.deferred_since() = None;
            return Some(lock);
        }
        let waited = self
            .deferred_since()
            .get_or_insert_with(Instant::now)
            .elapsed();
        if waited < self.max_deferral {
            pass.collection_deferred = true;
            return None;
        }
        tracing::warn!(
            deferred_minutes = waited.as_secs() / 60,
            "collection has found imports under way for too long; it waits for them \
             now, and new imports wait for it"
        );
        let lock = objects.collecting().await;
        *self.deferred_since() = None;
        pass.collection_waited = true;
        Some(lock)
    }

    /// Remove the objects only deleted artifacts had.
    async fn collect(&self, pass: &mut DeletionPass) -> Result<(), crate::gc::GcError> {
        let pool = self.database.pool();
        if collectable_objects(pool).await?.is_empty() {
            return Ok(());
        }
        // No ingest is between storing bytes and registering them while this
        // is held, and none starts: an object found already stored is
        // counted before collection looks, or found missing and stored again
        // after.
        let objects = self.manifests.objects();
        let Some(_collecting) = self.collection_lock(objects, pass).await else {
            return Ok(());
        };
        let candidates = collectable_objects(pool).await?;
        let (live, certain) = crate::gc::live_objects(&self.database, &self.manifests).await?;
        for (sha256, size) in candidates {
            let Ok(digest) = sha256.parse::<tangible_domain::Sha256Digest>() else {
                continue;
            };
            if !certain || live.contains(&digest) {
                pass.objects_kept += 1;
                continue;
            }
            // The row is locked while the bytes go, as a second guard: no
            // ingest is between storing bytes and registering them while the
            // collection lock is held, so none can count it meanwhile.
            let Some(removal) = begin_removal(pool, &sha256).await? else {
                pass.objects_kept += 1;
                continue;
            };
            match objects.remove(&digest).await {
                Ok(_) => {
                    removal.finish().await?;
                    pass.objects_removed += 1;
                    pass.bytes_freed += u64::try_from(size).unwrap_or(0);
                }
                Err(error) => {
                    tracing::error!(%sha256, error = ?error, "an object could not be removed; it is kept");
                    removal.abandon().await?;
                    pass.objects_kept += 1;
                }
            }
        }
        if !certain {
            tracing::warn!(
                "a manifest or an import's checkpoint could not be read, so no object was removed"
            );
        }
        if pass.objects_removed > 0 {
            audit_collection(pool, pass.objects_removed, pass.bytes_freed).await?;
        }
        Ok(())
    }

    /// Ask the runner to look now.
    pub fn nudge(&self) {
        self.nudge.notify_one();
    }
}
