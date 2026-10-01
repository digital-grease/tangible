// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The background half of importing.
//!
//! An import route accepts bytes and records a job; this drains that queue.
//! They are separate because the work is long (hashing and inspecting a
//! Blu-ray image takes minutes), and a request that held a connection open for
//! it would fail on the first proxy timeout, taking the import with it.
//!
//! The queue discipline is the same as the burn queue's, and for the same
//! reasons: claims take a lease with `FOR UPDATE SKIP LOCKED`, so two server
//! processes never work the same import, and a process that dies leaves work
//! that becomes claimable again when its lease lapses rather than work nobody
//! will ever touch.
//!
//! Failures are separated into the ones worth trying again and the ones that
//! never will be. A retryable failure is held off with a backoff rather than
//! retried immediately, because an import that fails in milliseconds would
//! otherwise spin as fast as the database can answer.

use std::time::Duration;

use tangible_db::repositories::{
    ImportJobRecord, claim_next_import_job, complete_import_job, fail_import_job,
    record_import_progress,
};
use tangible_db::{Database, DbError};
use tangible_domain::{ImportState, LogicalPath};
use tangible_storage::{IngestLimits, ManifestStoreError, StagingError, StorageError, WatchRoots};

use crate::catalog;
use crate::import::{ImportCheckpoint, ImportError, ImportPipeline, ImportRequest};

/// How the runner behaves.
#[derive(Debug, Clone)]
pub struct ImportRunnerSettings {
    /// How long a claim is held before another process may take the work.
    ///
    /// Long enough to cover hashing a large image, because a lease that
    /// lapses mid-import invites a second process to start the same work.
    pub lease: Duration,
    /// How long to wait when the queue is empty.
    pub poll_interval: Duration,
    /// How long a retryable failure is held before it is offered again.
    pub retry_backoff: Duration,
    /// What to call this process in the lease.
    pub owner: String,
    /// Limits applied when promoting staged bytes into the object store.
    pub limits: IngestLimits,
    /// Directories an administrator has said may be imported from.
    pub roots: WatchRoots,
}

impl Default for ImportRunnerSettings {
    fn default() -> Self {
        Self {
            lease: Duration::from_secs(30 * 60),
            poll_interval: Duration::from_secs(2),
            retry_backoff: Duration::from_secs(30),
            owner: "tangible-server".to_owned(),
            limits: IngestLimits::default(),
            // None configured is the safe default: with no roots, no
            // watched-folder import can name a path at all.
            roots: WatchRoots::default(),
        }
    }
}

/// Drains the import queue.
#[derive(Debug, Clone)]
pub struct ImportRunner {
    database: Database,
    pipeline: ImportPipeline,
    settings: ImportRunnerSettings,
}

impl ImportRunner {
    /// Assemble a runner.
    #[must_use]
    pub const fn new(
        database: Database,
        pipeline: ImportPipeline,
        settings: ImportRunnerSettings,
    ) -> Self {
        Self {
            database,
            pipeline,
            settings,
        }
    }

    /// Work the queue until `shutdown` resolves.
    ///
    /// An import already running finishes first. Abandoning it would leave
    /// staged bytes and a lease behind for no gain, and the work is
    /// resumable rather than instant.
    pub async fn run<S>(self, shutdown: S)
    where
        S: Future<Output = ()> + Send,
    {
        let mut shutdown = Box::pin(shutdown);
        loop {
            let step = self.step();
            tokio::select! {
                () = &mut shutdown => {
                    tracing::info!("import runner stopping");
                    return;
                }
                worked = step => {
                    let idle = match worked {
                        Ok(worked) => !worked,
                        Err(error) => {
                            tracing::error!(error = ?error, "the import queue could not be read");
                            true
                        }
                    };
                    if idle {
                        tokio::select! {
                            () = &mut shutdown => return,
                            () = tokio::time::sleep(self.settings.poll_interval) => {}
                        }
                    }
                }
            }
        }
    }

    /// Claim one import and run it. Returns whether there was any.
    ///
    /// # Errors
    ///
    /// [`DbError`] if the queue itself cannot be read. An import that fails is
    /// recorded against the job rather than returned here: the job is the
    /// place an operator looks.
    pub async fn step(&self) -> Result<bool, DbError> {
        let Some(job) = claim_next_import_job(
            self.database.pool(),
            &self.settings.owner,
            lease_seconds(self.settings.lease),
        )
        .await?
        else {
            return Ok(false);
        };

        tracing::info!(import_id = %job.id, source = %job.source_kind, "import claimed");
        self.execute(job).await;
        Ok(true)
    }

