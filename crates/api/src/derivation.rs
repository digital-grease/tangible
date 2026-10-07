// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Making derivatives in the background.
//!
//! A request becomes a job (see `tangible_db::derivations`); this module works
//! the jobs. For each one it copies the parent's files out of the
//! content-addressed store into a staging area, runs the engine there,
//! promotes the single output file into the store, writes the child's
//! manifest with its lineage, catalogues the child and records the
//! derivation. The parent is read, never written: no tool runs against a
//! canonical object, and the copy it does run against is discarded after.
//!
//! The queue discipline is the import queue's. A claim takes a lease, a
//! worker that dies leaves a job that becomes claimable again, and a
//! retryable failure is held off before it is offered again.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tangible_db::derivations::{
    CompletedDerivation, DerivationJobRecord, claim_next_derivation_job, complete_derivation_job,
    fail_derivation_job,
};
use tangible_db::{Database, DbError};
use tangible_domain::derivation::{
    ChdOptions, DerivationSpec, NotApplicable, OptionsError, content_digest,
};
use tangible_domain::enums::{
    ArtifactFormat, ArtifactKind, ArtifactOrigin, ComponentRole, MediaFamily, ValidationState,
};
use tangible_domain::manifest::{
    ArtifactManifest, BurnSupport, Classification, Compatibility, Component, ContentHashes,
    Detector, Extensions, Lineage, Origin, SCHEMA_ID, StorageRef, ToolRef, Topology, Validation,
};
use tangible_domain::{ArtifactId, ComponentId, LogicalPath, LossCharacter, Transformation};
use tangible_image::derive::{DerivationEngine, DeriveError, DeriveRequest};
use tangible_storage::{
    FilesystemStore, IngestLimits, ManifestStore, ManifestStoreError, StagingError, StagingKind,
    StagingManager,
};
use time::OffsetDateTime;
use tokio::sync::Notify;

use crate::catalog;

/// A derivation, checked and ready to queue.
#[derive(Debug, Clone)]
pub struct PlannedDerivation {
    /// What will be done, by which tool.
    pub spec: DerivationSpec,
    /// The options as stored and fingerprinted.
    pub options_json: Value,
    /// The fingerprint.
    pub fingerprint: String,
}

/// Why a derivation cannot be planned for a parent.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    /// No transformation suits the parent, and none was named.
    #[error("no transformation can be applied to a {format} image whose media family is {media}")]
    NothingSuits {
        /// The parent's format.
        format: ArtifactFormat,
        /// Its media family.
        media: MediaFamily,
    },
    /// The named transformation does not apply.
    #[error(transparent)]
    NotApplicable(#[from] NotApplicable),
    /// The options were refused.
    #[error(transparent)]
    Options(#[from] OptionsError),
    /// This server's engine cannot run the transformation.
    #[error("{0}")]
    Unsupported(String),
}

/// Why a job did not produce a derivative.
#[derive(Debug, thiserror::Error)]
pub enum DerivationError {
    /// The job no longer suits its parent, or its options are refused.
    #[error(transparent)]
    Plan(#[from] PlanError),
    /// The job was asked for under another tool version than the one this
    /// server runs, so its fingerprint would describe work never done.
    #[error(
        "the job was requested for {requested}, and this server runs {running}; ask for it again"
    )]
    ToolChanged {
        /// The version the job was fingerprinted for.
        requested: String,
        /// The version available.
        running: String,
    },
    /// The parent has no component the tool reads.
    #[error("the parent has no {0} for the tool to read")]
    NoInput(&'static str),
    /// The parent's manifest could not be read.
    #[error(transparent)]
    Manifest(#[from] ManifestStoreError),
    /// Staging failed.
    #[error(transparent)]
    Staging(#[from] StagingError),
    /// Copying a stored object for the tool failed, or it was not the size
    /// its manifest says.
    #[error("{0}")]
    Materialize(String),
    /// The engine failed.
    #[error(transparent)]
    Engine(#[from] DeriveError),
}

impl DerivationError {
    /// Whether the same job could succeed later.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::Plan(_) | Self::ToolChanged { .. } | Self::NoInput(_) => false,
            Self::Manifest(_) | Self::Staging(_) | Self::Materialize(_) => true,
            Self::Engine(error) => error.is_retryable(),
        }
    }

    /// The stable code for the job record.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Plan(_) => "DERIVATION_NOT_APPLICABLE",
            Self::ToolChanged { .. } => "DERIVATION_TOOL_CHANGED",
            Self::NoInput(_) => "DERIVATION_NO_INPUT",
            Self::Manifest(_) => "DERIVATION_PARENT_UNREADABLE",
            Self::Staging(_) => "DERIVATION_STAGING_FAILED",
            Self::Materialize(_) => "DERIVATION_MATERIALIZE_FAILED",
            Self::Engine(error) => error.code(),
        }
    }
}

