// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Making derivatives in the background.
//!
//! A request becomes a job (see `tangible_db::derivations`); this module works
//! the jobs. For a CHD it copies the parent's files out of the
//! content-addressed store into a staging area, runs the engine there,
//! promotes the single output file into the store, writes the child's
//! manifest with its lineage, catalogues the child and records the
//! derivation. The parent is read, never written: no tool runs against a
//! canonical object, and the copy it does run against is discarded after.
//!
//! A CUE/BIN of a CloneCD image is made by Tangible itself, with no engine
//! (docs/plans/ccd-images.md): a sheet written from the parent's tracks,
//! read back to check it gives the same tracks, and the parent's IMG as the
//! BIN, the same stored object, so no bytes are copied.
//!
//! A CHD imported as an original is extracted by the engine
//! (docs/plans/chd-images.md): a CUE/BIN of a CD CHD, the ISO of a DVD one,
//! each checked by the engine against the CHD's data SHA-1, then read here
//! as an import reads one.
//!
//! So are an Alcohol 120% image's (docs/plans/mds-images.md): a CUE/BIN of a
//! CD, its MDF the BIN, or, when the MDF interleaves subchannel, a BIN of
//! each sector's first 2352 bytes; and the ISO a DVD image's one MDF is,
//! read back as one and kept as the same stored object.
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
    Detector, Extensions, Lineage, Origin, SCHEMA_ID, StorageRef, Subchannel, ToolRef, Topology,
    Track, TrackIndex, Validation, ValidatorResult,
};
use tangible_domain::{ArtifactId, ComponentId, LogicalPath, LossCharacter, Transformation};
use tangible_image::chd;
use tangible_image::derive::{
    DerivationEngine, DeriveError, DeriveReport, DeriveRequest, bin_beside,
};
use tangible_image::{cue, iso, mds};
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
    /// The engine for CHDs, when this server has one; the transformations
    /// Tangible does itself need none.
    engine: Option<Arc<dyn DerivationEngine>>,
    limits: IngestLimits,
}

impl std::fmt::Debug for DerivationPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DerivationPipeline")
            .field("tool", &self.engine.as_ref().map(|e| e.tool_name()))
            .field("version", &self.engine.as_ref().map(|e| e.tool_version()))
            .finish_non_exhaustive()
    }
}

