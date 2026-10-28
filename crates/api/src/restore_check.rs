// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! After a restore: do the catalogue and the library agree (STORAGE.md 12)?
//!
//! A backup is the database dumped, then the library copied (deploy/README,
//! "Backups"). Restored, the library may hold artifacts imported after the
//! dump, which the catalogue does not name; and a library copied badly may
//! lack objects the catalogue does name. This finds both in a few seconds,
//! by looking rather than reading: every object the catalogue names is
//! there at the size recorded, every artifact it names has its manifest,
//! and every manifest in the library names an artifact it has. Whether the
//! bytes are right is a full integrity check's question (`tangible scrub`),
//! which reads every one.
//!
//! An artifact the library has and the catalogue lacks can be catalogued
//! again from its manifest, as its import did, once every object the
//! manifest names is there at its size. Nothing else is ever changed: the
//! check reports, and what it finds is for the operator.

use std::collections::{BTreeSet, HashMap};

use tangible_db::gc::catalogue_marks;
use tangible_db::restore::{catalogued_artifacts, record_recatalogued, unfinished_jobs};
use tangible_db::{Database, DbError};
use tangible_domain::{ArtifactId, Sha256Digest};
use tangible_storage::{FilesystemStore, ManifestStore, ManifestStoreError, StorageError};

use crate::catalog::register_manifest;

/// How many of each kind of finding the command line lists; the rest are
/// counted.
pub const LISTED: usize = 20;

/// How a check runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestoreCheckSettings {
    /// Catalogue again what the library has and the catalogue lacks.
    pub register: bool,
    /// How many of each kind of finding to list.
    pub list: usize,
}

/// Why the check could not run.
#[derive(Debug, thiserror::Error)]
pub enum RestoreCheckError {
    /// The catalogue could not be read.
    #[error("the catalogue could not be read: {0}")]
    Database(#[from] DbError),
    /// The library could not be listed or looked at.
    #[error("the library could not be read: {0}")]
    Storage(String),
}

impl From<StorageError> for RestoreCheckError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error.to_string())
    }
}

impl From<ManifestStoreError> for RestoreCheckError {
    fn from(error: ManifestStoreError) -> Self {
        Self::Storage(error.to_string())
    }
}

/// The first findings, and how many there were.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Findings<T> {
    /// How many.
    pub count: usize,
    /// The first of them.
    pub listed: Vec<T>,
    #[serde(skip)]
    limit: usize,
}

impl<T> Default for Findings<T> {
    fn default() -> Self {
        Self {
            count: 0,
            listed: Vec::new(),
            limit: LISTED,
        }
    }
}

impl<T> Findings<T> {
    fn push(&mut self, finding: T) {
        self.count += 1;
        if self.listed.len() < self.limit {
            self.listed.push(finding);
        }
    }

    fn limited(limit: usize) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }

    /// Whether there are none.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// An object whose stored size is not the size recorded.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WrongSize {
    /// The object.
    pub sha256: String,
    /// The size the catalogue recorded.
    pub recorded: i64,
    /// The size stored.
    pub stored: u64,
}

/// A manifest the catalogue lacks, and what became of it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Uncatalogued {
    /// The artifact.
    pub artifact_id: String,
    /// `registered`, `found` (not asked to register), or why it was not
    /// registered.
    pub outcome: String,
}

/// What the check found.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct RestoreReport {
    /// Migrations this build has that the database has not applied.
    pub pending_migrations: Vec<String>,
    /// Objects the catalogue names that were looked for.
    pub objects_checked: usize,
    /// Of those, not in the library.
    pub missing_objects: Findings<String>,
    /// Of those, in the library at another size.
    pub wrong_size: Findings<WrongSize>,
    /// Artifacts the catalogue names.
    pub artifacts_checked: usize,
    /// Of those, without a manifest in the library.
    pub missing_manifests: Findings<String>,
    /// Manifests in the library the catalogue does not name.
    pub uncatalogued: Findings<Uncatalogued>,
    /// How many of those were catalogued now.
    pub registered: usize,
    /// Unfinished work the catalogue records: kind, state, how many.
    pub unfinished: Vec<(String, String, i64)>,
}

impl RestoreReport {
    /// Whether anything needs the operator: the schema behind, objects or
    /// manifests missing, or artifacts the catalogue still lacks.
    #[must_use]
    pub fn needs_attention(&self) -> bool {
        !self.pending_migrations.is_empty()
            || !self.missing_objects.is_empty()
            || !self.wrong_size.is_empty()
            || !self.missing_manifests.is_empty()
            || self.uncatalogued.count > self.registered
    }
}