/// What a finished job produced.
#[derive(Debug, Clone, Copy)]
pub struct DerivationOutcome {
    /// The derivative, its manifest published.
    pub child_artifact_id: ArtifactId,
    /// What it preserved.
    pub loss_character: LossCharacter,
}

/// Runs derivations.
#[derive(Clone)]
pub struct DerivationPipeline {
    staging: StagingManager,
    objects: FilesystemStore,
    manifests: ManifestStore,
    engine: Arc<dyn DerivationEngine>,
    limits: IngestLimits,
}

impl std::fmt::Debug for DerivationPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DerivationPipeline")
            .field("tool", &self.engine.tool_name())
            .field("version", &self.engine.tool_version())
            .finish_non_exhaustive()
    }
}

impl DerivationPipeline {
    /// Assemble a pipeline.
    #[must_use]
    pub fn new(
        staging: StagingManager,
        objects: FilesystemStore,
        manifests: ManifestStore,
        engine: Arc<dyn DerivationEngine>,
    ) -> Self {
        Self {
            staging,
            objects,
            manifests,
            engine,
            limits: IngestLimits::default(),
        }
    }

    /// The engine.
    #[must_use]
    pub fn engine(&self) -> &dyn DerivationEngine {
        self.engine.as_ref()
    }

    /// Where manifests are kept.
    #[must_use]
    pub const fn manifests(&self) -> &ManifestStore {
        &self.manifests
    }

    /// Check a request against its parent and fingerprint it.
    ///
    /// With no transformation named, the one that suits the parent is used;
    /// with no options, its defaults.
    ///
    /// # Errors
    ///
    /// [`PlanError`] when nothing applies or the options are refused.
    pub fn plan(
        &self,
        parent: &ArtifactManifest,
        transformation: Option<Transformation>,
        options: Option<ChdOptions>,
    ) -> Result<PlannedDerivation, PlanError> {
        let format = parent.classification.format;
        let media = parent.classification.media_family.unwrap_or_default();
        let transformation = match transformation {
            Some(t) => {
                t.check_applies(format, media)?;
                t
            }
            None => Transformation::suited_to(format, media)
                .ok_or(PlanError::NothingSuits { format, media })?,
        };
        self.engine
            .check_supported(transformation)
            .map_err(PlanError::Unsupported)?;
        let options = options.unwrap_or_else(|| ChdOptions::defaults(transformation));
        let spec = DerivationSpec::new(
            transformation,
            options,
            ToolRef {
                name: self.engine.tool_name().to_owned(),
                version: self.engine.tool_version(),
            },
        )?;
        let parent_content = content_digest(
            parent
                .components
                .iter()
                .map(|c| (&c.logical_path, &c.content.sha256)),
        );
        Ok(PlannedDerivation {
            options_json: Value::Object(spec.normalized_options().into_iter().collect()),
            fingerprint: spec.fingerprint(&parent_content),
            spec,
        })
    }

