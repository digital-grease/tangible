// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The integrity runner (docs/plans/storage-integrity.md, backlog E3-06):
//! reads stored objects back and compares each with the digest and length
//! recorded when it was stored.
//!
//! One check at a time, a check of one artifact before a full scrub: the
//! work is disk-bound, and two checks would only slow each other. Objects
//! are read in digest order and progress written every few seconds with the
//! last digest finished, so a check whose lease lapses (the server
//! restarted) is taken over and resumes. A problem marks the artifacts
//! naming the object damaged as soon as it is found. Nothing is repaired:
//! the remedy for a damaged original is a backup, never a rewrite.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tangible_db::integrity::{
    IntegrityCheckRecord, MAX_KEPT_PROBLEMS, Progress, catalogued_components, claim_next_check,
    complete_check, count_and_record_totals, mark_artifact_damaged, mark_object_damaged,
    object_removed, objects_after, queue_scheduled_scrub, record_progress,
};
use tangible_db::{Database, DbError};
use tangible_domain::{ArtifactId, IntegrityProblemKind, Sha256Digest};
use tangible_storage::{FilesystemStore, ManifestStore, ManifestStoreError, StorageError};
use tokio::sync::Notify;

/// How the runner behaves.
#[derive(Debug, Clone)]
pub struct IntegrityRunnerSettings {
    /// How long a claim is held between progress writes. Long enough for the
    /// largest single object on a slow disk.
    pub lease: Duration,
    /// How long to wait when there is nothing to check and nobody nudges.
    pub poll_interval: Duration,
    /// How often progress is written.
    pub progress_every: Duration,
    /// How many objects are fetched from the database at a time.
    pub batch: i64,
    /// What to call this process in the lease.
    pub owner: String,
    /// How long after a full scrub ends the next is queued, or `None` for
    /// never (`TANGIBLE_SCRUB_INTERVAL_DAYS`, off by default: the disk load
    /// is an administrator's decision).
    pub scrub_every: Option<Duration>,
}

impl Default for IntegrityRunnerSettings {
    fn default() -> Self {
        Self {
            lease: Duration::from_mins(30),
            poll_interval: Duration::from_secs(60),
            progress_every: Duration::from_secs(2),
            batch: 500,
            owner: "tangible-server".to_owned(),
            scrub_every: None,
        }
    }
}

/// What the routes hold to wake the runner when a check is asked for.
#[derive(Debug, Clone, Default)]
pub struct Integrity {
    nudge: Arc<Notify>,
}

impl Integrity {
    /// A handle with nothing waiting on it yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Wake the runner now rather than at its next poll.
    pub fn nudge(&self) {
        self.nudge.notify_one();
    }
}

/// Runs integrity checks.
#[derive(Debug, Clone)]
pub struct IntegrityRunner {
    database: Database,
    objects: FilesystemStore,
    manifests: ManifestStore,
    integrity: Integrity,
    settings: IntegrityRunnerSettings,
}

/// A problem found, as a check records it.
fn problem(kind: IntegrityProblemKind, sha256: Option<&str>, detail: &str) -> Value {
    json!({ "kind": kind.as_str(), "sha256": sha256, "detail": detail })
}

impl IntegrityRunner {
    /// Assemble a runner.
    #[must_use]
    pub const fn new(
        database: Database,
        objects: FilesystemStore,
        manifests: ManifestStore,
        integrity: Integrity,
        settings: IntegrityRunnerSettings,
    ) -> Self {
        Self {
            database,
            objects,
            manifests,
            integrity,
            settings,
        }
    }

    /// Run checks until `shutdown` resolves. A check under way stops at the
    /// next object and resumes from there when the server is back.
    pub async fn run<S>(self, shutdown: S)
    where
        S: Future<Output = ()> + Send,
    {
        crate::background::run_steps(
            shutdown,
            || async {
                match self.step().await {
                    Ok(worked) => worked,
                    Err(error) => {
                        tracing::error!(error = ?error, "the integrity queue could not be read");
                        false
                    }
                }
            },
            || async {
                tokio::select! {
                    () = self.integrity.nudge.notified() => {}
                    () = tokio::time::sleep(self.settings.poll_interval) => {}
                }
            },
        )
        .await;
        tracing::info!("integrity runner stopped");
    }

    /// Take one check and run it to its end, or until it is withdrawn.
    /// Returns whether there was one.
    ///
    /// # Errors
    ///
    /// [`DbError`] if the queue cannot be read or written.
    pub async fn step(&self) -> Result<bool, DbError> {
        if let Some(every) = self.settings.scrub_every
            && let Some(check) =
                queue_scheduled_scrub(self.database.pool(), lease_seconds(every)).await?
        {
            tracing::info!(check_id = %check.id, "a scheduled full scrub was queued");
        }
        let lease = lease_seconds(self.settings.lease);
        let Some(check) =
            claim_next_check(self.database.pool(), &self.settings.owner, lease).await?
        else {
            return Ok(false);
        };
        tracing::info!(check_id = %check.id, scope = %check.scope, "integrity check claimed");
        self.execute(&check).await?;
        Ok(true)
    }