/// Check the catalogue against the library, cataloguing again what the
/// library has and the catalogue lacks when the settings say to.
///
/// # Errors
///
/// [`RestoreCheckError`] when the catalogue or the library cannot be read
/// at all; what is missing from either is a finding, not an error.
pub async fn restore_check(
    database: &Database,
    objects: &FilesystemStore,
    manifests: &ManifestStore,
    settings: RestoreCheckSettings,
) -> Result<RestoreReport, RestoreCheckError> {
    let pool = database.pool();
    let mut report = RestoreReport {
        pending_migrations: database
            .pending_migrations()
            .await?
            .into_iter()
            .map(|migration| migration.to_string())
            .collect(),
        missing_objects: Findings::limited(settings.list),
        wrong_size: Findings::limited(settings.list),
        missing_manifests: Findings::limited(settings.list),
        uncatalogued: Findings::limited(settings.list),
        ..RestoreReport::default()
    };

    let marks = catalogue_marks(pool).await?;
    let recorded: HashMap<Sha256Digest, i64> = marks.recorded.iter().copied().collect();
    let mut live: Vec<Sha256Digest> = marks.live.iter().copied().collect();
    live.sort_by_key(Sha256Digest::to_hex);
    for digest in live {
        report.objects_checked += 1;
        match objects.stat(&digest).await? {
            None => report.missing_objects.push(digest.to_hex()),
            Some(stat) => {
                if let Some(&size) = recorded.get(&digest)
                    && u64::try_from(size).ok() != Some(stat.size_bytes)
                {
                    report.wrong_size.push(WrongSize {
                        sha256: digest.to_hex(),
                        recorded: size,
                        stored: stat.size_bytes,
                    });
                }
            }
        }
    }

    let catalogued: BTreeSet<uuid::Uuid> = catalogued_artifacts(pool).await?.into_iter().collect();
    let stored: BTreeSet<uuid::Uuid> = manifests
        .list()
        .await?
        .into_iter()
        .map(|id| *id.as_uuid())
        .collect();
    report.artifacts_checked = catalogued.len();
    for id in catalogued.difference(&stored) {
        report.missing_manifests.push(id.to_string());
    }
    for id in stored.difference(&catalogued) {
        let artifact = ArtifactId::from_uuid(*id);
        let outcome = if settings.register {
            match catalogue_again(database, objects, manifests, artifact).await {
                Ok(true) => {
                    report.registered += 1;
                    "registered".to_owned()
                }
                // Catalogued by something else since it was listed: as
                // fixed as if this had done it.
                Ok(false) => {
                    report.registered += 1;
                    "already catalogued".to_owned()
                }
                Err(why) => why,
            }
        } else {
            "found".to_owned()
        };
        report.uncatalogued.push(Uncatalogued {
            artifact_id: id.to_string(),
            outcome,
        });
    }

    report.unfinished = unfinished_jobs(pool)
        .await?
        .into_iter()
        .map(|(kind, state, count)| (kind.to_owned(), state, count))
        .collect();
    Ok(report)
}

/// Catalogue one artifact again from its manifest, once the manifest is the
/// artifact's own and every object it names is in the library at its
/// length; or say why not. Answers whether this catalogued it.
async fn catalogue_again(
    database: &Database,
    objects: &FilesystemStore,
    manifests: &ManifestStore,
    artifact: ArtifactId,
) -> Result<bool, String> {
    let manifest = manifests
        .read(artifact)
        .await
        .map_err(|error| format!("its manifest could not be read: {error}"))?;
    // A manifest filed under one artifact that describes another (a copy
    // gone wrong) would otherwise catalogue the other one and report this
    // one fixed.
    if manifest.artifact_id != artifact {
        return Err(format!(
            "its manifest file describes artifact {} instead; not catalogued",
            manifest.artifact_id
        ));
    }
    for component in &manifest.components {
        match objects.stat(&component.content.sha256).await {
            Ok(Some(stat)) if stat.size_bytes == component.length_bytes => {}
            Ok(Some(stat)) => {
                return Err(format!(
                    "{} is stored at {} bytes, not the {} its manifest says",
                    component.logical_path, stat.size_bytes, component.length_bytes
                ));
            }
            Ok(None) => {
                return Err(format!("{} is not in the library", component.logical_path));
            }
            Err(error) => {
                return Err(format!(
                    "{} could not be looked at: {error}",
                    component.logical_path
                ));
            }
        }
    }
    let registered = register_manifest(database, &manifest)
        .await
        .map_err(|error| format!("it could not be catalogued: {error}"))?;
    if registered.created
        && let Err(error) = record_recatalogued(database.pool(), *artifact.as_uuid()).await
    {
        tracing::warn!(artifact_id = %artifact, error = ?error, "the re-cataloguing could not be audited");
    }
    tracing::info!(artifact_id = %artifact, "artifact catalogued again from its manifest after a restore");
    Ok(registered.created)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn findings_list_the_first_twenty_and_count_the_rest() {
        let mut findings = Findings::default();
        for n in 0..25 {
            findings.push(n);
        }
        assert_eq!(findings.count, 25);
        assert_eq!(findings.listed, (0..20).collect::<Vec<_>>());
        assert!(!findings.is_empty());
    }

    #[test]
    fn needs_attention_unless_everything_agrees_or_was_registered() {
        let mut report = RestoreReport::default();
        assert!(!report.needs_attention());
        report.uncatalogued.push(Uncatalogued {
            artifact_id: "a".into(),
            outcome: "registered".into(),
        });
        report.registered = 1;
        assert!(!report.needs_attention(), "registered is fixed");
        report.uncatalogued.push(Uncatalogued {
            artifact_id: "b".into(),
            outcome: "found".into(),
        });
        assert!(report.needs_attention());
        let mut report = RestoreReport::default();
        report.unfinished.push(("burn".into(), "writing".into(), 1));
        assert!(
            !report.needs_attention(),
            "unfinished work is said, not failed"
        );
        report.pending_migrations.push("0025_dats".into());
        assert!(report.needs_attention());
    }
}