    /// Run one job and publish the derivative's manifest.
    ///
    /// The staging area is discarded afterwards whichever way it went: it
    /// holds a copy of the parent, possibly gigabytes, and a retry rebuilds
    /// it from the store.
    ///
    /// # Errors
    ///
    /// [`DerivationError`] for whichever step failed.
    pub async fn run(
        &self,
        job: &DerivationJobRecord,
    ) -> Result<DerivationOutcome, DerivationError> {
        let running = self.engine.tool_version();
        if job.tool_version != running || job.tool_name != self.engine.tool_name() {
            return Err(DerivationError::ToolChanged {
                requested: format!("{} {}", job.tool_name, job.tool_version),
                running: format!("{} {running}", self.engine.tool_name()),
            });
        }
        let parent = self.manifests.read(job.parent_artifact_id).await?;
        let options: ChdOptions = serde_json::from_value(job.options.clone())
            .map_err(|_| DerivationError::Plan(PlanError::Options(OptionsError::CodecCount(0))))?;
        // Checked again here, not only when asked: the parent's manifest is
        // the authority, and a job outlives the request that made it.
        let planned = self.plan(&parent, Some(job.transformation), Some(options))?;

        let id = job.id.to_string();
        let _ = self.staging.discard(StagingKind::Derivatives, &id).await;
        let area = self.staging.create(StagingKind::Derivatives, &id).await?;
        let result = self.derive_in(&area, job, &parent, &planned).await;
        if let Err(error) = self.staging.discard(StagingKind::Derivatives, &id).await {
            tracing::warn!(job_id = %job.id, error = ?error, "could not remove a derivation's staging area");
        }
        result
    }

    async fn derive_in(
        &self,
        area: &tangible_storage::StagingArea,
        job: &DerivationJobRecord,
        parent: &ArtifactManifest,
        planned: &PlannedDerivation,
    ) -> Result<DerivationOutcome, DerivationError> {
        let input_dir = area.path().join("input");
        for component in &parent.components {
            self.materialize(component, &input_dir).await?;
        }
        let input = tool_input(parent)?;
        let output_name = output_name(&input.logical_path);
        let output_logical = LogicalPath::parse(&format!("output/{}", output_name.as_str()))
            .map_err(|e| DerivationError::Materialize(e.to_string()))?;
        let output = area.prepare_parent(&output_logical).await?;

        let started = OffsetDateTime::now_utc();
        let report = self
            .engine
            .derive(&DeriveRequest {
                transformation: planned.spec.transformation,
                options: &planned.spec.options,
                input_dir: &input_dir,
                input: &input.logical_path,
                output: &output,
            })
            .await?;
        let completed = OffsetDateTime::now_utc();

        let ingested = area
            .promote(&output_logical, &self.objects, self.limits)
            .await?;
        let child = ArtifactId::generate();
        let manifest = child_manifest(&ChildManifest {
            child,
            job,
            parent,
            planned,
            output_name: &output_name,
            digest: ingested.digest,
            length: ingested.size_bytes,
            loss_character: report.loss_character,
            notes: &report.notes,
            started,
            completed,
            tool: ToolRef {
                name: self.engine.tool_name().to_owned(),
                version: self.engine.tool_version(),
            },
        });
        self.manifests.write(&manifest).await?;
        Ok(DerivationOutcome {
            child_artifact_id: child,
            loss_character: report.loss_character,
        })
    }