    async fn execute(&self, check: &IntegrityCheckRecord) -> Result<(), DbError> {
        let pool = self.database.pool();
        if check.objects_total.is_none() {
            count_and_record_totals(pool, check).await?;
        }
        let mut walk = Walk {
            objects_checked: check.objects_checked,
            bytes_checked: check.bytes_checked,
            last: check.resume_after.clone(),
            pending: Vec::new(),
            pending_count: 0,
            found: usize::try_from(check.problem_count).unwrap_or(usize::MAX),
            written_at: Instant::now(),
        };
        loop {
            let batch =
                objects_after(pool, check, walk.last.as_deref(), self.settings.batch).await?;
            if batch.is_empty() {
                break;
            }
            for (sha256, size) in batch {
                let found = match self.verify(&sha256, size).await {
                    // Collected since the walk listed it: gone on purpose.
                    Some(_) if object_removed(pool, &sha256).await? => None,
                    found => found,
                };
                if let Some(found) = found {
                    mark_object_damaged(pool, &sha256).await?;
                    walk.found(found);
                }
                walk.objects_checked += 1;
                walk.bytes_checked += size;
                walk.last = Some(sha256);
                if walk.written_at.elapsed() >= self.settings.progress_every
                    && !self.write(check, &mut walk).await?
                {
                    tracing::info!(check_id = %check.id, "integrity check withdrawn or taken over");
                    return Ok(());
                }
            }
        }
        if let Some(artifact_id) = check.artifact_id
            && let Some(found) = self.check_manifest(artifact_id).await?
        {
            mark_artifact_damaged(pool, artifact_id).await?;
            walk.found(found);
        }
        if !self.write(check, &mut walk).await? {
            return Ok(());
        }
        if complete_check(pool, check.id, &self.settings.owner).await? {
            tracing::info!(
                check_id = %check.id,
                objects = walk.objects_checked,
                problems = walk.found,
                "integrity check complete"
            );
        }
        Ok(())
    }

    /// Write progress; answers whether the check is still this process's.
    async fn write(&self, check: &IntegrityCheckRecord, walk: &mut Walk) -> Result<bool, DbError> {
        let still = record_progress(
            self.database.pool(),
            &Progress {
                id: check.id,
                owner: &self.settings.owner,
                objects_checked: walk.objects_checked,
                bytes_checked: walk.bytes_checked,
                resume_after: walk.last.as_deref(),
                new_problems: &walk.pending,
                new_problem_count: walk.pending_count,
                lease_seconds: lease_seconds(self.settings.lease),
            },
        )
        .await?;
        walk.pending.clear();
        walk.pending_count = 0;
        walk.written_at = Instant::now();
        Ok(still)
    }

    /// What is wrong with object `sha256`, if anything.
    async fn verify(&self, sha256: &str, size: i64) -> Option<Value> {
        let Ok(digest) = sha256.parse::<Sha256Digest>() else {
            return Some(problem(
                IntegrityProblemKind::Corrupt,
                Some(sha256),
                "the recorded digest does not parse",
            ));
        };
        match self.objects.verify(&digest).await {
            Ok(stat) if i64::try_from(stat.size_bytes).ok() == Some(size) => None,
            Ok(stat) => Some(problem(
                IntegrityProblemKind::SizeMismatch,
                Some(sha256),
                &format!("{} bytes, where {size} were recorded", stat.size_bytes),
            )),
            Err(StorageError::NotFound { .. }) => Some(problem(
                IntegrityProblemKind::Missing,
                Some(sha256),
                "the object is not in the store",
            )),
            Err(StorageError::Corrupt { actual, .. }) => Some(problem(
                IntegrityProblemKind::Corrupt,
                Some(sha256),
                &format!("its contents hash to {actual}"),
            )),
            // The path and the error's own text stay in the log: a check's
            // findings are shown to operators, and the layout is not theirs.
            Err(error) => {
                tracing::warn!(sha256, error = ?error, "an object could not be read");
                Some(problem(
                    IntegrityProblemKind::Unreadable,
                    Some(sha256),
                    "the object could not be read; the server's log says why",
                ))
            }
        }
    }

    /// Whether an artifact's manifest names exactly the components the
    /// catalogue records.
    async fn check_manifest(&self, artifact_id: uuid::Uuid) -> Result<Option<Value>, DbError> {
        let catalogued = catalogued_components(self.database.pool(), artifact_id).await?;
        let manifest = match self
            .manifests
            .read(ArtifactId::from_uuid(artifact_id))
            .await
        {
            Ok(manifest) => manifest,
            Err(ManifestStoreError::NotFound { .. }) => {
                return Ok(Some(problem(
                    IntegrityProblemKind::ManifestUnreadable,
                    None,
                    "the manifest is not in the library",
                )));
            }
            Err(error) => {
                tracing::warn!(artifact_id = %artifact_id, error = ?error, "a manifest could not be read");
                return Ok(Some(problem(
                    IntegrityProblemKind::ManifestUnreadable,
                    None,
                    "the manifest could not be read or does not parse; the server's log says why",
                )));
            }
        };
        let mut named: Vec<(String, String)> = manifest
            .components
            .iter()
            .map(|component| {
                (
                    component.logical_path.as_str().to_owned(),
                    component.content.sha256.to_hex(),
                )
            })
            .collect();
        named.sort();
        Ok((named != catalogued).then(|| {
            problem(
                IntegrityProblemKind::ManifestMismatch,
                None,
                &format!(
                    "the manifest names {} components and the catalogue {}, or other ones",
                    named.len(),
                    catalogued.len()
                ),
            )
        }))
    }
}

/// Where a check's walk is.
struct Walk {
    objects_checked: i64,
    bytes_checked: i64,
    last: Option<String>,
    /// Problems found since the last write, to keep.
    pending: Vec<Value>,
    /// Problems found since the last write, kept or not.
    pending_count: i32,
    /// Problems found by the check in all, kept or not.
    found: usize,
    written_at: Instant,
}

impl Walk {
    /// Count a problem, and keep it while the check keeps fewer than
    /// [`MAX_KEPT_PROBLEMS`].
    fn found(&mut self, found: Value) {
        if self.found < MAX_KEPT_PROBLEMS {
            self.pending.push(found);
        }
        self.found += 1;
        self.pending_count = self.pending_count.saturating_add(1);
    }
}

fn lease_seconds(duration: Duration) -> i64 {
    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}