    /// Run one import through the pipeline and record what happened.
    async fn execute(&self, job: ImportJobRecord) {
        let pool = self.database.pool();

        // The checkpoint is what makes a resumed import cheap: it says which
        // components are already in the object store, so they are not hashed
        // and promoted twice.
        let checkpoint = job
            .checkpoint
            .clone()
            .and_then(|value| serde_json::from_value::<ImportCheckpoint>(value).ok());
        let resuming = checkpoint.is_some();
        let mut checkpoint = checkpoint.unwrap_or_default();

        if resuming {
            tracing::info!(
                import_id = %job.id,
                stage = %checkpoint.state,
                "resuming an import from its checkpoint"
            );
        }

        let request = ImportRequest {
            import_id: job.id,
            source_kind: job.source_kind.clone(),
            source_filename: descriptor_string(&job.source_descriptor, "filename"),
            source_reference: descriptor_string(&job.source_descriptor, "reference"),
            limits: self.settings.limits,
        };

        // Bytes first, for sources that have not delivered them yet. An
        // upload arrives already staged; a watched folder has to be copied in,
        // because the operator's directory is not the staging area and an
        // import must never consume the original in place.
        if job.source_kind == "watch_folder" && checkpoint.state == ImportState::Requested {
            self.record(&job, ImportState::Acquiring, &checkpoint).await;
            if let Err((code, detail, retryable)) = self.acquire(&job).await {
                tracing::warn!(import_id = %job.id, code, detail, "acquisition failed");
                self.fail(&job, retryable, code, &detail).await;
                return;
            }
        }

        // Say that work has begun before doing any, so a queue an operator is
        // watching moves rather than sitting at "requested" for a minute.
        self.record(&job, ImportState::Hashing, &checkpoint).await;

        let outcome = self.pipeline.run(&request, &mut checkpoint).await;

        // Written whichever way it went: a failure that discarded the
        // checkpoint would make the retry redo work that succeeded.
        self.record(&job, progress_state(checkpoint.state), &checkpoint)
            .await;

        match outcome {
            Ok(outcome) => {
                // The manifest is the durable record; the catalog is what a
                // burn job can reference. An artifact in one and not the
                // other is visible in the library and impossible to burn.
                match self.pipeline.manifests().read(outcome.artifact_id).await {
                    Ok(manifest) => {
                        if let Err(error) =
                            catalog::register_manifest(&self.database, &manifest).await
                        {
                            tracing::error!(
                                import_id = %job.id,
                                error = ?error,
                                "the artifact was stored but could not be catalogued"
                            );
                            self.fail(&job, true, "CATALOG_WRITE_FAILED", &error.to_string())
                                .await;
                            return;
                        }
                    }
                    Err(error) => {
                        tracing::error!(
                            import_id = %job.id,
                            error = ?error,
                            "the published manifest could not be read back"
                        );
                        self.fail(&job, true, "MANIFEST_UNREADABLE", &error.to_string())
                            .await;
                        return;
                    }
                }

                match complete_import_job(pool, job.id, *outcome.artifact_id.as_uuid()).await {
                    Ok(_) => tracing::info!(
                        import_id = %job.id,
                        artifact_id = %outcome.artifact_id,
                        "import complete"
                    ),
                    Err(error) => tracing::error!(
                        import_id = %job.id,
                        error = ?error,
                        "the import finished but could not be marked complete"
                    ),
                }
            }
            Err(error) => {
                let retryable = is_retryable(&error);
                tracing::warn!(
                    import_id = %job.id,
                    retryable,
                    error = ?error,
                    "import failed"
                );
                self.fail(&job, retryable, failure_code(&error), &error.to_string())
                    .await;
            }
        }
    }