    /// Put one stored object where the tool will look for it: a hard link
    /// when the store and staging share a filesystem (objects are
    /// read-only, so the tool cannot change one through it), otherwise a
    /// copy. Either way the size must match the manifest.
    async fn materialize(
        &self,
        component: &Component,
        input_dir: &Path,
    ) -> Result<(), DerivationError> {
        let failed = |what: &str, error: &dyn std::fmt::Display| {
            DerivationError::Materialize(format!(
                "{what} {}: {error}",
                component.logical_path.as_str()
            ))
        };
        let source = self.objects.object_path(&component.content.sha256);
        let destination = input_dir.join(component.logical_path.as_str());
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| failed("could not prepare a directory for", &e))?;
        }
        if std::fs::hard_link(&source, &destination).is_err() {
            tokio::fs::copy(&source, &destination)
                .await
                .map_err(|e| failed("could not copy", &e))?;
        }
        let length = tokio::fs::metadata(&destination)
            .await
            .map_err(|e| failed("could not read", &e))?
            .len();
        if length != component.length_bytes {
            return Err(DerivationError::Materialize(format!(
                "{} is {length} bytes in the store, and its manifest says {}",
                component.logical_path.as_str(),
                component.length_bytes
            )));
        }
        Ok(())
    }
}

/// The component the tool reads: the descriptor of a CD layout, or the
/// image itself.
fn tool_input(parent: &ArtifactManifest) -> Result<&Component, DerivationError> {
    let (role, what) = match parent.classification.format {
        ArtifactFormat::CueBin | ArtifactFormat::TocBin => {
            (ComponentRole::Descriptor, "descriptor")
        }
        _ => (ComponentRole::PrimaryImage, "primary image"),
    };
    parent
        .components
        .iter()
        .find(|c| c.role == role)
        .ok_or(DerivationError::NoInput(what))
}

/// The derivative's file name: the input's name with a `.chd` extension.
fn output_name(input: &LogicalPath) -> LogicalPath {
    let last = input.as_str().rsplit('/').next().unwrap_or("image");
    let stem = last.rsplit_once('.').map_or(last, |(stem, _)| stem);
    LogicalPath::parse(&format!("{stem}.chd"))
        .or_else(|_| LogicalPath::parse("image.chd"))
        .unwrap_or_else(|_| input.clone())
}

struct ChildManifest<'a> {
    child: ArtifactId,
    job: &'a DerivationJobRecord,
    parent: &'a ArtifactManifest,
    planned: &'a PlannedDerivation,
    output_name: &'a LogicalPath,
    digest: tangible_domain::Sha256Digest,
    length: u64,
    loss_character: LossCharacter,
    notes: &'a [String],
    started: OffsetDateTime,
    completed: OffsetDateTime,
    tool: ToolRef,
}

fn child_manifest(input: &ChildManifest<'_>) -> ArtifactManifest {
    let component = Component {
        id: ComponentId::generate(),
        logical_path: input.output_name.clone(),
        source_filename: Some(input.output_name.as_str().to_owned()),
        role: ComponentRole::PrimaryImage,
        ordinal: 0,
        length_bytes: input.length,
        storage: StorageRef::for_digest(&input.digest),
        content: ContentHashes {
            sha256: input.digest,
            sha1: None,
            md5: None,
            crc32: None,
        },
        media_type: None,
        attributes: Extensions::new(),
    };
    let mut evidence = vec![format!(
        "made by {} {} from artifact {}",
        input.tool.name, input.tool.version, input.parent.artifact_id
    )];
    evidence.extend(input.notes.iter().cloned());
    ArtifactManifest {
        schema: SCHEMA_ID.to_owned(),
        artifact_id: input.child,
        created_at: input.completed,
        origin: Origin {
            kind: ArtifactOrigin::Derived,
            source_kind: "derivation".to_owned(),
            source_reference: Some(input.job.id.to_string()),
            source_filename: Some(input.output_name.as_str().to_owned()),
            acquired_at: Some(input.completed),
            operator_note: None,
        },
        classification: Classification {
            artifact_kind: ArtifactKind::SingleFileImage,
            format: input.planned.spec.transformation.output_format(),
            format_confidence: 1.0,
            detectors: vec![Detector {
                name: input.tool.name.clone(),
                version: input.tool.version.clone(),
                evidence,
            }],
            media_family: input.parent.classification.media_family,
            total_bytes: input.length,
        },
        components: vec![component],
        // The CHD's own structure is not parsed yet; its parent's manifest
        // keeps the track layout it was made from.
        topology: Topology::Unknown,
        validation: Validation {
            state: ValidationState::Pending,
            validated_at: None,
            validators: vec![],
        },
        lineage: Some(Lineage {
            parent_artifact_id: input.parent.artifact_id,
            transformation: input.planned.spec.transformation.to_string(),
            tool: input.tool.clone(),
            normalized_options: input
                .planned
                .spec
                .normalized_options()
                .into_iter()
                .collect(),
            command_fingerprint: input.planned.fingerprint.clone(),
            loss_character: input.loss_character,
            started_at: Some(input.started),
            completed_at: Some(input.completed),
        }),
        associations: vec![],
        compatibility: Compatibility {
            structurally_valid: false,
            burn_support: BurnSupport {
                // No engine burns a CHD; its parent is what gets burned.
                state: "unsupported".to_owned(),
                profiles: vec![],
                engines: vec![],
                warnings: vec![],
            },
            target_claims: vec![],
        },
        extensions: Extensions::new(),
    }
}