/// The tool a transformation Tangible does itself is recorded as.
fn tangible_tool() -> ToolRef {
    ToolRef {
        name: tangible_domain::derivation::TANGIBLE.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

impl DerivationPipeline {
    /// Assemble a pipeline with an engine for CHDs.
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
            engine: Some(engine),
            limits: IngestLimits::default(),
        }
    }

    /// Assemble a pipeline with no engine: it makes only what Tangible makes
    /// itself, a CUE/BIN of a CloneCD image.
    #[must_use]
    pub fn without_engine(
        staging: StagingManager,
        objects: FilesystemStore,
        manifests: ManifestStore,
    ) -> Self {
        Self {
            staging,
            objects,
            manifests,
            engine: None,
            limits: IngestLimits::default(),
        }
    }

    /// The engine, when there is one.
    #[must_use]
    pub fn engine(&self) -> Option<&dyn DerivationEngine> {
        self.engine.as_deref()
    }

    /// The tool that would run `transformation` here, or why none can.
    ///
    /// # Errors
    ///
    /// [`PlanError::Unsupported`] when no engine here runs it.
    pub fn tool_for(&self, transformation: Transformation) -> Result<ToolRef, PlanError> {
        if transformation.is_in_process() {
            return Ok(tangible_tool());
        }
        let engine = self.engine.as_ref().ok_or_else(|| {
            PlanError::Unsupported(
                "this server runs no derivation engine for CHDs; see TANGIBLE_DERIVATION_ENGINE"
                    .to_owned(),
            )
        })?;
        engine
            .check_supported(transformation)
            .map_err(PlanError::Unsupported)?;
        Ok(ToolRef {
            name: engine.tool_name().to_owned(),
            version: engine.tool_version(),
        })
    }

    /// The transformation to offer for an artifact: the first that applies
    /// and that this server can run.
    #[must_use]
    pub fn suggested(&self, format: ArtifactFormat, media: MediaFamily) -> Option<Transformation> {
        Transformation::all_suited_to(format, media).find(|t| self.tool_for(*t).is_ok())
    }

    /// The transformation to offer for this artifact, as [`Self::suggested`],
    /// except for a CHD made here: what it was made from is the CUE/BIN or
    /// ISO, so extracting it again is never offered (it may still be asked
    /// for).
    #[must_use]
    pub fn suggested_for(&self, manifest: &ArtifactManifest) -> Option<Transformation> {
        if manifest.classification.format == ArtifactFormat::Chd && manifest.lineage.is_some() {
            return None;
        }
        self.suggested(
            manifest.classification.format,
            manifest.classification.media_family.unwrap_or_default(),
        )
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
            None => self
                .suggested_for(parent)
                .or_else(|| {
                    Transformation::suited_to(format, media)
                        .filter(|_| format != ArtifactFormat::Chd || parent.lineage.is_none())
                })
                .ok_or(PlanError::NothingSuits { format, media })?,
        };
        let tool = self.tool_for(transformation)?;
        let options = if transformation.takes_options() {
            Some(options.unwrap_or_else(|| ChdOptions::defaults(transformation)))
        } else {
            // Kept as given, so options sent to a transformation that takes
            // none are refused rather than ignored.
            options
        };
        let spec = DerivationSpec::new(transformation, options, tool)?;
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
        let running = self.tool_for(job.transformation)?;
        if job.tool_version != running.version || job.tool_name != running.name {
            return Err(DerivationError::ToolChanged {
                requested: format!("{} {}", job.tool_name, job.tool_version),
                running: format!("{} {}", running.name, running.version),
            });
        }
        let parent = self.manifests.read(job.parent_artifact_id).await?;
        let options: Option<ChdOptions> = if job.transformation.takes_options() {
            Some(serde_json::from_value(job.options.clone()).map_err(|_| {
                DerivationError::Plan(PlanError::Options(OptionsError::CodecCount(0)))
            })?)
        } else {
            None
        };
        // Checked again here, not only when asked: the parent's manifest is
        // the authority, and a job outlives the request that made it.
        let planned = self.plan(&parent, Some(job.transformation), options)?;

        let id = job.id.to_string();
        let _ = self.staging.discard(StagingKind::Derivatives, &id).await;
        let area = self.staging.create(StagingKind::Derivatives, &id).await?;
        let result = match planned.spec.transformation {
            Transformation::CueBinFromCcd => self.cue_from_ccd(&area, job, &parent, &planned).await,
            Transformation::CueBinFromMds => self.cue_from_mds(&area, job, &parent, &planned).await,
            Transformation::IsoFromMds => self.iso_from_mds(job, &parent, &planned).await,
            Transformation::ChdCreateCd
            | Transformation::ChdCreateDvd
            | Transformation::CueBinFromChd
            | Transformation::IsoFromChd => self.derive_in(&area, job, &parent, &planned).await,
        };
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
        let transformation = planned.spec.transformation;
        let options = planned.spec.options.as_ref();
        let Some(engine) = &self.engine else {
            return Err(DerivationError::Plan(PlanError::Unsupported(
                "this server runs no derivation engine for CHDs".to_owned(),
            )));
        };
        if transformation.takes_options() && options.is_none() {
            return Err(DerivationError::Plan(PlanError::Options(
                OptionsError::Missing(transformation),
            )));
        }
        let input_dir = area.path().join("input");
        for component in &parent.components {
            self.materialize(component, &input_dir).await?;
        }
        let input = tool_input(parent)?;
        if matches!(
            transformation,
            Transformation::CueBinFromChd | Transformation::IsoFromChd
        ) {
            let extraction = Extraction {
                area,
                job,
                parent,
                planned,
                input,
                input_dir: &input_dir,
            };
            return self.extract_chd(engine.as_ref(), &extraction).await;
        }
        let output_name = output_name(&input.logical_path);
        let output_logical = LogicalPath::parse(&format!("output/{}", output_name.as_str()))
            .map_err(|e| DerivationError::Materialize(e.to_string()))?;
        let output = area.prepare_parent(&output_logical).await?;

        let started = OffsetDateTime::now_utc();
        let report = engine
            .derive(&DeriveRequest {
                transformation: planned.spec.transformation,
                options,
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
            tool: planned.spec.tool.clone(),
        });
        self.manifests.write(&manifest).await?;
        Ok(DerivationOutcome {
            child_artifact_id: child,
            loss_character: report.loss_character,
        })
    }

    /// Extract a CHD imported as an original: the engine extracts and checks
    /// it, and what it wrote is read here as an import would read it.
    async fn extract_chd(
        &self,
        engine: &dyn DerivationEngine,
        extraction: &Extraction<'_>,
    ) -> Result<DerivationOutcome, DerivationError> {
        let transformation = extraction.planned.spec.transformation;
        let chd_path = extraction
            .input_dir
            .join(extraction.input.logical_path.as_str());
        // What the CHD says, before chdman is asked: a delta cannot be read
        // without its parent, and a CUE/BIN cannot hold subcode.
        let description = read_chd(chd_path).await?;
        if description.header.parent_sha1.is_some() {
            return Err(not_expressible(
                "the CHD is a delta of another CHD and cannot be read without that parent"
                    .to_owned(),
            ));
        }
        if transformation == Transformation::CueBinFromChd
            && description
                .tracks
                .iter()
                .any(|track| track.subcode != chd::ChdSubcode::None)
        {
            return Err(not_expressible(
                "the CHD keeps subcode, which a CUE/BIN cannot hold".to_owned(),
            ));
        }

        let stem = file_stem(&extraction.input.logical_path);
        let extension = if transformation == Transformation::CueBinFromChd {
            "cue"
        } else {
            "iso"
        };
        let output_logical = LogicalPath::parse(&format!("output/{stem}.{extension}"))
            .map_err(|e| DerivationError::Materialize(e.to_string()))?;
        let output = extraction.area.prepare_parent(&output_logical).await?;
        let started = OffsetDateTime::now_utc();
        let report = engine
            .derive(&DeriveRequest {
                transformation,
                options: None,
                input_dir: extraction.input_dir,
                input: &extraction.input.logical_path,
                output: &output,
            })
            .await?;
        if transformation == Transformation::CueBinFromChd {
            self.store_extracted_cue_bin(extraction, &stem, &output, &report, started)
                .await
        } else {
            self.store_extracted_iso(extraction, &stem, &output, &report, started)
                .await
        }
    }

    /// Read the CUE/BIN chdman extracted as an import reads one, store both
    /// files, and publish its manifest.
    async fn store_extracted_cue_bin(
        &self,
        extraction: &Extraction<'_>,
        stem: &str,
        sheet_path: &Path,
        report: &DeriveReport,
        started: OffsetDateTime,
    ) -> Result<DerivationOutcome, DerivationError> {
        let bin_name = format!("{stem}.bin");
        let bin_length = tokio::fs::metadata(bin_beside(sheet_path))
            .await
            .map_err(|e| DerivationError::Materialize(format!("could not read the BIN: {e}")))?
            .len();
        let sheet = read_prefix(sheet_path, cue::MAX_CUE_BYTES + 1)
            .await
            .map_err(|e| DerivationError::Materialize(format!("could not read the sheet: {e}")))?;
        let parsed =
            cue::parse(&sheet).map_err(|e| not_equivalent(format!("chdman's sheet: {e}")))?;
        if !matches!(parsed.files.as_slice(), [file] if file.name == bin_name) {
            return Err(not_equivalent(
                "chdman's sheet does not name the one BIN it wrote".to_owned(),
            ));
        }
        let laid = cue::layout(&parsed, &[bin_length])
            .map_err(|e| not_equivalent(format!("chdman's sheet: {e}")))?;

        let area = extraction.area;
        let cue_logical = LogicalPath::parse(&format!("output/{stem}.cue"))
            .map_err(|e| DerivationError::Materialize(e.to_string()))?;
        let bin_logical = LogicalPath::parse(&format!("output/{bin_name}"))
            .map_err(|e| DerivationError::Materialize(e.to_string()))?;
        let cue_stored = area
            .promote(&cue_logical, &self.objects, self.limits)
            .await?;
        let bin_stored = area
            .promote(&bin_logical, &self.objects, self.limits)
            .await?;
        let completed = OffsetDateTime::now_utc();

        let name = |text: String| {
            LogicalPath::parse(&text).map_err(|e| DerivationError::Materialize(e.to_string()))
        };
        let bin = stored_component(bin_logical, bin_stored.digest, bin_stored.size_bytes);
        let child = ArtifactId::generate();
        let manifest = cue_bin_manifest(&CueBinManifest {
            child,
            job: extraction.job,
            parent: extraction.parent,
            planned: extraction.planned,
            cue: (
                name(format!("{stem}.cue"))?,
                cue_stored.digest,
                cue_stored.size_bytes,
            ),
            bin: (name(bin_name)?, &bin),
            layout: &laid,
            notes: &report.notes,
            loss: report.loss_character,
            validator_code: "CUE_LAID_OUT",
            started,
            completed,
        });
        self.manifests.write(&manifest).await?;
        Ok(DerivationOutcome {
            child_artifact_id: child,
            loss_character: report.loss_character,
        })
    }

    /// Read the ISO chdman extracted as an import reads one, store it, and
    /// publish its manifest.
    async fn store_extracted_iso(
        &self,
        extraction: &Extraction<'_>,
        stem: &str,
        iso_path: &Path,
        report: &DeriveReport,
        started: OffsetDateTime,
    ) -> Result<DerivationOutcome, DerivationError> {
        let length = tokio::fs::metadata(iso_path)
            .await
            .map_err(|e| DerivationError::Materialize(format!("could not read the ISO: {e}")))?
            .len();
        let prefix = read_prefix(iso_path, iso::REQUIRED_PREFIX_BYTES)
            .await
            .map_err(|e| DerivationError::Materialize(format!("could not read the ISO: {e}")))?;
        let inspection = iso::inspect(&prefix, length, Some("iso"));
        if inspection.primary.is_none() {
            return Err(not_equivalent(
                "the extracted image holds no ISO 9660 volume".to_owned(),
            ));
        }
        let logical = LogicalPath::parse(&format!("output/{stem}.iso"))
            .map_err(|e| DerivationError::Materialize(e.to_string()))?;
        let stored = extraction
            .area
            .promote(&logical, &self.objects, self.limits)
            .await?;
        let completed = OffsetDateTime::now_utc();
        let name = LogicalPath::parse(&format!("{stem}.iso"))
            .map_err(|e| DerivationError::Materialize(e.to_string()))?;
        let component = stored_component(logical, stored.digest, stored.size_bytes);
        let child = ArtifactId::generate();
        let manifest = iso_manifest(&IsoManifest {
            child,
            job: extraction.job,
            parent: extraction.parent,
            planned: extraction.planned,
            iso: (name, &component),
            inspection: &inspection,
            notes: &report.notes,
            loss: report.loss_character,
            started,
            completed,
        });
        self.manifests.write(&manifest).await?;
        Ok(DerivationOutcome {
            child_artifact_id: child,
            loss_character: report.loss_character,
        })
    }

    /// Make a CUE/BIN of a CloneCD image, in-process: a sheet written from
    /// the parent's tracks and read back to check it says the same, stored;
    /// the parent's IMG, already stored, as the BIN.
    async fn cue_from_ccd(
        &self,
        area: &tangible_storage::StagingArea,
        job: &DerivationJobRecord,
        parent: &ArtifactManifest,
        planned: &PlannedDerivation,
    ) -> Result<DerivationOutcome, DerivationError> {
        let started = OffsetDateTime::now_utc();
        let failed = |code: &'static str, detail: String| {
            DerivationError::Engine(DeriveError::Failed {
                code,
                detail,
                retryable: false,
            })
        };
        let Topology::CdTracks {
            descriptor_component_id,
            catalog,
            session_count,
            tracks,
            subchannel,
        } = &parent.topology
        else {
            return Err(DerivationError::NoInput("track layout"));
        };
        let image_id = tracks
            .first()
            .map(|track| track.component_id)
            .ok_or(DerivationError::NoInput("track layout"))?;
        if tracks.iter().any(|track| track.component_id != image_id) {
            return Err(failed(
                "DERIVATION_NOT_EXPRESSIBLE",
                "the tracks are in more than one file".to_owned(),
            ));
        }
        let image = parent
            .components
            .iter()
            .find(|component| component.id == image_id)
            .ok_or(DerivationError::NoInput("image"))?;
        let stem = parent
            .components
            .iter()
            .find(|component| component.id == *descriptor_component_id)
            .map_or_else(
                || "disc".to_owned(),
                |component| file_stem(&component.logical_path),
            );
        let laid = cue::CdLayout {
            catalog: catalog.clone(),
            tracks: tracks.iter().map(track_layout).collect(),
            session_count: *session_count,
            warnings: Vec::new(),
        };
        let mut dropped = vec!["the raw table of contents".to_owned()];
        if subchannel.as_ref().is_some_and(|s| s.present) {
            dropped.insert(0, "the subchannel".to_owned());
        }
        let draft = CueBinDraft {
            laid,
            stem,
            bin: image.clone(),
            dropped,
        };
        self.store_cue_bin(area, job, parent, planned, draft, started)
            .await
    }

    /// Make a CUE/BIN of an Alcohol 120% CD image, in-process, from its
    /// descriptor read again: its MDF as the BIN when it holds plain
    /// sectors, otherwise a new BIN of each sector without its subchannel.
    async fn cue_from_mds(
        &self,
        area: &tangible_storage::StagingArea,
        job: &DerivationJobRecord,
        parent: &ArtifactManifest,
        planned: &PlannedDerivation,
    ) -> Result<DerivationOutcome, DerivationError> {
        let started = OffsetDateTime::now_utc();
        let (stem, descriptor) = self.mds_descriptor(parent).await?;
        let data = only_data_file(parent)?;
        let mut dropped = vec!["the descriptor's raw table of contents".to_owned()];
        if descriptor.has_dpm {
            dropped.push("its DPM data".to_owned());
        }

        if descriptor
            .tracks
            .iter()
            .all(|track| track.subchannel == mds::Subchannel::None)
        {
            let laid = mds::layout(&descriptor, data.length_bytes)
                .map_err(|e| not_expressible(e.to_string()))?;
            let draft = CueBinDraft {
                laid,
                stem,
                bin: data.clone(),
                dropped,
            };
            return self
                .store_cue_bin(area, job, parent, planned, draft, started)
                .await;
        }

        let laid = mds::layout_without_subchannel(&descriptor, data.length_bytes)
            .map_err(|e| not_expressible(e.to_string()))?;
        let staged = LogicalPath::parse(&format!("output/{stem}.bin"))
            .map_err(|e| DerivationError::Materialize(e.to_string()))?;
        let at = area.prepare_parent(&staged).await?;
        strip_subchannel(
            &self.objects.object_path(&data.content.sha256),
            &at,
            laid.total_sectors(),
        )
        .await
        .map_err(|e| {
            DerivationError::Materialize(format!("could not take out the subchannel: {e}"))
        })?;
        let ingested = area.promote(&staged, &self.objects, self.limits).await?;
        dropped.insert(0, "the subchannel".to_owned());
        let draft = CueBinDraft {
            laid,
            stem,
            bin: stored_component(staged, ingested.digest, ingested.size_bytes),
            dropped,
        };
        self.store_cue_bin(area, job, parent, planned, draft, started)
            .await
    }

    /// Make the ISO an Alcohol 120% DVD image's one MDF is, in-process: the
    /// MDF read back as an ISO 9660 volume, and kept as the same stored
    /// object under an `.iso` name.
    async fn iso_from_mds(
        &self,
        job: &DerivationJobRecord,
        parent: &ArtifactManifest,
        planned: &PlannedDerivation,
    ) -> Result<DerivationOutcome, DerivationError> {
        let started = OffsetDateTime::now_utc();
        let (stem, descriptor) = self.mds_descriptor(parent).await?;
        let data = only_data_file(parent)?;
        let at_start = matches!(
            descriptor.tracks.as_slice(),
            [track] if track.start_offset == 0 && track.files.len() == 1
        );
        if !at_start {
            return Err(not_expressible(
                "the DVD image is not one track at the start of one data file".to_owned(),
            ));
        }
        let path = self.objects.object_path(&data.content.sha256);
        let prefix = read_prefix(&path, iso::REQUIRED_PREFIX_BYTES)
            .await
            .map_err(|e| DerivationError::Materialize(format!("could not read the MDF: {e}")))?;
        let inspection = iso::inspect(&prefix, data.length_bytes, Some("iso"));
        if inspection.primary.is_none() {
            return Err(not_equivalent(
                "the data file holds no ISO 9660 volume".to_owned(),
            ));
        }
        let completed = OffsetDateTime::now_utc();
        let mut dropped =
            vec!["the descriptor, with any disc structures and BCA it holds".to_owned()];
        if descriptor.has_dpm {
            dropped.push("its DPM data".to_owned());
        }
        let notes = vec![
            format!("not carried: {}", listed(&dropped)),
            "the ISO is the MDF's own bytes, read back as an ISO 9660 volume".to_owned(),
        ];
        let name = LogicalPath::parse(&format!("{stem}.iso"))
            .map_err(|e| DerivationError::Materialize(e.to_string()))?;
        let child = ArtifactId::generate();
        let manifest = iso_manifest(&IsoManifest {
            child,
            job,
            parent,
            planned,
            iso: (name, data),
            inspection: &inspection,
            notes: &notes,
            loss: LossCharacter::Lossy,
            started,
            completed,
        });
        self.manifests.write(&manifest).await?;
        Ok(DerivationOutcome {
            child_artifact_id: child,
            loss_character: LossCharacter::Lossy,
        })
    }

    /// An MDS image's descriptor, read again from the store, and the stem its
    /// derivatives are named for.
    async fn mds_descriptor(
        &self,
        parent: &ArtifactManifest,
    ) -> Result<(String, mds::MdsDescriptor), DerivationError> {
        let component = parent
            .components
            .iter()
            .find(|component| component.role == ComponentRole::Descriptor)
            .ok_or(DerivationError::NoInput("descriptor"))?;
        let limit = mds::MAX_DESCRIPTOR_BYTES;
        let bytes = read_prefix(
            &self.objects.object_path(&component.content.sha256),
            limit + 1,
        )
        .await
        .map_err(|e| DerivationError::Materialize(format!("could not read the descriptor: {e}")))?;
        if bytes.len() as u64 != component.length_bytes {
            return Err(DerivationError::Materialize(format!(
                "{} is {} bytes in the store, and its manifest says {}",
                component.logical_path.as_str(),
                bytes.len(),
                component.length_bytes
            )));
        }
        let descriptor = mds::parse(&bytes)
            .map_err(|e| not_expressible(format!("the descriptor could not be read: {e}")))?;
        Ok((file_stem(&component.logical_path), descriptor))
    }

    /// Write a CUE sheet for a draft, read it back, store it, and publish
    /// the CUE/BIN's manifest.
    async fn store_cue_bin(
        &self,
        area: &tangible_storage::StagingArea,
        job: &DerivationJobRecord,
        parent: &ArtifactManifest,
        planned: &PlannedDerivation,
        draft: CueBinDraft,
        started: OffsetDateTime,
    ) -> Result<DerivationOutcome, DerivationError> {
        let stem = &draft.stem;
        let bin_name = format!("{stem}.bin");
        let cue_name = LogicalPath::parse(&format!("{stem}.cue"))
            .map_err(|e| DerivationError::Materialize(e.to_string()))?;
        let bin_path = LogicalPath::parse(&bin_name)
            .map_err(|e| DerivationError::Materialize(e.to_string()))?;
        let (sheet, back) = checked_sheet(&draft.laid, &bin_name, draft.bin.length_bytes)?;

        let staged = LogicalPath::parse(&format!("output/{}", cue_name.as_str()))
            .map_err(|e| DerivationError::Materialize(e.to_string()))?;
        let at = area.prepare_parent(&staged).await?;
        tokio::fs::write(&at, sheet.as_bytes())
            .await
            .map_err(|e| DerivationError::Materialize(format!("could not write the sheet: {e}")))?;
        let ingested = area.promote(&staged, &self.objects, self.limits).await?;
        let completed = OffsetDateTime::now_utc();

        let notes = vec![
            format!("not carried: {}", listed(&draft.dropped)),
            "the sheet was read back and gives the same tracks".to_owned(),
        ];
        let image = &draft.bin;
        let child = ArtifactId::generate();
        let manifest = cue_bin_manifest(&CueBinManifest {
            child,
            job,
            parent,
            planned,
            cue: (cue_name, ingested.digest, ingested.size_bytes),
            bin: (bin_path, image),
            layout: &back,
            notes: &notes,
            loss: LossCharacter::Lossy,
            validator_code: "CUE_READ_BACK_IDENTICAL",
            started,
            completed,
        });
        self.manifests.write(&manifest).await?;
        Ok(DerivationOutcome {
            child_artifact_id: child,
            loss_character: LossCharacter::Lossy,
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

/// A sheet for `laid`, naming `bin_name`, and the layout it reads back as
/// over a BIN of `bin_bytes`, through the parser every import uses: it must
/// give exactly the tracks it was written from.
fn checked_sheet(
    laid: &cue::CdLayout,
    bin_name: &str,
    bin_bytes: u64,
) -> Result<(String, cue::CdLayout), DerivationError> {
    let failed = |code: &'static str, detail: String| {
        DerivationError::Engine(DeriveError::Failed {
            code,
            detail,
            retryable: false,
        })
    };
    let sheet = cue::write_sheet(laid, bin_name)
        .map_err(|e| failed("DERIVATION_NOT_EXPRESSIBLE", e.to_string()))?;
    let back = cue::parse(sheet.as_bytes())
        .map_err(|e| failed("DERIVATION_NOT_EQUIVALENT", e.to_string()))
        .and_then(|parsed| {
            cue::layout(&parsed, &[bin_bytes])
                .map_err(|e| failed("DERIVATION_NOT_EQUIVALENT", e.to_string()))
        })?;
    if back.tracks != laid.tracks || back.catalog != laid.catalog {
        return Err(failed(
            "DERIVATION_NOT_EQUIVALENT",
            "the sheet written does not read back as the same tracks".to_owned(),
        ));
    }
    Ok((sheet, back))
}

/// A file's name without its directory or extension.
fn file_stem(path: &LogicalPath) -> String {
    let last = path.as_str().rsplit('/').next().unwrap_or("disc");
    last.rsplit_once('.')
        .map_or(last, |(stem, _)| stem)
        .to_owned()
}

/// A derivation Tangible cannot express for this parent: it will not change.
fn not_expressible(detail: String) -> DerivationError {
    DerivationError::Engine(DeriveError::Failed {
        code: "DERIVATION_NOT_EXPRESSIBLE",
        detail,
        retryable: false,
    })
}

/// A derivative that did not read back as what it was made from.
fn not_equivalent(detail: String) -> DerivationError {
    DerivationError::Engine(DeriveError::Failed {
        code: "DERIVATION_NOT_EQUIVALENT",
        detail,
        retryable: false,
    })
}

/// Things in a sentence: `a`, `a and b`, `a, b and c`.
fn listed(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The one data file of an image whose tracks are all in it.
fn only_data_file(parent: &ArtifactManifest) -> Result<&Component, DerivationError> {
    let mut data = parent
        .components
        .iter()
        .filter(|component| component.role == ComponentRole::TrackData);
    let first = data.next().ok_or(DerivationError::NoInput("data file"))?;
    if data.next().is_some() {
        return Err(not_expressible(
            "the image is in more than one data file".to_owned(),
        ));
    }
    Ok(first)
}

/// A component for an object just stored; its name, role and place are
/// the manifest's to set.
fn stored_component(
    logical_path: LogicalPath,
    digest: tangible_domain::Sha256Digest,
    length: u64,
) -> Component {
    Component {
        id: ComponentId::generate(),
        logical_path,
        source_filename: None,
        role: ComponentRole::TrackData,
        ordinal: 0,
        length_bytes: length,
        storage: StorageRef::for_digest(&digest),
        content: ContentHashes {
            sha256: digest,
            sha1: None,
            md5: None,
            crc32: None,
        },
        media_type: None,
        attributes: Extensions::new(),
    }
}

/// Copy the first `sectors` sectors of an image that interleaves 96 bytes of
/// subchannel after each 2352, without the subchannel. Streamed: a chunk of
/// sectors at a time, never the whole image.
async fn strip_subchannel(source: &Path, destination: &Path, sectors: u64) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    const CHUNK: usize = 256;
    let raw = mds::RAW_SECTOR_BYTES as usize;
    let whole = raw + mds::SUBCHANNEL_BYTES as usize;
    let mut reader = tokio::fs::File::open(source).await?;
    let mut writer = tokio::io::BufWriter::new(tokio::fs::File::create(destination).await?);
    let mut buffer = vec![0_u8; whole * CHUNK];
    let mut left = sectors;
    while left > 0 {
        let count = usize::try_from(left).map_or(CHUNK, |left| left.min(CHUNK));
        let chunk = &mut buffer[..whole * count];
        reader.read_exact(chunk).await?;
        for sector in chunk.chunks_exact(whole) {
            writer.write_all(&sector[..raw]).await?;
        }
        left = left.saturating_sub(count as u64);
    }
    writer.flush().await?;
    writer.into_inner().sync_all().await
}

/// Read at most `limit` bytes from the start of a file.
async fn read_prefix(path: &Path, limit: usize) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;
    let file = tokio::fs::File::open(path).await?;
    let mut buffer = Vec::new();
    file.take(limit as u64).read_to_end(&mut buffer).await?;
    Ok(buffer)
}

/// A manifest track as the CUE writer takes it, all in one file.
fn track_layout(track: &Track) -> cue::TrackLayout {
    cue::TrackLayout {
        number: track.number,
        session: track.session,
        mode: track.mode.clone(),
        file: 0,
        file_offset_bytes: track.file_offset_bytes,
        start_lba: track.start_lba,
        sector_count: track.sector_count,
        pregap_sectors: track.pregap_sectors,
        indexes: track
            .indexes
            .iter()
            .map(|index| (index.number, index.relative_lba))
            .collect(),
        isrc: track.isrc.clone(),
        sample_byte_order: track.sample_byte_order,
        flags: track.flags.clone(),
    }
}

/// A layout's tracks as a manifest records them, all in `component`.
fn manifest_tracks(layout: &cue::CdLayout, component: ComponentId) -> Vec<Track> {
    layout
        .tracks
        .iter()
        .map(|track| Track {
            number: track.number,
            session: track.session,
            mode: track.mode.clone(),
            component_id: component,
            file_offset_bytes: track.file_offset_bytes,
            start_lba: track.start_lba,
            sector_count: track.sector_count,
            pregap_sectors: track.pregap_sectors,
            isrc: track.isrc.clone(),
            sample_byte_order: track.sample_byte_order,
            flags: track.flags.clone(),
            indexes: track
                .indexes
                .iter()
                .map(|(number, relative_lba)| TrackIndex {
                    number: *number,
                    relative_lba: *relative_lba,
                })
                .collect(),
            hashes: std::collections::BTreeMap::new(),
        })
        .collect()
}

/// A CUE/BIN to be written: its tracks, the name its files take, the data
/// file that is its BIN, and what of the parent it does not carry.
struct CueBinDraft {
    laid: cue::CdLayout,
    stem: String,
    bin: Component,
    dropped: Vec<String>,
}

struct CueBinManifest<'a> {
    child: ArtifactId,
    job: &'a DerivationJobRecord,
    parent: &'a ArtifactManifest,
    planned: &'a PlannedDerivation,
    /// The sheet: its name, digest and length.
    cue: (LogicalPath, tangible_domain::Sha256Digest, u64),
    /// The BIN: its name, and the parent's IMG it is.
    bin: (LogicalPath, &'a Component),
    layout: &'a cue::CdLayout,
    notes: &'a [String],
    /// What it preserved.
    loss: LossCharacter,
    /// What its validator says of the sheet: written and read back the
    /// same, or chdman's, laid out.
    validator_code: &'static str,
    started: OffsetDateTime,
    completed: OffsetDateTime,
}

/// What a CHD extraction works with.
struct Extraction<'a> {
    area: &'a tangible_storage::StagingArea,
    job: &'a DerivationJobRecord,
    parent: &'a ArtifactManifest,
    planned: &'a PlannedDerivation,
    /// The CHD, as a component of the parent.
    input: &'a Component,
    /// Where the parent's components were put for the tool.
    input_dir: &'a Path,
}