    /// Copy a watched-folder source into the job's staging area.
    ///
    /// Copied rather than moved or linked. The operator's folder is theirs:
    /// an import that renamed the file out of it would be mutating the
    /// original, which is the one thing this system promises never to do.
    ///
    /// Returns a code, a reason, and whether trying again could help.
    async fn acquire(&self, job: &ImportJobRecord) -> Result<(), (&'static str, String, bool)> {
        let path_id = descriptor_string(&job.source_descriptor, "path_id").ok_or((
            "IMPORT_DESCRIPTOR_INVALID",
            "no path_id".to_owned(),
            false,
        ))?;
        let relative = descriptor_string(&job.source_descriptor, "relative_path").ok_or((
            "IMPORT_DESCRIPTOR_INVALID",
            "no relative_path".to_owned(),
            false,
        ))?;

        let source = self
            .settings
            .roots
            .resolve(&path_id, &relative)
            .await
            .map_err(|error| {
                let retryable = matches!(error, tangible_storage::WatchRootError::Unreadable);
                ("IMPORT_SOURCE_UNUSABLE", error.to_string(), retryable)
            })?;

        // The staged name is the source's filename, not its path: the staging
        // area is flat for a single-file import, and the logical path is
        // validated before anything is written.
        let filename = source.file_name().and_then(|name| name.to_str()).ok_or((
            "IMPORT_SOURCE_UNUSABLE",
            "the file has no usable name".to_owned(),
            false,
        ))?;
        let logical = LogicalPath::parse(filename).map_err(|error| {
            (
                "IMPORT_SOURCE_UNUSABLE",
                format!("the filename is not a usable logical path: {error}"),
                false,
            )
        })?;

        let area = self
            .pipeline
            .open_area(job.id)
            .await
            .map_err(|error| ("IMPORT_STAGING_FAILED", error.to_string(), true))?;
        let destination = area
            .prepare_parent(&logical)
            .await
            .map_err(|error| ("IMPORT_STAGING_FAILED", error.to_string(), true))?;

        let copied = tokio::fs::copy(&source, &destination)
            .await
            .map_err(|error| ("IMPORT_COPY_FAILED", error.to_string(), true))?;

        tracing::info!(
            import_id = %job.id,
            bytes = copied,
            "copied a watched file into staging"
        );

        if let Err(error) = record_import_progress(
            self.database.pool(),
            job.id,
            ImportState::Staged,
            None,
            None,
            i64::try_from(copied).ok(),
            lease_seconds(self.settings.lease),
        )
        .await
        {
            tracing::warn!(import_id = %job.id, error = ?error, "could not record acquisition");
        }
        Ok(())
    }

    async fn record(
        &self,
        job: &ImportJobRecord,
        state: ImportState,
        checkpoint: &ImportCheckpoint,
    ) {
        let json = serde_json::to_value(checkpoint).ok();
        let resume = resume_stage(checkpoint.state);
        if let Err(error) = record_import_progress(
            self.database.pool(),
            job.id,
            state,
            json.as_ref(),
            resume,
            None,
            lease_seconds(self.settings.lease),
        )
        .await
        {
            // Not fatal to the import: the work is still going, and the next
            // write may succeed. It does mean a crash would lose the
            // checkpoint, which the retry pays for by redoing a stage.
            tracing::warn!(import_id = %job.id, error = ?error, "could not record progress");
        }
    }

    async fn fail(&self, job: &ImportJobRecord, retryable: bool, code: &str, detail: &str) {
        if let Err(error) = fail_import_job(
            self.database.pool(),
            job.id,
            retryable,
            code,
            detail,
            lease_seconds(self.settings.retry_backoff),
        )
        .await
        {
            tracing::error!(import_id = %job.id, error = ?error, "could not record the failure");
        }
    }
}

fn lease_seconds(duration: Duration) -> i64 {
    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}

/// Read a string out of a source descriptor, if it has one.
fn descriptor_string(descriptor: &serde_json::Value, key: &str) -> Option<String> {
    descriptor
        .get(key)
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
}

/// The state a progress write may record for a checkpoint.
///
/// A pipeline that has finished leaves its checkpoint at `Complete`, but the
/// job is not complete until the catalog has the artifact and the artifact is
/// attached, which only `complete_import_job` does, and it writes both at
/// once because the schema refuses a completed import without an artifact.
/// Recording `Complete` here was refused by that constraint on every
/// successful import, which the first end-to-end run's logs showed, and the
/// refused write also discarded the checkpoint. Until then the job is still
/// registering, so that is what is recorded.
fn progress_state(checkpoint: ImportState) -> ImportState {
    match checkpoint {
        ImportState::Complete => ImportState::Registering,
        other => other,
    }
}

/// Which stage a retry should re-enter at.
///
/// Only the stages the schema accepts as resumable. Anything else means the
/// import had not got far enough for resumption to save anything.
fn resume_stage(state: ImportState) -> Option<&'static str> {
    match state {
        ImportState::Hashing => Some("hashing"),
        ImportState::Inspecting => Some("inspecting"),
        ImportState::Registering | ImportState::Complete => Some("registering"),
        _ => None,
    }
}

