// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Import orchestration.
//!
//! Drives one import through its stages: acquire into staging, hash, inspect,
//! register into the object store, publish a manifest.
//!
//! This lives in the api crate because it is the only crate below the
//! composition root permitted to depend on both storage and image, which the
//! layout keeps as siblings. It is a service rather than a route; nothing here
//! touches HTTP.
//!
//! # Restart behaviour
//!
//! Every stage records what it accomplished in an [`ImportCheckpoint`], which
//! is serializable so it can be persisted with the job. A restart resumes from
//! the last completed stage rather than the beginning; re-downloading
//! gigabytes because the process died during inspection would be a poor
//! trade, and re-promoting bytes already in the store is wasted work rather
//! than a correctness problem.
//!
//! Resumption is safe because every stage is idempotent. Promotion
//! deduplicates on digest, so re-running it returns the same object. Manifest
//! publication is an atomic replace. The only stage that cannot simply be
//! repeated is acquisition, which is why a failure there restarts rather than
//! resumes.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tangible_domain::enums::{
    ArtifactFormat, ArtifactKind, ArtifactOrigin, ComponentRole, MediaFamily, ValidationState,
};
use tangible_domain::manifest::{
    ArtifactManifest, BurnSupport, Classification, Compatibility, Component, ContentHashes,
    Detector, Extensions, FilesystemEvidence as ManifestFilesystem, Origin, SCHEMA_ID, StorageRef,
    Subchannel, Topology, Track, TrackIndex, Validation, ValidatorResult,
};
use tangible_domain::{
    ArtifactId, ComponentId, ImportJobId, ImportState, ImportTransitionError, LogicalPath,
    Sha256Digest,
};
use tangible_image::{cue, iso, toc};
use tangible_storage::{
    FilesystemStore, IngestLimits, ManifestStore, ManifestStoreError, StagingArea, StagingError,
    StagingKind, StagingManager, StorageError,
};
use time::OffsetDateTime;

/// What the caller knows about an import before it runs.
#[derive(Debug, Clone)]
pub struct ImportRequest {
    /// Identity of the job.
    pub import_id: ImportJobId,
    /// How the bytes are arriving, e.g. `upload`.
    pub source_kind: String,
    /// Filename as received, if any. Used as a detection hint only.
    pub source_filename: Option<String>,
    /// A reference safe to record in a published manifest.
    pub source_reference: Option<String>,
    /// Limits applied when promoting into the object store.
    pub limits: IngestLimits,
}

/// What one component contributed, recorded so a restart need not redo it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromotedComponent {
    /// Digest the bytes were filed under.
    pub digest: Sha256Digest,
    /// Length in bytes.
    pub length_bytes: u64,
}

/// Progress through an import, persistable with the job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportCheckpoint {
    /// The stage reached.
    pub state: ImportState,
    /// Components promoted so far, keyed by logical path.
    ///
    /// A map rather than a list so resumption is a lookup: a component already
    /// present is skipped without needing to know the order things ran in.
    pub promoted: BTreeMap<String, PromotedComponent>,
    /// Artifact identity, allocated once and reused across restarts.
    ///
    /// Allocated at registration and kept, so a resumed import publishes to
    /// the same manifest path rather than creating a second artifact for the
    /// same bytes.
    pub artifact_id: Option<ArtifactId>,
    /// Detection verdict, once inspection has run.
    pub detected_format: Option<ArtifactFormat>,
    /// Detection confidence.
    pub confidence: f32,
    /// Structural observations gathered so far.
    pub warnings: Vec<String>,
    /// Detection evidence.
    pub evidence: Vec<String>,
}

impl Default for ImportCheckpoint {
    fn default() -> Self {
        Self {
            state: ImportState::Requested,
            promoted: BTreeMap::new(),
            artifact_id: None,
            detected_format: None,
            confidence: 0.0,
            warnings: Vec::new(),
            evidence: Vec::new(),
        }
    }
}

/// A completed import.
#[derive(Debug, Clone)]
pub struct ImportOutcome {
    /// The registered artifact.
    pub artifact_id: ArtifactId,
    /// Where the manifest was published.
    pub manifest_path: PathBuf,
    /// Final state, always [`ImportState::Complete`].
    pub state: ImportState,
    /// Structural observations an operator should see.
    pub warnings: Vec<String>,
}