/// A CHD's header and metadata, read from a file.
async fn read_chd(path: std::path::PathBuf) -> Result<chd::ChdDescription, DerivationError> {
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&path)?;
        let length = file.metadata()?.len();
        Ok::<_, std::io::Error>(chd::read(&mut std::io::BufReader::new(file), length))
    })
    .await
    .map_err(|e| DerivationError::Materialize(format!("could not read the CHD: {e}")))?
    .map_err(|e| DerivationError::Materialize(format!("could not read the CHD: {e}")))?
    .map_err(|e| not_expressible(format!("the CHD could not be read: {e}")))
}

/// A CUE/BIN derivative's two components: the sheet, a new object, and the
/// BIN, the parent's IMG under a new name.
fn cue_bin_components(input: &CueBinManifest<'_>) -> (Component, Component) {
    let (cue_name, cue_digest, cue_length) = &input.cue;
    let (bin_name, image) = &input.bin;
    let descriptor = Component {
        id: ComponentId::generate(),
        logical_path: cue_name.clone(),
        source_filename: Some(cue_name.as_str().to_owned()),
        role: ComponentRole::Descriptor,
        ordinal: 0,
        length_bytes: *cue_length,
        storage: StorageRef::for_digest(cue_digest),
        content: ContentHashes {
            sha256: *cue_digest,
            sha1: None,
            md5: None,
            crc32: None,
        },
        media_type: None,
        attributes: Extensions::new(),
    };
    let data = Component {
        id: ComponentId::generate(),
        logical_path: bin_name.clone(),
        source_filename: Some(bin_name.as_str().to_owned()),
        role: ComponentRole::TrackData,
        ordinal: 1,
        // The same stored object as the parent's IMG: nothing is copied.
        ..(*image).clone()
    };
    (descriptor, data)
}

