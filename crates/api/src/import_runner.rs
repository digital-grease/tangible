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
//!
//! Three sources have bytes to fetch before anything else can happen: a
//! watched folder, copied in; a URL (ADR 0016), fetched through the
//! outbound client and resumed after an interruption from what is already in
//! staging; and a torrent handed to qBittorrent, followed by infohash until
//! it finishes and then copied in from the watched root its download folder
//! is mounted at. A torrent still downloading holds its import in
//! `acquiring` with a short lease and no process behind it, so the queue
//! comes back to it without anything waiting in memory.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use tangible_db::integrations::{
    get_integration, record_integration_health, record_integration_job_content,
    record_integration_job_state,
};
use tangible_db::repositories::{
    ImportJobRecord, claim_next_import_job, complete_import_job, fail_import_job,
    record_import_expected, record_import_progress, wait_for_import_source,
};
use tangible_db::{Database, DbError};
use tangible_domain::{ImportState, LogicalPath};
use tangible_integrations::download::{Phase, relative_to};
use tangible_integrations::fetch::{Fetched, Progress, Resume, fetch_to_file};
use tangible_integrations::outbound::{OutboundClient, Url};
use tangible_storage::{IngestLimits, ManifestStoreError, StagingError, StorageError, WatchRoots};
use tokio::io::AsyncReadExt as _;

use crate::catalog;
use crate::download_clients::{open_client, open_s3};
use crate::import::{
    DownloadCheckpoint, ImportCheckpoint, ImportError, ImportPipeline, ImportRequest,
};
use crate::secrets::SecretKey;
use crate::source_removal::SourceRemovalHandle;

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
    /// What URL imports fetch with, when they are turned on. `None` fails
    /// any URL import still queued from when they were on.
    pub outbound: Option<OutboundClient>,
    /// The largest file a URL import fetches.
    pub max_download_bytes: u64,
    /// What download clients are reached with. `None` holds any torrent
    /// import until the server has one.
    pub integrations: Option<OutboundClient>,
    /// The key a download client's password is sealed with.
    pub secret_key: Option<Arc<SecretKey>>,
    /// How often a torrent still downloading is asked after.
    pub torrent_poll: Duration,
    /// The remover to wake when an import completes, which may have queued
    /// its source's removal (docs/plans/source-retention.md).
    pub source_removal: Option<SourceRemovalHandle>,
    /// The identification runner to wake when an import completes, so the
    /// new artifact's preservation hashes are computed at once.
    pub identification: Option<crate::identification::IdentificationHandle>,
}

impl Default for ImportRunnerSettings {
    fn default() -> Self {
        Self {
            lease: Duration::from_mins(30),
            poll_interval: Duration::from_secs(2),
            retry_backoff: Duration::from_secs(30),
            owner: "tangible-server".to_owned(),
            limits: IngestLimits::default(),
            // None configured is the safe default: with no roots, no
            // watched-folder import can name a path at all.
            roots: WatchRoots::default(),
            outbound: None,
            max_download_bytes: 64 * 1024 * 1024 * 1024,
            integrations: None,
            secret_key: None,
            torrent_poll: Duration::from_secs(30),
            source_removal: None,
            identification: None,
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
        crate::background::run_steps(
            shutdown,
            || async {
                match self.step().await {
                    Ok(worked) => worked,
                    Err(error) => {
                        tracing::error!(error = ?error, "the import queue could not be read");
                        false
                    }
                }
            },
            || tokio::time::sleep(self.settings.poll_interval),
        )
        .await;
        tracing::info!("import runner stopped");
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
            self.settle_rips().await;
            self.expire_uploads().await;
            return Ok(false);
        };

        tracing::info!(import_id = %job.id, source = %job.source_kind, "import claimed");
        self.execute(job).await;
        self.settle_rips().await;
        Ok(true)
    }

