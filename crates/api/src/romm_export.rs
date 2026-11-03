// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Keeping a RomM library in step with the catalog.
//!
//! The exporter is a background task, like the import runner. It looks at
//! every edition switched on for RomM, and every one switched off whose
//! folder is still there, and makes the folder match: written, rebuilt,
//! renamed or taken out. It runs every few minutes and whenever something
//! that changes an export happens, such as a setting changing or a disc
//! being linked.
//!
//! The state that matters lives in the database, the edition's setting and
//! what the export last wrote, so a restart loses nothing: the next pass
//! sees what is wanted and what is there, and closes the gap.
//!
//! The work on files is `tangible_integrations::romm`; this module decides
//! what to export from the catalog and the manifests, and records what
//! happened.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tangible_db::Database;
use tangible_db::romm::{
    DiscInputs, LinkedArtifact, Receipt, chd_verdicts, edition_romm, editions_to_reconcile,
    export_inputs, fill_receipt_lengths, record_chd_verdict, record_folder_removed,
    record_receipts, record_status,
};
use tangible_db::romm_api::owe_scans;
use tangible_domain::enums::ComponentRole;
use tangible_domain::manifest::ArtifactManifest;
use tangible_domain::{ArtifactFormat, ArtifactId, EditionId, LossCharacter};
use tangible_integrations::romm::{
    self, Applied, ApplyError, DiscImage, ExportRequest, SourceFile,
};
use tangible_storage::ManifestStore;
use tokio::sync::Notify;

use crate::romm_sync::RommSyncHandle;

/// How often every export is checked when nothing prompts it sooner.
const INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Largest CUE sheet read for rewriting, as the importer bounds it.
const MAX_SHEET_BYTES: u64 = 1 << 20;

/// The export, as the routes see it: whether it is configured, and how to
/// prompt it.
#[derive(Debug, Clone)]
pub struct RommExport {
    root: PathBuf,
    nudge: Arc<Notify>,
    sync: Option<RommSyncHandle>,
}

impl RommExport {
    /// An export into `root`, RomM's `roms` directory.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            nudge: Arc::new(Notify::new()),
            sync: None,
        }
    }

    /// Keep RomM in step through its API with `sync`: prompted when the
    /// exporter changes folders, and when RomM's API is configured.
    #[must_use]
    pub fn with_sync(mut self, sync: RommSyncHandle) -> Self {
        self.sync = Some(sync);
        self
    }

    /// The platforms the connected RomM knows, as the task keeping RomM in
    /// step last read them.
    #[must_use]
    pub fn known_platforms(&self) -> Option<Vec<String>> {
        self.sync.as_ref().and_then(RommSyncHandle::known_platforms)
    }

    /// Ask the task keeping RomM in step through its API to look again now.
    pub fn nudge_sync(&self) {
        if let Some(sync) = &self.sync {
            sync.nudge();
        }
    }

    /// Ask the exporter to look again now rather than at its next interval.
    pub fn nudge(&self) {
        self.nudge.notify_one();
    }

    /// The export root.
    #[must_use]
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }
}

/// Runs `chdman verify` on a CHD imported as it is, which the export sends
/// as it is once it has passed (owner decision 2026-11-02, OWNER_QUEUE 14).
pub trait ChdVerifier: Send + Sync + std::fmt::Debug {
    /// The version a verdict is recorded under.
    fn tool_version(&self) -> String;

    /// Check `chd`, only reading it. `Err` says why it did not verify.
    fn verify<'a>(
        &'a self,
        chd: &'a std::path::Path,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

impl ChdVerifier for tangible_image::chdman::Chdman {
    fn tool_version(&self) -> String {
        self.version().to_owned()
    }

    fn verify<'a>(
        &'a self,
        chd: &'a std::path::Path,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            tangible_image::chdman::Chdman::verify(self, chd)
                .await
                .map_err(|error| error.to_string())
        })
    }
}

/// Whether a CHD imported as it is may go to RomM.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChdCheck {
    /// `chdman verify` passed.
    Passed,
    /// It failed, or the CHD could not be read; why.
    Failed(String),
    /// Never verified, and this server has no chdman to.
    Unverifiable,
}