/// A CUE/BIN derivative's manifest: the sheet and the BIN as components,
/// the tracks the sheet reads back as, and its lineage, lossy.
fn cue_bin_manifest(input: &CueBinManifest<'_>) -> ArtifactManifest {
    let (cue_name, _, cue_length) = &input.cue;
    let (descriptor, data) = cue_bin_components(input);
    let tracks = manifest_tracks(input.layout, data.id);
    let tool = &input.planned.spec.tool;
    let mut evidence = vec![format!(
        "made by {} {} from artifact {}",
        tool.name, tool.version, input.parent.artifact_id
    )];
    evidence.extend(input.notes.iter().cloned());
    let total_bytes = cue_length.saturating_add(data.length_bytes);
    ArtifactManifest {
        schema: SCHEMA_ID.to_owned(),
        artifact_id: input.child,
        created_at: input.completed,
        origin: Origin {
            kind: ArtifactOrigin::Derived,
            source_kind: "derivation".to_owned(),
            source_reference: Some(input.job.id.to_string()),
            source_filename: Some(cue_name.as_str().to_owned()),
            acquired_at: Some(input.completed),
            operator_note: None,
            physical_dump: None,
            container: None,
        },
        classification: Classification {
            artifact_kind: ArtifactKind::MultiFileImage,
            format: ArtifactFormat::CueBin,
            format_confidence: 1.0,
            detectors: vec![Detector {
                name: tool.name.clone(),
                version: tool.version.clone(),
                evidence,
            }],
            media_family: Some(MediaFamily::Cd),
            total_bytes,
        },
        topology: Topology::CdTracks {
            descriptor_component_id: descriptor.id,
            catalog: input.layout.catalog.clone(),
            session_count: input.layout.session_count,
            tracks,
            subchannel: Some(Subchannel {
                present: false,
                representation: None,
            }),
        },
        components: vec![descriptor, data],
        // Laid out by the CUE parser every import uses: written here and
        // read back the same, or chdman's.
        validation: Validation {
            state: ValidationState::Valid,
            validated_at: Some(input.completed),
            validators: vec![ValidatorResult {
                name: "cue-structural".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                result: "pass".to_owned(),
                code: input.validator_code.to_owned(),
                details: Extensions::new(),
            }],
        },
        lineage: Some(Lineage {
            parent_artifact_id: input.parent.artifact_id,
            transformation: input.planned.spec.transformation.to_string(),
            tool: tool.clone(),
            normalized_options: input
                .planned
                .spec
                .normalized_options()
                .into_iter()
                .collect(),
            command_fingerprint: input.planned.fingerprint.clone(),
            loss_character: input.loss,
            started_at: Some(input.started),
            completed_at: Some(input.completed),
        }),
        associations: vec![],
        compatibility: Compatibility {
            structurally_valid: true,
            burn_support: BurnSupport {
                // As for an imported CUE/BIN: no engine has been consulted.
                state: "unknown".to_owned(),
                profiles: vec![],
                engines: vec![],
                warnings: vec![],
            },
            target_claims: vec![],
        },
        extensions: Extensions::new(),
    }
}