    /// Move rips on whose import has ended. Checked after every import and
    /// whenever the queue is empty, so a server restarted mid-import still
    /// catches up.
    async fn settle_rips(&self) {
        match tangible_db::rips::settle_rips(self.database.pool()).await {
            Ok(moved) if !moved.is_empty() => {
                tracing::info!(rips = ?moved, "rips followed their imports to the end");
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(error = ?error, "could not settle rips"),
        }
    }

    /// Abandon uploads left idle past their expiry and remove what they sent.
    async fn expire_uploads(&self) {
        match tangible_db::uploads::expire_uploads(self.database.pool()).await {
            Ok(expired) => {
                for id in expired {
                    let id = tangible_domain::ImportJobId::from_uuid(id);
                    if let Err(error) = self.pipeline.discard(id).await {
                        tracing::warn!(upload_id = %id, error = ?error, "could not remove an expired upload");
                    } else {
                        tracing::info!(upload_id = %id, "an idle upload expired and was removed");
                    }
                }
            }
            Err(error) => tracing::warn!(error = ?error, "could not expire idle uploads"),
        }
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
            // Written by the server when it queued a rip's import; a
            // descriptor that does not parse is a server bug, and the
            // manifest then simply lacks the record rather than failing.
            physical_dump: parsed(&job.source_descriptor, "physical_dump"),
            lineage: parsed(&job.source_descriptor, "lineage"),
        };

        // Bytes first, for sources that have not delivered them yet. An
        // upload arrives already staged; a watched folder has to be copied in,
        // because the operator's directory is not the staging area and an
        // import must never consume the original in place.
        if checkpoint.state == ImportState::Requested
            && !self.acquire_first(&job, &mut checkpoint).await
        {
            return;
        }

        // Say that work has begun before doing any, so a queue an operator is
        // watching moves rather than sitting at "requested" for a minute.
        self.record(&job, ImportState::Hashing, &checkpoint).await;

        // Held until the artifact is registered: collection waits for it, so
        // an object this import finds already stored cannot be collected
        // before it is counted (ADR 0020).
        let _ingesting = self.pipeline.manifests().objects().ingesting().await;
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
                    Ok(_) => {
                        tracing::info!(
                            import_id = %job.id,
                            artifact_id = %outcome.artifact_id,
                            "import complete"
                        );
                        // Registered and recorded: the bytes are in the
                        // library, and the staged copy is disposable. Kept
                        // until now, and kept on any failure, because it is
                        // the evidence a bad import leaves.
                        if let Err(error) = self.pipeline.discard(job.id).await {
                            tracing::warn!(
                                import_id = %job.id,
                                error = ?error,
                                "the import's staging area could not be removed"
                            );
                        }
                        self.announce_completed();
                    }
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

    /// Wake what follows a completed import: the remover, which may have a
    /// source to remove, and the identification runner, which has a new
    /// artifact to hash.
    fn announce_completed(&self) {
        if let Some(remover) = &self.settings.source_removal {
            remover.nudge();
        }
        if let Some(identification) = &self.settings.identification {
            identification.nudge();
        }
    }

    /// Bring the source's bytes in, answering whether to go on: not when the
    /// import is held for its source, or failed, which is recorded here.
    async fn acquire_first(
        &self,
        job: &ImportJobRecord,
        checkpoint: &mut ImportCheckpoint,
    ) -> bool {
        match self.bring_in(job, checkpoint).await {
            Ok(Acquired::Ready) => true,
            // Held until the source is ready; the queue comes back to it.
            Ok(Acquired::Waiting) => false,
            Err((code, detail, retryable)) => {
                tracing::warn!(import_id = %job.id, code, detail, "acquisition failed");
                self.fail(job, retryable, code, &detail).await;
                false
            }
        }
    }

    /// Bring a source's bytes into staging, for the sources that have not
    /// delivered them yet: a watched folder is copied, a URL fetched, a
    /// torrent followed until it finishes. An upload, or a rip, arrives
    /// already staged.
    async fn bring_in(
        &self,
        job: &ImportJobRecord,
        checkpoint: &mut ImportCheckpoint,
    ) -> Result<Acquired, Failure> {
        match job.source_kind.as_str() {
            "watch_folder" => {
                self.record(job, ImportState::Acquiring, checkpoint).await;
                self.acquire(job).await.map(|()| Acquired::Ready)
            }
            "http_url" => {
                self.record(job, ImportState::Acquiring, checkpoint).await;
                self.download(job, checkpoint)
                    .await
                    .map(|()| Acquired::Ready)
            }
            "qbittorrent_job" | "sabnzbd_job" | "nzbget_job" => self.follow_download(job).await,
            "s3_object" => {
                self.record(job, ImportState::Acquiring, checkpoint).await;
                self.download_s3(job, checkpoint)
                    .await
                    .map(|()| Acquired::Ready)
            }
            // An upload sent in parts may name the length and digest its
            // sender expects; checked before anything is hashed or kept.
            "upload" if job.source_descriptor.get("expected_bytes").is_some() => {
                self.check_staged(job).await.map(|()| Acquired::Ready)
            }
            _ => Ok(Acquired::Ready),
        }
    }

    /// Ask the client after its job: a torrent by infohash, a SABnzbd or
    /// NZBGet job by its id. Still downloading, or the client unreachable or
    /// misconfigured: held, and asked again in a while, saying why when
    /// something is wrong. Finished: its path is mapped from the client's
    /// download folder onto the watched root it is mounted at, and copied in
    /// like a watched folder. The client's own failure, a job gone from it,
    /// or a path outside the folder end the import.
    async fn follow_download(&self, job: &ImportJobRecord) -> Result<Acquired, Failure> {
        let invalid = |what: &str| ("IMPORT_DESCRIPTOR_INVALID", what.to_owned(), false);
        let integration_id: uuid::Uuid = parsed(&job.source_descriptor, "integration_id")
            .ok_or_else(|| invalid("no integration_id"))?;
        // `infohash` is where the first torrent imports kept it.
        let client_job_id = descriptor_string(&job.source_descriptor, "client_job_id")
            .or_else(|| descriptor_string(&job.source_descriptor, "infohash"))
            .ok_or_else(|| invalid("no client_job_id"))?;
        let pool = self.database.pool();
        let record = get_integration(pool, integration_id)
            .await
            .map_err(|error| ("STORAGE_UNAVAILABLE", error.to_string(), true))?
            .ok_or_else(|| {
                (
                    "INTEGRATION_MISSING",
                    "the integration this job was handed to is gone".to_owned(),
                    false,
                )
            })?;
        let opened = open_client(
            &self.database,
            self.settings.integrations.as_ref(),
            self.settings.secret_key.as_deref(),
            &record,
        )
        .await;
        let (mut client, folders) = match opened {
            Ok(opened) => opened,
            // An administrator can put it right; until then the import waits.
            Err(error) => {
                return Ok(self
                    .hold(job, None, None, Some((error.code, &error.detail)))
                    .await);
            }
        };
        let name = client.name();
        let status = match client.job(&client_job_id).await {
            Ok(status) => {
                let _ = record_integration_health(pool, integration_id, Ok(None)).await;
                status
            }
            Err(error) => {
                let _ = record_integration_health(pool, integration_id, Err(error.code())).await;
                let detail = error.to_string();
                return Ok(self
                    .hold(job, None, None, Some((error.code(), &detail)))
                    .await);
            }
        };
        let Some(status) = status else {
            // A job added a moment ago may not be listed yet.
            let age = time::OffsetDateTime::now_utc() - job.created_at;
            if age < time::Duration::minutes(2) {
                return Ok(self.hold(job, None, None, None).await);
            }
            return Err((
                "INTEGRATION_JOB_MISSING",
                format!("{name} no longer has this job; it may have been removed there"),
                false,
            ));
        };
        if let Err(error) =
            record_integration_job_state(pool, *job.id.as_uuid(), &status.state).await
        {
            tracing::warn!(import_id = %job.id, error = ?error, "could not record the job's state");
        }
        match status.phase {
            Phase::Downloading { done, total } => Ok(self.hold(job, done, total, None).await),
            Phase::Failed(state) => Err((
                "INTEGRATION_JOB_FAILED",
                format!("{name} reports the job as {state}; put it right there, then retry"),
                false,
            )),
            Phase::Complete { content_path, size } => {
                let Some(relative) = relative_to(&folders.client_path, &content_path) else {
                    return Err((
                        "INTEGRATION_PATH_OUTSIDE_ROOT",
                        format!(
                            "{name} keeps this job outside the integration's download folder \
                             (client_path), so it cannot be read through the watched root"
                        ),
                        false,
                    ));
                };
                tracing::info!(import_id = %job.id, client = name, "the download finished; copying it in");
                let _ = record_import_expected(pool, job.id, size).await;
                // What a removal acts on, if the integration removes after
                // import: this path, not wherever the client says it is later.
                if let Err(error) =
                    record_integration_job_content(pool, *job.id.as_uuid(), &relative).await
                {
                    tracing::warn!(import_id = %job.id, error = ?error, "could not record where the job is");
                }
                self.copy_from_root(job, &folders.local_root, &relative)
                    .await
                    .map(|()| Acquired::Ready)
            }
        }
    }

    /// Hold an import for its source, writing what progress is known and
    /// why, if something is wrong.
    async fn hold(
        &self,
        job: &ImportJobRecord,
        done: Option<i64>,
        total: Option<i64>,
        problem: Option<(&str, &str)>,
    ) -> Acquired {
        if let Some((code, detail)) = problem {
            tracing::info!(import_id = %job.id, code, detail, "holding a torrent import");
        }
        if let Err(error) = wait_for_import_source(
            self.database.pool(),
            job.id,
            done,
            total,
            lease_seconds(self.settings.torrent_poll),
            problem,
        )
        .await
        {
            tracing::warn!(import_id = %job.id, error = ?error, "could not hold the import");
        }
        Acquired::Waiting
    }

    /// Check a staged upload against what its sender said it would be.
    async fn check_staged(&self, job: &ImportJobRecord) -> Result<(), Failure> {
        let filename = descriptor_string(&job.source_descriptor, "filename").ok_or((
            "IMPORT_DESCRIPTOR_INVALID",
            "no filename".to_owned(),
            false,
        ))?;
        let logical = LogicalPath::parse(&filename).map_err(|_| {
            (
                "IMPORT_DESCRIPTOR_INVALID",
                "an unusable filename".to_owned(),
                false,
            )
        })?;
        let area = self
            .pipeline
            .open_area(job.id)
            .await
            .map_err(|error| ("IMPORT_STAGING_FAILED", error.to_string(), true))?;
        let path = area
            .prepare_parent(&logical)
            .await
            .map_err(|error| ("IMPORT_STAGING_FAILED", error.to_string(), true))?;
        let bytes = tokio::fs::metadata(&path).await.map_or(0, |m| m.len());
        check_expected(job, &path, bytes).await
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
        self.copy_from_root(job, &path_id, &relative).await
    }

    /// Copy a file or directory beneath a watched root into the job's
    /// staging area.
    async fn copy_from_root(
        &self,
        job: &ImportJobRecord,
        path_id: &str,
        relative: &str,
    ) -> Result<(), Failure> {
        let source = self
            .settings
            .roots
            .resolve_source(path_id, relative)
            .await
            .map_err(|error| {
                let retryable = matches!(error, tangible_storage::WatchRootError::Unreadable);
                ("IMPORT_SOURCE_UNUSABLE", error.to_string(), retryable)
            })?;

        // A file is staged under its own name, flat. A directory's files keep
        // their paths within it, so a sheet that names `tracks/01.bin` finds
        // it. Every logical path is validated before anything is written.
        // Each is then read by its path beneath the root, opened a name at a
        // time without following a link: what was found a moment ago is not
        // trusted to be what is there now.
        let files: Vec<(String, String)> = match source {
            tangible_storage::WatchedSource::File(path) => {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or((
                        "IMPORT_SOURCE_UNUSABLE",
                        "the file has no usable name".to_owned(),
                        false,
                    ))?
                    .to_owned();
                vec![(name, relative.to_owned())]
            }
            tangible_storage::WatchedSource::Directory { files } => {
                let relative = relative.trim_end_matches('/');
                files
                    .into_iter()
                    .map(|(inside, _)| {
                        let beneath = format!("{relative}/{inside}");
                        (inside, beneath)
                    })
                    .collect()
            }
        };

        let area = self
            .pipeline
            .open_area(job.id)
            .await
            .map_err(|error| ("IMPORT_STAGING_FAILED", error.to_string(), true))?;
        let mut copied = 0_u64;
        for (name, beneath) in &files {
            let logical = LogicalPath::parse(name).map_err(|error| {
                (
                    "IMPORT_SOURCE_UNUSABLE",
                    format!("{name} is not a usable logical path: {error}"),
                    false,
                )
            })?;
            let destination = area
                .prepare_parent(&logical)
                .await
                .map_err(|error| ("IMPORT_STAGING_FAILED", error.to_string(), true))?;
            copied = copied.saturating_add(
                self.settings
                    .roots
                    .copy_file(path_id, beneath, &destination)
                    .await
                    .map_err(|error| match error {
                        tangible_storage::WatchRootError::Unreadable => {
                            ("IMPORT_COPY_FAILED", error.to_string(), true)
                        }
                        refused => ("IMPORT_SOURCE_UNUSABLE", refused.to_string(), false),
                    })?,
            );
        }

        tracing::info!(
            import_id = %job.id,
            bytes = copied,
            files = files.len(),
            "copied a watched source into staging"
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

    /// Fetch a URL import's file into its staging area, resuming from what
    /// an earlier attempt left there, then check it against the length and
    /// SHA-256 the operator gave, if they gave them.
    ///
    /// The URL itself is never logged or put in a failure's detail: a signed
    /// URL's secret is in its query.
    async fn download(
        &self,
        job: &ImportJobRecord,
        checkpoint: &mut ImportCheckpoint,
    ) -> Result<(), Failure> {
        let invalid = |what: &str| ("IMPORT_DESCRIPTOR_INVALID", what.to_owned(), false);
        let Some(client) = &self.settings.outbound else {
            return Err((
                "URL_IMPORTS_OFF",
                "importing from a URL is turned off on this server".to_owned(),
                false,
            ));
        };
        let url = descriptor_string(&job.source_descriptor, "url")
            .and_then(|text| Url::parse(&text).ok())
            .ok_or_else(|| invalid("no usable url"))?;
        self.fetch_into_staging(job, checkpoint, client, &url).await
    }

    /// Fetch an S3 object into its import's staging area: a presigned GET,
    /// made now and pinned to the version recorded when the import was
    /// asked for, through the integrations client. Resumed, checked and
    /// recorded exactly as a URL import.
    async fn download_s3(
        &self,
        job: &ImportJobRecord,
        checkpoint: &mut ImportCheckpoint,
    ) -> Result<(), Failure> {
        let invalid = |what: &str| ("IMPORT_DESCRIPTOR_INVALID", what.to_owned(), false);
        let integration_id: uuid::Uuid = parsed(&job.source_descriptor, "integration_id")
            .ok_or_else(|| invalid("no integration_id"))?;
        let bucket = descriptor_string(&job.source_descriptor, "bucket")
            .ok_or_else(|| invalid("no bucket"))?;
        let key =
            descriptor_string(&job.source_descriptor, "key").ok_or_else(|| invalid("no key"))?;
        let version = descriptor_string(&job.source_descriptor, "version_id");
        let record = get_integration(self.database.pool(), integration_id)
            .await
            .map_err(|error| ("STORAGE_UNAVAILABLE", error.to_string(), true))?
            .ok_or_else(|| {
                (
                    "INTEGRATION_MISSING",
                    "the S3 integration this import names is gone".to_owned(),
                    false,
                )
            })?;
        let Some(http) = self.settings.integrations.as_ref() else {
            return Err((
                "INTEGRATION_OFF",
                "this server reaches no integrations".to_owned(),
                true,
            ));
        };
        let (client, settings) = open_s3(
            &self.database,
            Some(http),
            self.settings.secret_key.as_deref(),
            &record,
        )
        .await
        .map_err(|error| (error.code, error.detail, true))?;
        if !settings.buckets.contains(&bucket) {
            return Err((
                "INTEGRATION_MISCONFIGURED",
                format!("the integration no longer allows bucket {bucket}"),
                false,
            ));
        }
        let url = client
            .presigned_get(&bucket, &key, version.as_deref())
            .map_err(|error| (error.code(), error.to_string(), false))?;
        self.fetch_into_staging(job, checkpoint, http, &url).await
    }

    /// Fetch `url` with `client` into the import's staging area under its
    /// recorded filename, resuming from what an earlier attempt left there,
    /// then check it against the length and SHA-256 it should have.
    async fn fetch_into_staging(
        &self,
        job: &ImportJobRecord,
        checkpoint: &mut ImportCheckpoint,
        client: &OutboundClient,
        url: &Url,
    ) -> Result<(), Failure> {
        let invalid = |what: &str| ("IMPORT_DESCRIPTOR_INVALID", what.to_owned(), false);
        let filename = descriptor_string(&job.source_descriptor, "filename")
            .ok_or_else(|| invalid("no filename"))?;
        let logical = LogicalPath::parse(&filename).map_err(|_| invalid("an unusable filename"))?;
        let area = self
            .pipeline
            .open_area(job.id)
            .await
            .map_err(|error| ("IMPORT_STAGING_FAILED", error.to_string(), true))?;
        let path = area
            .prepare_parent(&logical)
            .await
            .map_err(|error| ("IMPORT_STAGING_FAILED", error.to_string(), true))?;

        let fetched = self
            .fetch_reporting(job, client, url, &path, checkpoint)
            .await?;
        checkpoint.download = Some(DownloadCheckpoint {
            validator: fetched.validator.clone(),
            final_reference: Some(fetched.final_reference.clone()),
            content_type: fetched.content_type.clone(),
        });
        check_expected(job, &path, fetched.bytes).await?;

        tracing::info!(
            import_id = %job.id,
            bytes = fetched.bytes,
            resumed_from = fetched.resumed_from,
            source = %fetched.final_reference,
            "fetched a URL import into staging"
        );
        self.record_fetch(job, checkpoint, ImportState::Staged, fetched.bytes)
            .await;
        Ok(())
    }

    /// Run the fetch, writing its progress and the validator a resume needs
    /// to the job every two seconds, and once more however it ends: a fetch
    /// cut off in its first seconds has not been reported yet.
    async fn fetch_reporting(
        &self,
        job: &ImportJobRecord,
        client: &OutboundClient,
        url: &Url,
        path: &std::path::Path,
        checkpoint: &mut ImportCheckpoint,
    ) -> Result<Fetched, Failure> {
        let offset = tokio::fs::metadata(path).await.map_or(0, |m| m.len());
        let resume = Resume {
            offset,
            validator: checkpoint
                .download
                .as_ref()
                .and_then(|d| d.validator.clone()),
        };
        if offset > 0 {
            tracing::info!(import_id = %job.id, offset, "resuming a URL import's fetch");
        }

        let latest: Arc<Mutex<Option<Snapshot>>> = Arc::new(Mutex::new(None));
        let reported = Arc::clone(&latest);
        let mut on_progress = move |progress: Progress<'_>| {
            if let Ok(mut latest) = reported.lock() {
                *latest = Some(Snapshot {
                    bytes: progress.bytes,
                    total: progress.total,
                    validator: progress.validator.map(str::to_owned),
                });
            }
        };
        let fetch = fetch_to_file(
            client,
            url,
            path,
            Some(&resume),
            self.settings.max_download_bytes,
            &mut on_progress,
        );
        tokio::pin!(fetch);
        let mut ticker = tokio::time::interval(Duration::from_secs(2));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut expected_recorded = false;
        let fetched = loop {
            tokio::select! {
                fetched = &mut fetch => break fetched,
                _ = ticker.tick() => {}
            }
            let snapshot = latest.lock().ok().and_then(|mut latest| latest.take());
            if let Some(snapshot) = snapshot {
                if let Some(total) = snapshot.total.and_then(|total| i64::try_from(total).ok())
                    && !expected_recorded
                {
                    expected_recorded = true;
                    let _ = record_import_expected(self.database.pool(), job.id, total).await;
                }
                // Kept in the checkpoint itself, not a copy: the snapshot is
                // taken here, so a fetch failing after this finds none, and
                // writing the checkpoint without the validator then lost
                // the resume (the retry fetched the whole file again).
                checkpoint.download = Some(DownloadCheckpoint {
                    validator: snapshot.validator,
                    ..checkpoint.download.take().unwrap_or_default()
                });
                self.record_fetch(job, checkpoint, ImportState::Acquiring, snapshot.bytes)
                    .await;
            }
        };
        match fetched {
            Ok(fetched) => Ok(fetched),
            Err(error) => {
                let snapshot = latest.lock().ok().and_then(|mut latest| latest.take());
                if let Some(snapshot) = snapshot {
                    checkpoint.download = Some(DownloadCheckpoint {
                        validator: snapshot.validator,
                        ..checkpoint.download.take().unwrap_or_default()
                    });
                }
                let kept = tokio::fs::metadata(path).await.map_or(0, |m| m.len());
                self.record_fetch(job, checkpoint, ImportState::Acquiring, kept)
                    .await;
                Err((error.code(), error.to_string(), error.retryable()))
            }
        }
    }

    /// Write a fetch's checkpoint and byte count to the job.
    async fn record_fetch(
        &self,
        job: &ImportJobRecord,
        checkpoint: &ImportCheckpoint,
        state: ImportState,
        bytes: u64,
    ) {
        let json = serde_json::to_value(checkpoint).ok();
        if let Err(error) = record_import_progress(
            self.database.pool(),
            job.id,
            state,
            json.as_ref(),
            None,
            i64::try_from(bytes).ok(),
            lease_seconds(self.settings.lease),
        )
        .await
        {
            tracing::warn!(import_id = %job.id, error = ?error, "could not record the fetch");
        }
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

/// Why a stage failed: a stable code, a reason, and whether trying again
/// could help.
type Failure = (&'static str, String, bool);

/// Whether a source's bytes are in staging.
enum Acquired {
    /// They are; go on.
    Ready,
    /// Not yet: the import is held, and the queue comes back to it.
    Waiting,
}

/// The latest progress a fetch reported.
struct Snapshot {
    bytes: u64,
    total: Option<u64>,
    validator: Option<String>,
}

/// Check a fetched file against the length and SHA-256 the operator gave,
/// removing it when it is not what they said, so a retry fetches it again.
async fn check_expected(
    job: &ImportJobRecord,
    path: &std::path::Path,
    bytes: u64,
) -> Result<(), Failure> {
    if let Some(expected) = job
        .source_descriptor
        .get("expected_bytes")
        .and_then(serde_json::Value::as_u64)
        && expected != bytes
    {
        let _ = tokio::fs::remove_file(path).await;
        return Err((
            "IMPORT_SIZE_MISMATCH",
            format!("expected {expected} bytes and received {bytes}"),
            false,
        ));
    }
    if let Some(expected) = descriptor_string(&job.source_descriptor, "expected_sha256") {
        let actual = sha256_of(path)
            .await
            .map_err(|error| ("IMPORT_STAGING_FAILED", error.to_string(), true))?;
        if actual != expected {
            let _ = tokio::fs::remove_file(path).await;
            return Err((
                "IMPORT_HASH_MISMATCH",
                format!("expected SHA-256 {expected} and received {actual}"),
                false,
            ));
        }
    }
    Ok(())
}

/// A staged file's SHA-256, in lowercase hex, read in pieces.
async fn sha256_of(path: &std::path::Path) -> std::io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn lease_seconds(duration: Duration) -> i64 {
    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}

/// A field of a job's descriptor, as the type the server wrote it as.
fn parsed<T: serde::de::DeserializeOwned>(descriptor: &serde_json::Value, key: &str) -> Option<T> {
    let value = descriptor.get(key)?;
    match serde_json::from_value(value.clone()) {
        Ok(parsed) => Some(parsed),
        Err(error) => {
            tracing::error!(%error, key, "an import's descriptor field could not be read");
            None
        }
    }
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
        // The archive is what it is: only the disk can do better next time.
        ImportError::Archive(archive) => {
            matches!(archive, tangible_image::archive::ArchiveError::Io { .. })
        }
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
        ImportError::Archive(archive) => archive.code(),
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