/// The background task that does the exporting.
#[derive(Debug, Clone)]
pub struct RommExporter {
    export: RommExport,
    database: Database,
    manifests: ManifestStore,
    integration_id: uuid::Uuid,
    /// What verifies CHDs imported as they are, when this server has chdman.
    verifier: Option<Arc<dyn ChdVerifier>>,
}

/// Why one edition could not be exported, in words for the operator.
#[derive(Debug)]
enum Outcome {
    Current {
        folder: String,
        files: i32,
        written: bool,
    },
    Removed,
    Blocked {
        folder: Option<String>,
        detail: String,
    },
    Failed {
        folder: Option<String>,
        detail: String,
    },
}

impl RommExporter {
    /// An exporter for `export`, reading the catalog from `database` and the
    /// artifacts from `manifests`, filing receipts under `integration_id`.
    #[must_use]
    pub fn new(
        export: RommExport,
        database: Database,
        manifests: ManifestStore,
        integration_id: uuid::Uuid,
    ) -> Self {
        Self {
            export,
            database,
            manifests,
            integration_id,
            verifier: None,
        }
    }

    /// Verify CHDs imported as they are with `verifier`, sending those that
    /// pass to RomM as they are.
    #[must_use]
    pub fn with_chd_verifier(mut self, verifier: Arc<dyn ChdVerifier>) -> Self {
        self.verifier = Some(verifier);
        self
    }