struct IsoManifest<'a> {
    child: ArtifactId,
    job: &'a DerivationJobRecord,
    parent: &'a ArtifactManifest,
    planned: &'a PlannedDerivation,
    /// The ISO: its name, and the parent's data file it is.
    iso: (LogicalPath, &'a Component),
    inspection: &'a iso::IsoInspection,
    notes: &'a [String],
    /// What it preserved.
    loss: LossCharacter,
    started: OffsetDateTime,
    completed: OffsetDateTime,
}

/// An ISO derivative's manifest: the parent's data file under an `.iso`
/// name, described as the ISO inspector read it, and its lineage, lossy.
fn iso_manifest(input: &IsoManifest<'_>) -> ArtifactManifest {
    let (name, data) = &input.iso;
    let component = Component {
        id: ComponentId::generate(),
        logical_path: name.clone(),
        source_filename: Some(name.as_str().to_owned()),
        role: ComponentRole::PrimaryImage,
        ordinal: 0,
        // The same stored object as the parent's MDF: nothing is copied.
        ..(*data).clone()
    };
    let tool = &input.planned.spec.tool;
    let mut evidence = vec![format!(
        "made by {} {} from artifact {}",
        tool.name, tool.version, input.parent.artifact_id
    )];
    evidence.extend(input.notes.iter().cloned());
    evidence.extend(input.inspection.evidence.iter().cloned());
    let topology = iso_topology(input.inspection);
    let validators = iso_validators(input.inspection);
    ArtifactManifest {
        schema: SCHEMA_ID.to_owned(),
        artifact_id: input.child,
        created_at: input.completed,
        origin: Origin {
            kind: ArtifactOrigin::Derived,
            source_kind: "derivation".to_owned(),
            source_reference: Some(input.job.id.to_string()),
            source_filename: Some(name.as_str().to_owned()),
            acquired_at: Some(input.completed),
            operator_note: None,
            physical_dump: None,
            container: None,
        },
        classification: Classification {
            artifact_kind: ArtifactKind::SingleFileImage,
            format: ArtifactFormat::Iso,
            format_confidence: 1.0,
            detectors: vec![Detector {
                name: tool.name.clone(),
                version: tool.version.clone(),
                evidence,
            }],
            media_family: input.parent.classification.media_family,
            total_bytes: component.length_bytes,
        },
        components: vec![component],
        topology,
        validation: Validation {
            state: if input.inspection.warnings.is_empty() {
                ValidationState::Valid
            } else {
                ValidationState::ValidWithWarnings
            },
            validated_at: Some(input.completed),
            validators,
        },
        lineage: Some(Lineage {
            parent_artifact_id: input.parent.artifact_id,
            transformation: input.planned.spec.transformation.to_string(),
            tool: tool.clone(),
            normalized_options: input
                .planned
                .spec
                .normalized_options()
                .into_iter()
                .collect(),
            command_fingerprint: input.planned.fingerprint.clone(),
            loss_character: input.loss,
            started_at: Some(input.started),
            completed_at: Some(input.completed),
        }),
        associations: vec![],
        compatibility: Compatibility {
            structurally_valid: true,
            burn_support: BurnSupport {
                // As for an imported ISO: no engine has been consulted.
                state: "unknown".to_owned(),
                profiles: vec![],
                engines: vec![],
                warnings: vec![],
            },
            target_claims: vec![],
        },
        extensions: Extensions::new(),
    }
}