/// How the runner behaves.
#[derive(Debug, Clone)]
pub struct DerivationRunnerSettings {
    /// How long a claim is held. Long: chdman on a Blu-ray image takes many
    /// minutes, and a lease that lapses mid-run invites a second worker.
    pub lease: Duration,
    /// How long to wait when the queue is empty and nobody nudges.
    pub poll_interval: Duration,
    /// How long a retryable failure is held before it is offered again.
    pub retry_backoff: Duration,
    /// What to call this process in the lease.
    pub owner: String,
}

impl Default for DerivationRunnerSettings {
    fn default() -> Self {
        Self {
            lease: Duration::from_secs(4 * 60 * 60),
            poll_interval: Duration::from_secs(30),
            retry_backoff: Duration::from_secs(60),
            owner: "tangible-server".to_owned(),
        }
    }
}

/// What the routes need to queue derivations and wake the runner.
#[derive(Debug, Clone)]
pub struct Derivations {
    pipeline: DerivationPipeline,
    nudge: Arc<Notify>,
}

impl Derivations {
    /// Wrap a pipeline.
    #[must_use]
    pub fn new(pipeline: DerivationPipeline) -> Self {
        Self {
            pipeline,
            nudge: Arc::new(Notify::new()),
        }
    }

    /// The pipeline.
    #[must_use]
    pub const fn pipeline(&self) -> &DerivationPipeline {
        &self.pipeline
    }

    /// Wake the runner now rather than at its next poll.
    pub fn nudge(&self) {
        self.nudge.notify_one();
    }
}

/// Drains the derivation queue.
#[derive(Debug, Clone)]
pub struct DerivationRunner {
    database: Database,
    derivations: Derivations,
    settings: DerivationRunnerSettings,
}

impl DerivationRunner {
    /// Assemble a runner.
    #[must_use]
    pub const fn new(
        database: Database,
        derivations: Derivations,
        settings: DerivationRunnerSettings,
    ) -> Self {
        Self {
            database,
            derivations,
            settings,
        }
    }

    /// Work the queue until `shutdown` resolves. A job already running
    /// finishes first.
    pub async fn run<S>(self, shutdown: S)
    where
        S: Future<Output = ()> + Send,
    {
        let mut shutdown = Box::pin(shutdown);
        loop {
            let worked = tokio::select! {
                () = &mut shutdown => {
                    tracing::info!("derivation runner stopping");
                    return;
                }
                worked = self.step() => worked,
            };
            let idle = match worked {
                Ok(worked) => !worked,
                Err(error) => {
                    tracing::error!(error = ?error, "the derivation queue could not be read");
                    true
                }
            };
            if idle {
                tokio::select! {
                    () = &mut shutdown => return,
                    () = self.derivations.nudge.notified() => {}
                    () = tokio::time::sleep(self.settings.poll_interval) => {}
                }
            }
        }
    }