    /// Run until `shutdown` resolves.
    pub async fn run<S>(self, shutdown: S)
    where
        S: Future<Output = ()> + Send,
    {
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            self.reconcile_all().await;
            tokio::select! {
                () = &mut shutdown => return,
                () = self.export.nudge.notified() => {}
                () = tokio::time::sleep(INTERVAL) => {}
            }
        }
    }

    /// One pass over every edition that needs looking at.
    pub async fn reconcile_all(&self) {
        let editions = match editions_to_reconcile(self.database.pool()).await {
            Ok(editions) => editions,
            Err(error) => {
                tracing::error!(error = ?error, "could not list editions for the RomM export");
                return;
            }
        };
        for edition in editions {
            if let Err(error) = self.reconcile(edition).await {
                tracing::error!(%edition, error = ?error, "could not reconcile a RomM export");
            }
        }
    }

    /// Make one edition's folder match what the catalog says.
    ///
    /// # Errors
    ///
    /// A database failure. Everything else is an outcome, recorded against
    /// the edition.
    pub async fn reconcile(&self, edition: EditionId) -> Result<(), tangible_db::DbError> {
        let pool = self.database.pool();
        let previous = edition_romm(pool, edition)
            .await?
            .and_then(|romm| romm.status)
            .filter(|status| status.state != "removed")
            .and_then(|status| status.folder);
        let Some(inputs) = export_inputs(pool, edition).await? else {
            return Ok(());
        };

        let outcome = if inputs.export {
            self.export(edition, &inputs, previous.as_deref()).await?
        } else {
            self.take_out(edition, previous.as_deref()).await?
        };
        self.owe_scans(previous.as_deref(), &outcome).await?;

        match &outcome {
            Outcome::Current {
                folder,
                files,
                written,
            } => {
                record_status(
                    pool,
                    edition,
                    "current",
                    Some(folder),
                    None,
                    *files,
                    *written,
                )
                .await?;
                if *written {
                    tracing::info!(%edition, %folder, files, "RomM export written");
                }
            }
            Outcome::Removed => {
                record_status(pool, edition, "removed", None, None, 0, false).await?;
                tracing::info!(%edition, "RomM export removed");
            }
            Outcome::Blocked { folder, detail } => {
                record_status(
                    pool,
                    edition,
                    "blocked",
                    folder.as_deref(),
                    Some(detail),
                    0,
                    false,
                )
                .await?;
                tracing::warn!(%edition, %detail, "RomM export blocked");
            }
            Outcome::Failed { folder, detail } => {
                record_status(
                    pool,
                    edition,
                    "failed",
                    folder.as_deref(),
                    Some(detail),
                    0,
                    false,
                )
                .await?;
                tracing::error!(%edition, %detail, "RomM export failed");
            }
        }
        Ok(())
    }

    /// Record which platform folders an outcome changed, so RomM is asked
    /// to scan them: the folder written, and the one it replaced or that
    /// was taken out.
    async fn owe_scans(
        &self,
        previous: Option<&str>,
        outcome: &Outcome,
    ) -> Result<(), tangible_db::DbError> {
        let changed = match outcome {
            Outcome::Current {
                folder,
                written: true,
                ..
            } => vec![Some(folder.as_str()), previous],
            Outcome::Removed => vec![previous],
            _ => Vec::new(),
        };
        let mut platforms: Vec<&str> = changed
            .into_iter()
            .flatten()
            .filter_map(|folder| folder.split('/').next())
            .filter(|platform| !platform.is_empty())
            .collect();
        platforms.sort_unstable();
        platforms.dedup();
        if platforms.is_empty() {
            return Ok(());
        }
        owe_scans(self.database.pool(), &platforms).await?;
        self.export.nudge_sync();
        Ok(())
    }

    async fn take_out(
        &self,
        edition: EditionId,
        previous: Option<&str>,
    ) -> Result<Outcome, tangible_db::DbError> {
        let Some(folder) = previous.map(ToOwned::to_owned) else {
            return Ok(Outcome::Removed);
        };
        let root = self.export.root.clone();
        let id = edition.to_string();
        let target = folder.clone();
        let removed = tokio::task::spawn_blocking(move || romm::remove(&root, &target, &id)).await;
        Ok(match removed {
            Ok(Ok(())) => {
                record_folder_removed(self.database.pool(), self.integration_id, &folder).await?;
                Outcome::Removed
            }
            Ok(Err(error)) => apply_failure(Some(folder), &error),
            Err(_) => Outcome::Failed {
                folder: Some(folder),
                detail: "the removal stopped unexpectedly".to_owned(),
            },
        })
    }

    async fn export(
        &self,
        edition: EditionId,
        inputs: &tangible_db::romm::ExportInputs,
        previous: Option<&str>,
    ) -> Result<Outcome, tangible_db::DbError> {
        let blocked = |detail: String| Outcome::Blocked {
            folder: previous.map(ToOwned::to_owned),
            detail,
        };
        let Some(platform) = inputs.platform.clone() else {
            return Ok(blocked("the edition has no platform".to_owned()));
        };
        if inputs.discs.is_empty() {
            return Ok(blocked(
                "the edition has no discs in its catalog entry".to_owned(),
            ));
        }

        let (discs, sources) = match self.gather_discs(&inputs.discs).await {
            Ok(gathered) => gathered,
            Err(detail) => return Ok(blocked(detail)),
        };

        let request = ExportRequest {
            edition_id: edition.to_string(),
            title: inputs.title.clone(),
            region: inputs.region.clone(),
            platform,
            discs,
        };
        let plan = match romm::plan(&request, &rewrite_sheet) {
            Ok(plan) => plan,
            Err(error) => return Ok(blocked(error.to_string())),
        };
        let folder = plan.relative_folder();

        // Which artifact each file came from, for its receipt: every file
        // name a disc contributes carries that disc's base name.
        let receipts: Vec<Receipt> = plan
            .files
            .iter()
            .map(|file| {
                let disc = disc_of(&file.name, &plan.folder, sources.len());
                let (artifact_id, representation) = sources[disc];
                Receipt {
                    destination: format!("{folder}/{}", file.name),
                    artifact_id,
                    representation: representation.to_owned(),
                    method: "copy".to_owned(),
                    sha256: file.sha256.clone(),
                    length_bytes: i64::try_from(file.length).unwrap_or(i64::MAX),
                }
            })
            .collect();

        let root = self.export.root.clone();
        let previous = previous.map(ToOwned::to_owned);
        let file_count = i32::try_from(plan.files.len()).unwrap_or(i32::MAX);
        let applied =
            tokio::task::spawn_blocking(move || romm::apply(&root, &plan, previous.as_deref()))
                .await;

        Ok(match applied {
            Ok(Ok(Applied::Unchanged)) => {
                // Receipts written before they recorded sizes get them, so
                // RomM's files can be compared by size too.
                fill_receipt_lengths(self.database.pool(), self.integration_id, &receipts).await?;
                Outcome::Current {
                    folder,
                    files: file_count,
                    written: false,
                }
            }
            Ok(Ok(Applied::Written(methods))) => {
                let receipts: Vec<Receipt> = receipts
                    .into_iter()
                    .map(|mut receipt| {
                        if let Some((_, method)) = methods
                            .iter()
                            .find(|(name, _)| receipt.destination.ends_with(&format!("/{name}")))
                        {
                            method.as_str().clone_into(&mut receipt.method);
                        }
                        receipt
                    })
                    .collect();
                record_receipts(
                    self.database.pool(),
                    self.integration_id,
                    &folder,
                    &receipts,
                )
                .await?;
                Outcome::Current {
                    folder,
                    files: file_count,
                    written: true,
                }
            }
            Ok(Err(error)) => apply_failure(Some(folder), &error),
            Err(_) => Outcome::Failed {
                folder: Some(folder),
                detail: "the export stopped unexpectedly".to_owned(),
            },
        })
    }

    /// Each disc's image, and the artifact and representation it came from.
    async fn gather_discs(
        &self,
        discs: &[DiscInputs],
    ) -> Result<(Vec<DiscImage>, Vec<(ArtifactId, &'static str)>), String> {
        let mut images = Vec::with_capacity(discs.len());
        let mut sources = Vec::with_capacity(discs.len());
        for (index, disc) in discs.iter().enumerate() {
            let number = index + 1;
            let checks = self.check_imported_chds(disc).await;
            let (artifact, format) = chosen_artifact(disc, &checks)
                .ok_or_else(|| missing_image(number, disc, &checks))?;
            let image = self
                .disc_image(artifact, format)
                .await
                .map_err(|detail| format!("disc {number}: {detail}"))?;
            images.push(image);
            sources.push((
                artifact,
                match format {
                    ArtifactFormat::Iso => "iso",
                    ArtifactFormat::Chd => "chd",
                    _ => "cue_bin",
                },
            ));
        }
        Ok((images, sources))
    }

    /// Whether each CHD imported as it is that is linked to `disc` may go
    /// to RomM.
    async fn check_imported_chds(&self, disc: &DiscInputs) -> BTreeMap<ArtifactId, ChdCheck> {
        let mut checks = BTreeMap::new();
        for link in disc.artifacts.iter().filter(|link| is_imported_chd(link)) {
            let check = self.check_imported_chd(link.id).await;
            checks.insert(link.id, check);
        }
        checks
    }

    /// Whether one CHD imported as it is has passed `chdman verify`,
    /// running it once for each chdman version that has not.
    async fn check_imported_chd(&self, artifact: ArtifactId) -> ChdCheck {
        let Ok(manifest) = self.manifests.read(artifact).await else {
            return ChdCheck::Failed("its manifest could not be read".to_owned());
        };
        let Some(image) = primary_image(&manifest) else {
            return ChdCheck::Failed("it has no image file".to_owned());
        };
        let sha256 = image.content.sha256.to_hex();
        let pool = self.database.pool();
        let verdicts = match chd_verdicts(pool, &sha256).await {
            Ok(verdicts) => verdicts,
            Err(error) => {
                tracing::warn!(%artifact, error = ?error, "could not read a CHD's verification");
                return ChdCheck::Failed("its verification could not be read".to_owned());
            }
        };
        if verdicts.passed {
            return ChdCheck::Passed;
        }
        let Some(verifier) = &self.verifier else {
            return verdicts
                .last_failure
                .map_or(ChdCheck::Unverifiable, ChdCheck::Failed);
        };
        let version = verifier.tool_version();
        if verdicts.tried_with.contains(&version) {
            return ChdCheck::Failed(
                verdicts
                    .last_failure
                    .unwrap_or_else(|| "it did not verify".to_owned()),
            );
        }

        let path = self.manifests.objects().object_path(&image.content.sha256);
        tracing::info!(%artifact, chdman = %version, "verifying a CHD imported as it is, for RomM");
        // What chdman says names the file it read; that path is the
        // library's, not something to show.
        let outcome = verifier
            .verify(&path)
            .await
            .map_err(|detail| detail.replace(&path.to_string_lossy().into_owned(), "the CHD"));
        let (passed, detail) = match &outcome {
            Ok(()) => (true, None),
            Err(detail) => (false, Some(detail.as_str())),
        };
        if let Err(error) = record_chd_verdict(pool, &sha256, &version, passed, detail).await {
            tracing::warn!(%artifact, error = ?error, "could not record a CHD's verification");
        }
        match outcome {
            Ok(()) => {
                tracing::info!(%artifact, "a CHD imported as it is verified, and goes to RomM as it is");
                ChdCheck::Passed
            }
            Err(detail) => {
                tracing::warn!(%artifact, %detail, "a CHD imported as it is did not verify");
                ChdCheck::Failed(detail)
            }
        }
    }

    /// The image one disc exports as, read from its manifest.
    async fn disc_image(
        &self,
        artifact: ArtifactId,
        format: ArtifactFormat,
    ) -> Result<DiscImage, String> {
        let manifest = self
            .manifests
            .read(artifact)
            .await
            .map_err(|_| "its manifest could not be read".to_owned())?;
        let objects = self.manifests.objects();
        let source = |component: &tangible_domain::manifest::Component| SourceFile {
            logical_path: component.logical_path.to_string(),
            object_path: objects.object_path(&component.content.sha256),
            sha256: component.content.sha256.to_hex(),
            length: component.length_bytes,
        };

        if matches!(format, ArtifactFormat::Iso | ArtifactFormat::Chd) {
            let image = primary_image(&manifest).ok_or("the image has no image file")?;
            return Ok(DiscImage::SingleFile(source(image)));
        }

        let sheet = manifest
            .components
            .iter()
            .find(|component| {
                component.role == ComponentRole::Descriptor
                    && component.logical_path.extension().as_deref() == Some("cue")
            })
            .ok_or("the CUE/BIN set has no CUE sheet")?;
        if sheet.length_bytes > MAX_SHEET_BYTES {
            return Err("its CUE sheet is implausibly large".to_owned());
        }
        let sheet_bytes = tokio::fs::read(objects.object_path(&sheet.content.sha256))
            .await
            .map_err(|_| "its CUE sheet could not be read".to_owned())?;
        let parsed = tangible_image::cue::parse(&sheet_bytes)
            .map_err(|error| format!("its CUE sheet could not be read: {error}"))?;
        let staged: Vec<tangible_domain::LogicalPath> = manifest
            .components
            .iter()
            .map(|component| component.logical_path.clone())
            .collect();
        let resolution = tangible_image::cue::resolve_references(&parsed, &staged);
        let mut files = Vec::with_capacity(resolution.files.len());
        for reference in &resolution.files {
            let Some(path) = &reference.resolved else {
                return Err(format!(
                    "its sheet names {:?}, which is not part of the artifact",
                    reference.declared
                ));
            };
            let component = manifest
                .components
                .iter()
                .find(|component| &component.logical_path == path)
                .ok_or("a resolved file is missing from the manifest")?;
            files.push((reference.declared.clone(), source(component)));
        }
        Ok(DiscImage::CueBin {
            sheet: source(sheet),
            sheet_bytes,
            files,
        })
    }
}