/// An ISO's topology, as the inspector read its primary volume.
fn iso_topology(inspection: &iso::IsoInspection) -> Topology {
    inspection
        .primary
        .as_ref()
        .map_or(Topology::Unknown, |primary| {
            Topology::SingleTrackBlockImage {
                block_size: primary.block_size,
                block_count: primary.block_count,
                volume_labels: Some(primary.volume_id.clone())
                    .filter(|label| !label.is_empty())
                    .into_iter()
                    .collect(),
                filesystems: inspection
                    .filesystems
                    .iter()
                    .map(|f| tangible_domain::manifest::FilesystemEvidence {
                        filesystem_type: f.name.to_owned(),
                        version: f.version.clone(),
                        evidence: f.evidence.clone(),
                    })
                    .collect(),
            }
        })
}

/// An ISO's validators, as an imported ISO's: what the inspector warned of,
/// it says.
fn iso_validators(inspection: &iso::IsoInspection) -> Vec<ValidatorResult> {
    let result = |result: &str, code: &str, details: Extensions| ValidatorResult {
        name: "iso-structural".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        result: result.to_owned(),
        code: code.to_owned(),
        details,
    };
    if inspection.warnings.is_empty() {
        return vec![result("pass", "ISO_PRIMARY_VOLUME_READ", Extensions::new())];
    }
    inspection
        .warnings
        .iter()
        .map(|warning| {
            let mut details = Extensions::new();
            details.insert("message".to_owned(), serde_json::json!(warning.to_string()));
            result("warning", "ISO_STRUCTURAL_WARNING", details)
        })
        .collect()
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
            physical_dump: None,
            container: None,
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
            lease: Duration::from_hours(4),
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
    romm: Option<crate::romm_export::RommExport>,
    identification: Option<crate::identification::IdentificationHandle>,
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
            romm: None,
            identification: None,
        }
    }

    /// Wake the identification runner after each derivative, so its hashes
    /// are computed at once rather than at the next poll.
    #[must_use]
    pub fn with_identification(
        mut self,
        identification: crate::identification::IdentificationHandle,
    ) -> Self {
        self.identification = Some(identification);
        self
    }

    /// Wake the RomM exporter after each derivative, so a game already in
    /// RomM switches to its new CHD at once rather than at the next pass.
    #[must_use]
    pub fn with_romm(mut self, romm: crate::romm_export::RommExport) -> Self {
        self.romm = Some(romm);
        self
    }

    /// Work the queue until `shutdown` resolves. A job already running
    /// finishes first.
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
                        tracing::error!(error = ?error, "the derivation queue could not be read");
                        false
                    }
                }
            },
            || async {
                tokio::select! {
                    () = self.derivations.nudge.notified() => {}
                    () = tokio::time::sleep(self.settings.poll_interval) => {}
                }
            },
        )
        .await;
        tracing::info!("derivation runner stopped");
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
        // Held until the derivative is registered, as an import holds it
        // (ADR 0020).
        let _ingesting = pipeline.manifests().objects().ingesting().await;
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
            Ok(true) => {
                tracing::info!(
                    job_id = %job.id,
                    child = %outcome.child_artifact_id,
                    loss = %outcome.loss_character,
                    "derivation complete"
                );
                if let Some(romm) = &self.romm {
                    romm.nudge();
                }
                if let Some(identification) = &self.identification {
                    identification.nudge();
                }
            }
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
    fn what_is_not_carried_reads_as_a_sentence() {
        let list =
            |items: &[&str]| listed(&items.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
        assert_eq!(list(&[]), "");
        assert_eq!(list(&["a"]), "a");
        assert_eq!(list(&["a", "b"]), "a and b");
        assert_eq!(list(&["a", "b", "c"]), "a, b and c");
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