    /// Take one job and run it. Returns whether there was one.
    ///
    /// # Errors
    ///
    /// [`DbError`] if the queue cannot be read. A job that fails is recorded
    /// against the job.
    pub async fn step(&self) -> Result<bool, DbError> {
        let lease = i64::try_from(self.settings.lease.as_secs()).unwrap_or(i64::MAX);
        let Some(job) =
            claim_next_derivation_job(self.database.pool(), &self.settings.owner, lease).await?
        else {
            return Ok(false);
        };
        tracing::info!(job_id = %job.id, parent = %job.parent_artifact_id, transformation = %job.transformation, "derivation claimed");
        self.execute(&job).await;
        Ok(true)
    }

    async fn execute(&self, job: &DerivationJobRecord) {
        let pipeline = self.derivations.pipeline();
        let outcome = match pipeline.run(job).await {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::warn!(job_id = %job.id, code = error.code(), error = %error, "derivation failed");
                self.fail(job, error.is_retryable(), error.code(), &error.to_string())
                    .await;
                return;
            }
        };
        // The manifest is the durable record; the catalog is what the rest
        // of the system can reference. Registered before the derivation is
        // recorded, so lineage never names an artifact the catalog lacks.
        let registered = match pipeline.manifests().read(outcome.child_artifact_id).await {
            Ok(manifest) => catalog::register_manifest(&self.database, &manifest)
                .await
                .map_err(|e| e.to_string()),
            Err(error) => Err(error.to_string()),
        };
        if let Err(detail) = registered {
            tracing::error!(job_id = %job.id, detail, "the derivative was stored but could not be catalogued");
            self.fail(job, true, "CATALOG_WRITE_FAILED", &detail).await;
            return;
        }
        match complete_derivation_job(
            self.database.pool(),
            CompletedDerivation {
                job_id: job.id,
                child_artifact_id: outcome.child_artifact_id,
                loss_character: outcome.loss_character,
            },
        )
        .await
        {
            Ok(true) => tracing::info!(
                job_id = %job.id,
                child = %outcome.child_artifact_id,
                loss = %outcome.loss_character,
                "derivation complete"
            ),
            Ok(false) => {
                tracing::warn!(job_id = %job.id, "the derivation finished but its job was no longer running");
            }
            Err(error) => {
                tracing::error!(job_id = %job.id, error = ?error, "the derivation finished but could not be recorded");
            }
        }
    }

    async fn fail(&self, job: &DerivationJobRecord, retryable: bool, code: &str, detail: &str) {
        let backoff = i64::try_from(self.settings.retry_backoff.as_secs()).unwrap_or(60);
        if let Err(error) = fail_derivation_job(
            self.database.pool(),
            job.id,
            retryable,
            code,
            detail,
            backoff,
        )
        .await
        {
            tracing::error!(job_id = %job.id, error = ?error, "could not record a derivation failure");
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_output_is_named_for_the_input() {
        let name = |s: &str| {
            output_name(&LogicalPath::parse(s).unwrap())
                .as_str()
                .to_owned()
        };
        assert_eq!(name("Game (Disc 1).cue"), "Game (Disc 1).chd");
        assert_eq!(name("tracks/disc.toc"), "disc.chd");
        assert_eq!(name("disc.iso"), "disc.chd");
        assert_eq!(name("noextension"), "noextension.chd");
    }

    #[test]
    fn plan_and_engine_failures_are_terminal_and_coded() {
        let refused = DerivationError::NoInput("descriptor");
        assert!(!refused.is_retryable());
        assert_eq!(refused.code(), "DERIVATION_NO_INPUT");
        let changed = DerivationError::ToolChanged {
            requested: "chdman 0.251".to_owned(),
            running: "chdman 0.252".to_owned(),
        };
        assert!(!changed.is_retryable());
        assert!(changed.to_string().contains("ask for it again"));
        assert!(DerivationError::Materialize("x".to_owned()).is_retryable());
    }
}