/// Rewrite a CUE sheet's file names with the CUE parser's own grammar.
fn rewrite_sheet(sheet: &[u8], renames: &BTreeMap<String, String>) -> Result<Vec<u8>, String> {
    tangible_image::rewrite_file_names(sheet, |name| renames.get(name).cloned())
        .map_err(|error| error.to_string())
}

/// Whether a CHD was shown, by extracting it again, to hold its parent's
/// tracks. Only such a CHD is exported: one made without that check, or
/// whose check found less, is not offered to an emulator as the disc.
fn is_checked_chd(link: &LinkedArtifact) -> bool {
    link.format == ArtifactFormat::Chd
        && matches!(
            link.loss_character,
            Some(LossCharacter::BitExactRepack | LossCharacter::StructurallyEquivalent)
        )
}

/// Whether an artifact is a CHD that was imported, not made here.
fn is_imported_chd(link: &LinkedArtifact) -> bool {
    link.format == ArtifactFormat::Chd && link.loss_character.is_none()
}

/// The artifact a disc exports from, in order of preference: a checked CHD,
/// which RomM's emulators read and which is a fraction of the size; then a
/// CHD imported as it is that `chdman verify` passed (`checks`); then an ISO
/// or CUE/BIN linked as a representation of the disc; then any ISO or
/// CUE/BIN linked at all.
fn chosen_artifact(
    disc: &DiscInputs,
    checks: &BTreeMap<ArtifactId, ChdCheck>,
) -> Option<(ArtifactId, ArtifactFormat)> {
    let exportable = |link: &&LinkedArtifact| {
        matches!(link.format, ArtifactFormat::Iso | ArtifactFormat::CueBin)
    };
    disc.artifacts
        .iter()
        .find(|link| is_checked_chd(link))
        .or_else(|| {
            disc.artifacts.iter().find(|link| {
                is_imported_chd(link) && checks.get(&link.id) == Some(&ChdCheck::Passed)
            })
        })
        .or_else(|| {
            disc.artifacts
                .iter()
                .filter(exportable)
                .find(|link| link.relationship == "representation_of")
        })
        .or_else(|| disc.artifacts.iter().find(exportable))
        .map(|link| (link.id, link.format))
}