/// Whether an import failure is worth trying again.
///
/// The distinction is whether anything could change. A filesystem that was
/// full may not be; an image that staged nothing will stage nothing next time
/// either, and retrying it forever would hide the real problem.
fn is_retryable(error: &ImportError) -> bool {
    match error {
        // Nothing arrived, so a retry imports the same nothing; and a refused
        // transition is a bug or a stale state rather than weather. Neither
        // improves by being tried again, and retrying forever would hide the
        // real problem behind an endlessly requeued job.
        ImportError::NothingStaged { .. } | ImportError::Transition(_) => false,
        ImportError::Staging(staging) => matches!(staging, StagingError::Io { .. }),
        ImportError::Storage(storage) => matches!(storage, StorageError::Io { .. }),
        ImportError::Manifest(manifest) => matches!(manifest, ManifestStoreError::Io { .. }),
        ImportError::ReadStaged { .. } => true,
    }
}

/// A stable code for an import failure.
fn failure_code(error: &ImportError) -> &'static str {
    match error {
        ImportError::NothingStaged { .. } => "IMPORT_NOTHING_STAGED",
        ImportError::Staging(_) => "IMPORT_STAGING_FAILED",
        ImportError::Storage(_) => "IMPORT_STORAGE_FAILED",
        ImportError::Manifest(_) => "IMPORT_MANIFEST_FAILED",
        ImportError::Transition(_) => "IMPORT_TRANSITION_REFUSED",
        ImportError::ReadStaged { .. } => "IMPORT_READ_FAILED",
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_finished_pipeline_is_recorded_as_registering_until_the_artifact_is_attached() {
        // Regression: recording `Complete` here broke the schema's rule that a
        // complete import has an artifact, on every successful import.
        assert_eq!(
            progress_state(ImportState::Complete),
            ImportState::Registering
        );
        assert_eq!(progress_state(ImportState::Hashing), ImportState::Hashing);
        assert_eq!(
            progress_state(ImportState::Registering),
            ImportState::Registering
        );
    }

    #[test]
    fn an_empty_import_is_not_retried_forever() {
        // Nothing staged means nothing will stage. Retrying would hide the
        // real problem behind an endlessly requeued job.
        assert!(!is_retryable(&ImportError::NothingStaged {
            import_id: "x".to_owned()
        }));
    }

    #[test]
    fn a_read_failure_is_worth_another_go() {
        assert!(is_retryable(&ImportError::ReadStaged {
            path: "x".to_owned(),
            source: std::io::Error::other("disk hiccup"),
        }));
    }

    #[test]
    fn every_failure_has_its_own_code() {
        let errors = [
            ImportError::NothingStaged {
                import_id: "x".to_owned(),
            },
            ImportError::ReadStaged {
                path: "x".to_owned(),
                source: std::io::Error::other("x"),
            },
        ];
        let mut codes = std::collections::BTreeSet::new();
        for error in errors {
            assert!(codes.insert(failure_code(&error)));
        }
    }

    #[test]
    fn only_stages_the_schema_accepts_are_offered_as_resume_points() {
        // The column has a CHECK listing exactly these. A resume stage it
        // refuses would fail the update rather than the review.
        let permitted = ["requested", "hashing", "inspecting", "registering"];
        for state in ImportState::all() {
            if let Some(stage) = resume_stage(*state) {
                assert!(permitted.contains(&stage), "{stage} is not resumable");
            }
        }
    }

    #[test]
    fn an_import_that_has_not_started_has_nothing_to_resume_from() {
        assert_eq!(resume_stage(ImportState::Requested), None);
        assert_eq!(resume_stage(ImportState::Staged), None);
    }

    #[test]
    fn a_descriptor_without_the_key_is_not_an_error() {
        let descriptor = serde_json::json!({ "filename": "disc.iso" });
        assert_eq!(
            descriptor_string(&descriptor, "filename").as_deref(),
            Some("disc.iso")
        );
        assert_eq!(descriptor_string(&descriptor, "reference"), None);
        assert_eq!(descriptor_string(&serde_json::json!(7), "filename"), None);
    }

    #[test]
    fn the_lease_outlasts_the_poll_interval_by_a_wide_margin() {
        // A lease that lapsed while an import was running would invite a
        // second process to start the same work.
        let settings = ImportRunnerSettings::default();
        assert!(settings.lease > settings.poll_interval * 100);
        assert!(settings.retry_backoff > settings.poll_interval);
    }
}