/// Why an import failed.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// The staging area held no files.
    #[error("import {import_id} staged no files")]
    NothingStaged {
        /// The job.
        import_id: String,
    },

    /// A staging operation failed.
    #[error(transparent)]
    Staging(#[from] StagingError),

    /// An object-store operation failed.
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// Manifest publication failed.
    #[error(transparent)]
    Manifest(#[from] ManifestStoreError),

    /// A state transition was refused.
    #[error("import state transition refused")]
    Transition(#[source] ImportTransitionError),

    /// A staged path could not be read.
    #[error("could not read staged file {path}")]
    ReadStaged {
        /// The path.
        path: String,
        /// Cause.
        #[source]
        source: std::io::Error,
    },
}

/// Runs imports.
#[derive(Debug, Clone)]
pub struct ImportPipeline {
    staging: StagingManager,
    objects: FilesystemStore,
    manifests: ManifestStore,
}

impl ImportPipeline {
    /// Assemble a pipeline from its collaborators.
    #[must_use]
    pub const fn new(
        staging: StagingManager,
        objects: FilesystemStore,
        manifests: ManifestStore,
    ) -> Self {
        Self {
            staging,
            objects,
            manifests,
        }
    }

    /// The staging manager, for a caller that needs to place bytes first.
    #[must_use]
    pub const fn staging(&self) -> &StagingManager {
        &self.staging
    }

    /// The object store.
    #[must_use]
    pub const fn objects(&self) -> &FilesystemStore {
        &self.objects
    }

    /// The manifest store.
    #[must_use]
    pub const fn manifests(&self) -> &ManifestStore {
        &self.manifests
    }

    /// Open, or reopen, the staging area for a job.
    ///
    /// # Errors
    ///
    /// [`ImportError::Staging`] if the area cannot be created.
    pub async fn open_area(&self, import_id: ImportJobId) -> Result<StagingArea, ImportError> {
        Ok(self
            .staging
            .create(StagingKind::Imports, &import_id.to_string())
            .await?)
    }

    /// Run an import from staged bytes to a published manifest.
    ///
    /// The checkpoint is advanced in place, so a caller persisting it after
    /// each call can resume by passing the same value back. Calling this again
    /// with a completed checkpoint is a no-op that returns the same outcome.
    ///
    /// # Errors
    ///
    /// Whichever [`ImportError`] the failing stage produces. The checkpoint
    /// still reflects the last stage that succeeded, so the call can be
    /// retried.
    pub async fn run(
        &self,
        request: &ImportRequest,
        checkpoint: &mut ImportCheckpoint,
    ) -> Result<ImportOutcome, ImportError> {
        let area = self
            .staging
            .reopen(StagingKind::Imports, &request.import_id.to_string())
            .await?;

        // --- staged -------------------------------------------------------
        let staged = area.entries().await?;
        if staged.is_empty() {
            return Err(ImportError::NothingStaged {
                import_id: request.import_id.to_string(),
            });
        }
        advance_to(checkpoint, ImportState::Staged)?;

        // --- hashing ------------------------------------------------------
        // Promotion computes the digest from the bytes actually read, so this
        // is both the hash step and the move into canonical storage. Anything
        // already promoted is skipped, which is what makes a restart cheap.
        for path in &staged {
            if checkpoint.promoted.contains_key(path.as_str()) {
                continue;
            }
            let promoted = area.promote(path, &self.objects, request.limits).await?;
            checkpoint.promoted.insert(
                path.as_str().to_owned(),
                PromotedComponent {
                    digest: promoted.digest,
                    length_bytes: promoted.size_bytes,
                },
            );
        }
        advance_to(checkpoint, ImportState::Hashing)?;

        // --- inspecting ---------------------------------------------------
        // Run on every attempt rather than only the first. Inspection is a
        // bounded read of a descriptor, and the structure it works out is what
        // the manifest is built from; caching only the verdict would leave a
        // resumed import publishing a manifest with no track layout in it.
        // What the checkpoint records is the *first* verdict, so a resumed run
        // cannot append the same warnings twice.
        let detection = self.inspect(&area, &staged, request, checkpoint).await?;
        if checkpoint.detected_format.is_none() {
            checkpoint.detected_format = Some(detection.format);
            checkpoint.confidence = detection.confidence;
            checkpoint.warnings.extend(detection.warnings.clone());
            checkpoint.evidence.extend(detection.evidence.clone());
        }
        advance_to(checkpoint, ImportState::Inspecting)?;

        // --- registering ---------------------------------------------------
        // The identity is allocated once and kept in the checkpoint, so a
        // resumed import republishes the same artifact rather than creating a
        // second one for the same bytes.
        let artifact_id = *checkpoint
            .artifact_id
            .get_or_insert_with(ArtifactId::generate);
        advance_to(checkpoint, ImportState::Registering)?;

        let manifest = Self::build_manifest(artifact_id, request, &staged, checkpoint, &detection);
        let manifest_path = self.manifests.write(&manifest).await?;
        advance_to(checkpoint, ImportState::Complete)?;

        Ok(ImportOutcome {
            artifact_id,
            manifest_path,
            state: checkpoint.state,
            warnings: checkpoint.warnings.clone(),
        })
    }

    /// Classify what was staged.
    ///
    /// A descriptor decides the answer when there is one, because a CUE sheet
    /// beside a BIN describes the BIN: classifying the largest file on its own
    /// would report a headerless blob and throw away the only thing that says
    /// what the blob is. Everything else falls through to reading the image
    /// itself.
    async fn inspect(
        &self,
        area: &StagingArea,
        staged: &[LogicalPath],
        request: &ImportRequest,
        checkpoint: &ImportCheckpoint,
    ) -> Result<Detection, ImportError> {
        let descriptors: Vec<&LogicalPath> = staged
            .iter()
            .filter(|path| matches!(path.extension().as_deref(), Some("cue" | "toc")))
            .collect();

        match descriptors.as_slice() {
            [] => self.inspect_iso(area, staged, request).await,
            [only] if only.extension().as_deref() == Some("toc") => {
                match self.inspect_toc(area, staged, only, checkpoint).await? {
                    Some(detection) => Ok(detection),
                    // Named `.toc` and not one: imported, not claimed.
                    None => self.inspect_iso(area, staged, request).await,
                }
            }
            [only] => match self.inspect_cue(area, staged, only, checkpoint).await? {
                Some(detection) => Ok(detection),
                // A file named `.cue` that is not one. The bytes are still
                // imported; they are simply not claimed to be a sheet.
                None => self.inspect_iso(area, staged, request).await,
            },
            many => {
                // Two discs staged together. Which BIN belongs to which sheet
                // is a grouping question, and answering it by guessing would
                // produce a manifest whose tracks point at another disc.
                let mut detection = self.inspect_iso(area, staged, request).await?;
                detection.warnings.push(format!(
                    "{} descriptors were staged together, so no track layout was built; \
                     import one disc at a time to get one",
                    many.len()
                ));
                Ok(detection)
            }
        }
    }

    /// Read a CUE sheet and lay out the tracks it describes.
    ///
    /// Returns `None` when the descriptor does not parse, which leaves the
    /// caller to classify the staged bytes some other way.
    async fn inspect_cue(
        &self,
        area: &StagingArea,
        staged: &[LogicalPath],
        descriptor: &LogicalPath,
        checkpoint: &ImportCheckpoint,
    ) -> Result<Option<Detection>, ImportError> {
        let resolved = area.resolve(descriptor).await?;
        // One byte past the limit, so a file that is too large is refused by
        // the parser rather than silently truncated into a valid-looking sheet.
        let bytes = read_prefix(&resolved, cue::MAX_CUE_BYTES + 1)
            .await
            .map_err(|source| ImportError::ReadStaged {
                path: descriptor.to_string(),
                source,
            })?;
        let Ok(sheet) = cue::parse(&bytes) else {
            return Ok(None);
        };

        let mut detection = Detection::plain(ArtifactFormat::CueBin, sheet.confidence);
        detection.descriptor = Some(descriptor.clone());
        detection.evidence.clone_from(&sheet.evidence);
        detection.warnings = sheet.warnings.iter().map(ToString::to_string).collect();

        // The sheet names files; the resolver turns those names into staged
        // files or into nothing. Nothing here can reach a file that was not
        // staged, whatever the sheet says.
        let resolution = cue::resolve_references(&sheet, staged);
        detection
            .warnings
            .extend(resolution.warnings.iter().map(ToString::to_string));
        for file in &resolution.files {
            if let Some(failure) = &file.failure {
                detection
                    .warnings
                    .push(format!("{} could not be used: {failure}", file.declared));
            }
        }

        let Some(paths) = resolution.paths() else {
            return Ok(Some(detection));
        };
        detection
            .evidence
            .push(format!("{} referenced file(s) resolved", paths.len()));

        // Sizes come from what was promoted rather than from the filesystem:
        // those are the bytes that were hashed and stored, and a layout
        // computed against anything else would describe a different disc.
        let sizes: Vec<u64> = paths
            .iter()
            .map(|path| {
                checkpoint
                    .promoted
                    .get(path.as_str())
                    .map_or(0, |promoted| promoted.length_bytes)
            })
            .collect();

        match cue::layout(&sheet, &sizes) {
            Ok(layout) => {
                detection
                    .warnings
                    .extend(layout.warnings.iter().map(ToString::to_string));
                detection.evidence.push(format!(
                    "{} track(s) over {} sectors in {} session(s)",
                    layout.tracks.len(),
                    layout.total_sectors(),
                    layout.session_count
                ));
                // The sheet parsed and every file it names is here. That is as
                // sure as detection gets; it is still not validation.
                detection.confidence = 0.99;
                detection.layout = Some(layout);
                detection.track_files = paths;
            }
            Err(error) => detection
                .warnings
                .push(format!("the tracks could not be laid out: {error}")),
        }

        Ok(Some(detection))
    }

    /// Read a cdrdao table of contents and lay out the tracks it describes.
    ///
    /// The same shape as [`Self::inspect_cue`], and through the same
    /// resolver: the files a TOC names are matched against what was staged,
    /// and nothing it says is ever opened. A TOC names a file per statement,
    /// often one file from every track, so each distinct name is resolved
    /// once.
    async fn inspect_toc(
        &self,
        area: &StagingArea,
        staged: &[LogicalPath],
        descriptor: &LogicalPath,
        checkpoint: &ImportCheckpoint,
    ) -> Result<Option<Detection>, ImportError> {
        let resolved = area.resolve(descriptor).await?;
        let bytes = read_prefix(&resolved, toc::MAX_TOC_BYTES + 1)
            .await
            .map_err(|source| ImportError::ReadStaged {
                path: descriptor.to_string(),
                source,
            })?;
        let Ok(document) = toc::parse(&bytes) else {
            return Ok(None);
        };

        let mut detection = Detection::plain(ArtifactFormat::TocBin, document.confidence);
        detection.descriptor = Some(descriptor.clone());
        detection.evidence.clone_from(&document.evidence);
        detection.warnings = document.warnings.iter().map(ToString::to_string).collect();

        let resolution = cue::resolve_names(&document.files, staged);
        detection
            .warnings
            .extend(resolution.warnings.iter().map(ToString::to_string));
        for file in &resolution.files {
            if let Some(failure) = &file.failure {
                detection
                    .warnings
                    .push(format!("{} could not be used: {failure}", file.declared));
            }
        }
        let Some(paths) = resolution.paths() else {
            return Ok(Some(detection));
        };
        detection
            .evidence
            .push(format!("{} referenced file(s) resolved", paths.len()));

        let sizes: Vec<u64> = paths
            .iter()
            .map(|path| {
                checkpoint
                    .promoted
                    .get(path.as_str())
                    .map_or(0, |promoted| promoted.length_bytes)
            })
            .collect();

        match toc::layout(&document, &sizes) {
            Ok(laid) => {
                detection
                    .warnings
                    .extend(laid.warnings.iter().map(ToString::to_string));
                detection.evidence.push(format!(
                    "{} track(s) over {} sectors",
                    laid.layout.tracks.len(),
                    laid.layout.total_sectors()
                ));
                detection.confidence = 0.99;
                detection.layout = Some(laid.layout);
                detection.track_files = paths;
            }
            Err(error) => detection
                .warnings
                .push(format!("the tracks could not be laid out: {error}")),
        }

        Ok(Some(detection))
    }

    /// Read enough of the primary component to classify it.
    async fn inspect_iso(
        &self,
        area: &StagingArea,
        staged: &[LogicalPath],
        request: &ImportRequest,
    ) -> Result<Detection, ImportError> {
        // The largest staged file is the payload; a descriptor sitting beside
        // it is small by definition.
        let mut primary = &staged[0];
        let mut largest = 0_u64;
        for path in staged {
            let resolved = area.resolve(path).await?;
            let length = tokio::fs::metadata(&resolved)
                .await
                .map_or(0, |metadata| metadata.len());
            if length >= largest {
                largest = length;
                primary = path;
            }
        }

        let resolved = area.resolve(primary).await?;
        let prefix = read_prefix(&resolved, iso::REQUIRED_PREFIX_BYTES)
            .await
            .map_err(|source| ImportError::ReadStaged {
                path: primary.to_string(),
                source,
            })?;

        let hint = request
            .source_filename
            .as_deref()
            .and_then(|name| {
                name.rsplit_once('.')
                    .map(|(_, ext)| ext.to_ascii_lowercase())
            })
            .or_else(|| primary.extension());

        let inspection = iso::inspect(&prefix, largest, hint.as_deref());

        let mut warnings: Vec<String> = inspection
            .warnings
            .iter()
            .map(ToString::to_string)
            .collect();
        if inspection.has_integrity_warning() {
            warnings.push(
                "the image reports an integrity problem; it is preserved exactly as received"
                    .to_owned(),
            );
        }

        Ok(Detection {
            format: if inspection.is_iso9660 {
                ArtifactFormat::Iso
            } else {
                // Honest rather than guessed. An unrecognised image is still
                // imported and preserved; it simply is not claimed to be
                // something it was not shown to be.
                ArtifactFormat::Unknown
            },
            confidence: inspection.confidence,
            warnings,
            evidence: inspection.evidence,
            filesystems: inspection
                .filesystems
                .iter()
                .map(|f| ManifestFilesystem {
                    filesystem_type: f.name.to_owned(),
                    version: f.version.clone(),
                    evidence: f.evidence.clone(),
                })
                .collect(),
            block_size: inspection.primary.as_ref().map(|p| p.block_size),
            block_count: inspection.primary.as_ref().map(|p| p.block_count),
            volume_label: inspection.primary.as_ref().map(|p| p.volume_id.clone()),
            descriptor: None,
            track_files: Vec::new(),
            layout: None,
        })
    }

    /// Turn the recorded promotions into manifest components.
    fn build_components(
        request: &ImportRequest,
        staged: &[LogicalPath],
        checkpoint: &ImportCheckpoint,
        detection: &Detection,
    ) -> (Vec<Component>, u64) {
        let mut components = Vec::new();
        let mut total_bytes = 0_u64;
        for (ordinal, path) in staged.iter().enumerate() {
            let Some(promoted) = checkpoint.promoted.get(path.as_str()) else {
                continue;
            };
            total_bytes = total_bytes.saturating_add(promoted.length_bytes);
            components.push(Component {
                id: ComponentId::generate(),
                logical_path: path.clone(),
                source_filename: request.source_filename.clone(),
                role: if detection.descriptor.as_ref() == Some(path) {
                    ComponentRole::Descriptor
                } else if detection.track_files.contains(path) {
                    ComponentRole::TrackData
                } else if staged.len() == 1 {
                    ComponentRole::PrimaryImage
                } else {
                    // Staged alongside the artifact and not named by anything
                    // that describes it. Preserved, and not given a role it
                    // was not shown to have.
                    ComponentRole::Unknown
                },
                ordinal: u32::try_from(ordinal).unwrap_or(u32::MAX),
                length_bytes: promoted.length_bytes,
                storage: StorageRef::for_digest(&promoted.digest),
                content: ContentHashes {
                    sha256: promoted.digest,
                    sha1: None,
                    md5: None,
                    crc32: None,
                },
                media_type: None,
                attributes: Extensions::new(),
            });
        }
        (components, total_bytes)
    }

    /// Describe the artifact's structure, when inspection worked one out.
    ///
    /// Topology is what makes a manifest portable: another tool reading it
    /// learns where the tracks are without re-deriving them from a descriptor
    /// it may not parse the same way. Anything not established is
    /// [`Topology::Unknown`] rather than a plausible-looking default, because
    /// a wrong offset here is a wrongly written disc.
    fn build_topology(detection: &Detection, components: &[Component]) -> Topology {
        if let Some(layout) = &detection.layout
            && let Some(descriptor) = &detection.descriptor
            && let Some(descriptor_component_id) = component_id(components, descriptor)
        {
            let mut tracks = Vec::with_capacity(layout.tracks.len());
            for track in &layout.tracks {
                // A track whose file is not a component would be a manifest
                // pointing at bytes it does not carry. Better to describe
                // nothing than to describe that.
                let Some(component) = detection
                    .track_files
                    .get(track.file)
                    .and_then(|path| component_id(components, path))
                else {
                    return Topology::Unknown;
                };
                tracks.push(Track {
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
                    hashes: BTreeMap::new(),
                });
            }
            return Topology::CdTracks {
                descriptor_component_id,
                catalog: layout.catalog.clone(),
                session_count: layout.session_count,
                tracks,
                // A CUE/BIN set carries no subchannel data. Formats that do
                // are a separate descriptor and a separate detector.
                subchannel: Some(Subchannel {
                    present: false,
                    representation: None,
                }),
            };
        }

        if let (Some(block_size), Some(block_count)) = (detection.block_size, detection.block_count)
        {
            return Topology::SingleTrackBlockImage {
                block_size,
                block_count,
                volume_labels: detection
                    .volume_label
                    .iter()
                    .filter(|label| !label.is_empty())
                    .cloned()
                    .collect(),
                filesystems: detection.filesystems.clone(),
            };
        }

        Topology::Unknown
    }

    /// Assemble the manifest from what the checkpoint recorded.
    fn build_manifest(
        artifact_id: ArtifactId,
        request: &ImportRequest,
        staged: &[LogicalPath],
        checkpoint: &ImportCheckpoint,
        detection: &Detection,
    ) -> ArtifactManifest {
        let format = checkpoint
            .detected_format
            .unwrap_or(ArtifactFormat::Unknown);

        // Named for what actually read the bytes, so a warning in a manifest
        // can be traced to the thing that produced it.
        let (detector, validator, warning_code, media_family) = match format {
            ArtifactFormat::CueBin => (
                "cue-parser",
                "cue-structural",
                "CUE_STRUCTURAL_WARNING",
                // A CUE sheet describes a CD; there is no other medium it is
                // used for. An ISO could be any of three, which is why the
                // other arm claims nothing.
                Some(MediaFamily::Cd),
            ),
            // So does a cdrdao table of contents.
            ArtifactFormat::TocBin => (
                "toc-parser",
                "toc-structural",
                "TOC_STRUCTURAL_WARNING",
                Some(MediaFamily::Cd),
            ),
            _ => ("iso9660", "iso-structural", "ISO_STRUCTURAL_WARNING", None),
        };

        let (components, total_bytes) =
            Self::build_components(request, staged, checkpoint, detection);
        let topology = Self::build_topology(detection, &components);

        // Warnings do not block: real preservation dumps carry benign
        // structural oddities, and refusing them would make the tool useless
        // for its purpose. An integrity problem is a different matter and is
        // surfaced through the validator entry below.
        let state = if checkpoint.warnings.is_empty() {
            ValidationState::Valid
        } else {
            ValidationState::ValidWithWarnings
        };

        let validators = checkpoint
            .warnings
            .iter()
            .map(|warning| ValidatorResult {
                name: validator.to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                result: "warning".to_owned(),
                code: warning_code.to_owned(),
                details: {
                    let mut details = Extensions::new();
                    details.insert("message".to_owned(), serde_json::json!(warning));
                    details
                },
            })
            .collect();

        ArtifactManifest {
            schema: SCHEMA_ID.to_owned(),
            artifact_id,
            created_at: OffsetDateTime::now_utc(),
            origin: Origin {
                kind: ArtifactOrigin::ImportedOriginal,
                source_kind: request.source_kind.clone(),
                source_reference: request.source_reference.clone(),
                source_filename: request.source_filename.clone(),
                acquired_at: Some(OffsetDateTime::now_utc()),
                operator_note: None,
            },
            classification: Classification {
                artifact_kind: if components.len() == 1 {
                    ArtifactKind::SingleFileImage
                } else {
                    ArtifactKind::MultiFileImage
                },
                format,
                format_confidence: checkpoint.confidence,
                detectors: vec![Detector {
                    name: detector.to_owned(),
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                    evidence: checkpoint.evidence.clone(),
                }],
                media_family,
                total_bytes,
            },
            components,
            topology,
            validation: Validation {
                state,
                validated_at: Some(OffsetDateTime::now_utc()),
                validators,
            },
            lineage: None,
            associations: vec![],
            compatibility: Compatibility {
                structurally_valid: state.permits_burning(),
                burn_support: BurnSupport {
                    // Detection is not a burn plan. Claiming support here
                    // would promise something no engine has been consulted
                    // about.
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

    /// Discard an import's staging area once it is fully registered.
    ///
    /// Separate from [`ImportPipeline::run`] so a caller can inspect the
    /// staged bytes after a failure, which is the only diagnostic evidence a
    /// bad import leaves.
    ///
    /// # Errors
    ///
    /// [`ImportError::Staging`] if removal fails.
    pub async fn discard(&self, import_id: ImportJobId) -> Result<bool, ImportError> {
        Ok(self
            .staging
            .discard(StagingKind::Imports, &import_id.to_string())
            .await?)
    }
}

/// The identifier of the component holding a given staged file.
fn component_id(components: &[Component], path: &LogicalPath) -> Option<ComponentId> {
    components
        .iter()
        .find(|component| &component.logical_path == path)
        .map(|component| component.id)
}

/// What inspection concluded, in manifest terms.
struct Detection {
    format: ArtifactFormat,
    confidence: f32,
    warnings: Vec<String>,
    evidence: Vec<String>,
    filesystems: Vec<ManifestFilesystem>,
    block_size: Option<u32>,
    block_count: Option<u64>,
    volume_label: Option<String>,
    /// The descriptor, for formats that have one.
    descriptor: Option<LogicalPath>,
    /// The files the descriptor names, in its own declaration order.
    ///
    /// Indexed by [`cue::TrackLayout::file`], which is why the order matters
    /// and why it is the resolver's order rather than the staged order.
    track_files: Vec<LogicalPath>,
    /// Track extents, when the descriptor could be laid out.
    layout: Option<cue::CdLayout>,
}

impl Detection {
    /// A detection that claims nothing beyond the format.
    fn plain(format: ArtifactFormat, confidence: f32) -> Self {
        Self {
            format,
            confidence,
            warnings: Vec::new(),
            evidence: Vec::new(),
            filesystems: Vec::new(),
            block_size: None,
            block_count: None,
            volume_label: None,
            descriptor: None,
            track_files: Vec::new(),
            layout: None,
        }
    }
}

/// Move the checkpoint forward to `target`, one stage at a time.
///
/// Walks the happy path rather than jumping, because the state machine
/// deliberately refuses skipped stages: registering without hashing would put
/// unverified bytes in the store, and the machine is what stops that. The
/// pipeline passes through every intermediate state even when it has nothing
/// to do there.
///
/// Reaching a stage already passed is a no-op, which is what makes a resumed
/// run idempotent: re-running a completed import returns its outcome instead
/// of failing.
fn advance_to(checkpoint: &mut ImportCheckpoint, target: ImportState) -> Result<(), ImportError> {
    // A job that failed or was cancelled must be explicitly retried, which
    // resets the state to the stage the retry re-enters. Silently resuming it
    // here would bypass that decision.
    if checkpoint.state.is_awaiting_retry()
        || (checkpoint.state.is_terminal() && checkpoint.state != ImportState::Complete)
    {
        return Err(ImportError::Transition(ImportTransitionError::Invalid {
            from: checkpoint.state,
            to: target,
        }));
    }

    // Happy-path states are declared in order, so this covers "already here"
    // and "already past" together.
    if checkpoint.state >= target {
        return Ok(());
    }

    while checkpoint.state < target {
        let Some(next) = checkpoint.state.next_on_success() else {
            return Err(ImportError::Transition(ImportTransitionError::Invalid {
                from: checkpoint.state,
                to: target,
            }));
        };
        checkpoint.state = next;
    }
    Ok(())
}

/// Read at most `limit` bytes from the head of a file.
async fn read_prefix(path: &std::path::Path, limit: usize) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;
    let mut file = tokio::fs::File::open(path).await?;
    let mut buffer = Vec::new();
    let mut handle = (&mut file).take(limit as u64);
    handle.read_to_end(&mut buffer).await?;
    Ok(buffer)
}