/// Why a disc has nothing to export, in words that say what to do.
fn missing_image(
    number: usize,
    disc: &DiscInputs,
    checks: &BTreeMap<ArtifactId, ChdCheck>,
) -> String {
    let imported_chd = disc
        .artifacts
        .iter()
        .filter(|link| is_imported_chd(link))
        .find_map(|link| checks.get(&link.id));
    if disc
        .artifacts
        .iter()
        .any(|link| link.format == ArtifactFormat::TocBin)
    {
        format!(
            "disc {number} is linked only to a TOC/BIN image, which RomM and its emulators do not \
             read; make a CHD of it, or link a CUE/BIN or ISO image of it"
        )
    } else if disc
        .artifacts
        .iter()
        .any(|link| link.format == ArtifactFormat::CcdImgSub)
    {
        // Emulators differ in reading CloneCD images; every one RomM serves
        // reads CUE/BIN and CHD (docs/plans/ccd-images.md).
        format!(
            "disc {number} is linked only to a CloneCD (CCD/IMG/SUB) image, which not every \
             emulator reads; make a CUE/BIN of it (which drops the subchannel) or a CHD, or \
             link a CUE/BIN or ISO image of it"
        )
    } else if disc
        .artifacts
        .iter()
        .any(|link| link.format == ArtifactFormat::MdsMdf)
    {
        // Neither RomM's emulators nor chdman read MDS; Tangible makes a
        // CUE/BIN or ISO of one itself (docs/plans/mds-images.md).
        format!(
            "disc {number} is linked only to an Alcohol 120% (MDS/MDF) image, which RomM's \
             emulators do not read; make a CUE/BIN (of a CD) or an ISO (of a DVD) of it, or \
             link a CUE/BIN or ISO image of it"
        )
    } else if disc.artifacts.is_empty() {
        format!("disc {number} has no image linked")
    } else if let Some(check) = imported_chd {
        // Not made here, so there is no original to check it against: it
        // goes as it is once chdman has verified it against its own SHA-1s
        // (owner decision 2026-11-02, OWNER_QUEUE 14).
        match check {
            ChdCheck::Failed(detail) => format!(
                "disc {number} is linked only to a CHD imported as it is, which did not pass \
                 chdman's verify ({detail}), so it is not sent to RomM; link a CUE/BIN or ISO \
                 image of the disc"
            ),
            _ => format!(
                "disc {number} is linked only to a CHD imported as it is, which goes to RomM once \
                 chdman has verified it, and this server runs no chdman \
                 (TANGIBLE_DERIVATION_ENGINE=chdman); or link a CUE/BIN or ISO image of the disc"
            ),
        }
    } else if disc
        .artifacts
        .iter()
        .any(|link| link.format == ArtifactFormat::Chd)
    {
        format!(
            "disc {number} has a CHD that was not checked against its original; make a CHD of \
             the original here, or link a CUE/BIN or ISO image"
        )
    } else {
        format!("disc {number} has no ISO, CUE/BIN or CHD image linked")
    }
}

/// The image file of a single-image artifact.
fn primary_image(manifest: &ArtifactManifest) -> Option<&tangible_domain::manifest::Component> {
    manifest
        .components
        .iter()
        .find(|component| component.role == ComponentRole::PrimaryImage)
        .or(match manifest.components.as_slice() {
            [only] => Some(only),
            _ => None,
        })
}

/// Which disc a planned file belongs to, from its name.
fn disc_of(name: &str, game: &str, discs: usize) -> usize {
    if discs <= 1 {
        return 0;
    }
    (1..=discs)
        .rev()
        .find(|number| name.starts_with(&format!("{game} (Disc {number})")))
        .map_or(0, |number| number - 1)
}

fn apply_failure(folder: Option<String>, error: &ApplyError) -> Outcome {
    match error {
        ApplyError::Unmanaged { .. } | ApplyError::ForeignFiles { .. } => Outcome::Blocked {
            folder,
            detail: error.to_string(),
        },
        ApplyError::RootMissing | ApplyError::Io { .. } => Outcome::Failed {
            folder,
            detail: error.to_string(),
        },
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use tangible_domain::DiscId;

    fn none() -> BTreeMap<ArtifactId, ChdCheck> {
        BTreeMap::new()
    }

    fn disc(links: &[(ArtifactFormat, &str)]) -> DiscInputs {
        derived(
            &links
                .iter()
                .map(|(format, relationship)| (*format, *relationship, None))
                .collect::<Vec<_>>(),
        )
    }

    fn derived(links: &[(ArtifactFormat, &str, Option<LossCharacter>)]) -> DiscInputs {
        DiscInputs {
            disc_id: DiscId::generate(),
            artifacts: links
                .iter()
                .map(|(format, relationship, loss)| LinkedArtifact {
                    id: ArtifactId::generate(),
                    format: *format,
                    relationship: (*relationship).to_owned(),
                    loss_character: *loss,
                })
                .collect(),
        }
    }

    #[test]
    fn a_checked_chd_is_preferred_and_an_unchecked_one_never_chosen() {
        let checked = derived(&[
            (ArtifactFormat::CueBin, "representation_of", None),
            (
                ArtifactFormat::Chd,
                "representation_of",
                Some(LossCharacter::StructurallyEquivalent),
            ),
        ]);
        assert_eq!(
            chosen_artifact(&checked, &none()).map(|(_, f)| f),
            Some(ArtifactFormat::Chd)
        );
        for loss in [
            None,
            Some(LossCharacter::Unknown),
            Some(LossCharacter::Lossy),
        ] {
            let unchecked = derived(&[
                (ArtifactFormat::CueBin, "representation_of", None),
                (ArtifactFormat::Chd, "representation_of", loss),
            ]);
            assert_eq!(
                chosen_artifact(&unchecked, &none()).map(|(_, f)| f),
                Some(ArtifactFormat::CueBin),
                "a CHD whose loss is {loss:?} must not be exported"
            );
        }
        let toc_with_chd = derived(&[
            (ArtifactFormat::TocBin, "representation_of", None),
            (
                ArtifactFormat::Chd,
                "representation_of",
                Some(LossCharacter::StructurallyEquivalent),
            ),
        ]);
        assert_eq!(
            chosen_artifact(&toc_with_chd, &none()).map(|(_, f)| f),
            Some(ArtifactFormat::Chd),
            "a TOC/BIN disc reaches RomM through its checked CHD"
        );
    }

    #[test]
    fn a_representation_is_preferred_and_only_images_rommm_reads_are_chosen() {
        let d = disc(&[
            (ArtifactFormat::TocBin, "representation_of"),
            (ArtifactFormat::Iso, "contains"),
            (ArtifactFormat::CueBin, "representation_of"),
        ]);
        assert_eq!(
            chosen_artifact(&d, &none()).map(|(_, f)| f),
            Some(ArtifactFormat::CueBin)
        );
        let d = disc(&[(ArtifactFormat::Iso, "unknown")]);
        assert_eq!(
            chosen_artifact(&d, &none()).map(|(_, f)| f),
            Some(ArtifactFormat::Iso)
        );
        assert_eq!(
            chosen_artifact(
                &disc(&[(ArtifactFormat::TocBin, "representation_of")]),
                &none()
            ),
            None
        );
    }

    #[test]
    fn a_disc_with_nothing_to_export_says_what_to_do() {
        assert!(
            missing_image(
                2,
                &disc(&[(ArtifactFormat::TocBin, "representation_of")]),
                &none()
            )
            .contains("TOC/BIN")
        );
        assert_eq!(
            missing_image(1, &disc(&[]), &none()),
            "disc 1 has no image linked"
        );
        let clonecd = missing_image(
            5,
            &disc(&[(ArtifactFormat::CcdImgSub, "representation_of")]),
            &none(),
        );
        assert!(clonecd.contains("CloneCD"), "{clonecd}");
        assert!(clonecd.contains("make a CUE/BIN"), "{clonecd}");
        assert_eq!(
            chosen_artifact(
                &disc(&[(ArtifactFormat::CcdImgSub, "representation_of")]),
                &none()
            ),
            None,
            "a CloneCD image is not sent to RomM as it is"
        );
        let mds = missing_image(
            6,
            &disc(&[(ArtifactFormat::MdsMdf, "representation_of")]),
            &none(),
        );
        assert!(
            mds.contains("MDS/MDF") && mds.contains("make a CUE/BIN"),
            "{mds}"
        );
        assert_eq!(
            chosen_artifact(
                &disc(&[(ArtifactFormat::MdsMdf, "representation_of")]),
                &none()
            ),
            None,
            "an MDS image is not sent to RomM as it is"
        );
        assert!(
            missing_image(
                3,
                &derived(&[(ArtifactFormat::Chd, "unknown", Some(LossCharacter::Unknown))]),
                &none()
            )
            .contains("not checked")
        );
        let imported = disc(&[(ArtifactFormat::Chd, "representation_of")]);
        let id = imported.artifacts[0].id;
        let unverifiable = missing_image(
            7,
            &imported,
            &BTreeMap::from([(id, ChdCheck::Unverifiable)]),
        );
        assert!(
            unverifiable.contains("imported as it is") && unverifiable.contains("runs no chdman"),
            "{unverifiable}"
        );
        let failed = missing_image(
            7,
            &imported,
            &BTreeMap::from([(id, ChdCheck::Failed("hunk 12 is corrupt".to_owned()))]),
        );
        assert!(
            failed.contains("did not pass chdman's verify (hunk 12 is corrupt)"),
            "{failed}"
        );
        assert!(
            missing_image(
                4,
                &disc(&[(ArtifactFormat::TocBin, "representation_of")]),
                &none()
            )
            .contains("make a CHD")
        );
    }

    #[test]
    fn an_imported_chd_goes_as_it_is_only_once_verified() {
        let d = derived(&[
            (ArtifactFormat::CueBin, "representation_of", None),
            (ArtifactFormat::Chd, "representation_of", None),
        ]);
        let chd = d.artifacts[1].id;
        for (check, expected) in [
            (None, ArtifactFormat::CueBin),
            (Some(ChdCheck::Unverifiable), ArtifactFormat::CueBin),
            (
                Some(ChdCheck::Failed("bad".to_owned())),
                ArtifactFormat::CueBin,
            ),
            (Some(ChdCheck::Passed), ArtifactFormat::Chd),
        ] {
            let checks: BTreeMap<_, _> = check
                .clone()
                .map(|check| (chd, check))
                .into_iter()
                .collect();
            assert_eq!(
                chosen_artifact(&d, &checks).map(|(_, f)| f),
                Some(expected),
                "{check:?}"
            );
        }

        // Alone, it is the disc's image once verified, and nothing before.
        let alone = disc(&[(ArtifactFormat::Chd, "representation_of")]);
        let id = alone.artifacts[0].id;
        assert_eq!(chosen_artifact(&alone, &none()), None);
        assert_eq!(
            chosen_artifact(&alone, &BTreeMap::from([(id, ChdCheck::Passed)])),
            Some((id, ArtifactFormat::Chd))
        );

        // A CHD made and checked here still comes first.
        let both = derived(&[
            (ArtifactFormat::Chd, "representation_of", None),
            (
                ArtifactFormat::Chd,
                "representation_of",
                Some(LossCharacter::StructurallyEquivalent),
            ),
        ]);
        let imported = both.artifacts[0].id;
        let made = both.artifacts[1].id;
        assert_eq!(
            chosen_artifact(&both, &BTreeMap::from([(imported, ChdCheck::Passed)])).map(|(a, _)| a),
            Some(made)
        );
    }

    #[test]
    fn a_file_is_attributed_to_its_disc() {
        assert_eq!(disc_of("Game (Disc 2).cue", "Game", 2), 1);
        assert_eq!(disc_of("Game (Disc 1) (Track 03).bin", "Game", 2), 0);
        assert_eq!(disc_of("Game (Disc 12).cue", "Game", 12), 11);
        assert_eq!(disc_of("Game (Disc 1).cue", "Game", 12), 0);
        assert_eq!(disc_of("Game.iso", "Game", 1), 0);
    }
}
