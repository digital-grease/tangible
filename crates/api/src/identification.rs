// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Preservation hashes and identification (docs/plans/dat-matching.md).
//!
//! Every artifact gets the CRC32, MD5 and SHA-1 preservation databases list:
//! of each component, and of each CD track's bytes, which is what Redump
//! lists a track by. A background runner takes one artifact at a time, with
//! a lease, reads its stored objects once each (every track of a BIN is a
//! range hashed as the file goes past), and records the hashes. Artifacts
//! already in the library are hashed the same way, once.
//!
//! The stored objects are only read. A missing or unreadable one fails the
//! artifact's hashing, tried again later and then left failed, saying why;
//! an integrity check is what says what is wrong with the object.

use std::sync::Arc;
use std::time::Duration;

use tangible_db::identification::{
    Hashes, claim_for_hashing, claim_next_unhashed, fail_hashing, record_hashes,
};
use tangible_db::{Database, DbError};
use tangible_domain::ArtifactId;
use tangible_domain::manifest::{ArtifactManifest, Topology};
use tangible_storage::preservation::{PreservationHashes, hash_file};
use tangible_storage::{FilesystemStore, ManifestStore};
use tokio::sync::Notify;

/// How the runner behaves.
#[derive(Debug, Clone)]
pub struct IdentificationSettings {
    /// What to call this process in the lease.
    pub owner: String,
    /// How long a claim is held: longer than hashing the largest artifact.
    pub lease: Duration,
    /// How long to wait when nothing is pending and nobody nudges.
    pub poll_interval: Duration,
    /// How long a failed artifact waits before it is tried again.
    pub retry_after: Duration,
    /// How many tries before an artifact is left failed.
    pub max_attempts: i32,
}

impl Default for IdentificationSettings {
    fn default() -> Self {
        Self {
            owner: "tangible-server".to_owned(),
            lease: Duration::from_secs(60 * 60),
            poll_interval: Duration::from_secs(60),
            retry_after: Duration::from_secs(60 * 60),
            max_attempts: 3,
        }
    }
}

/// What imports and derivations hold to wake the runner when an artifact
/// arrives.
#[derive(Debug, Clone, Default)]
pub struct IdentificationHandle {
    nudge: Arc<Notify>,
}

impl IdentificationHandle {
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

/// Hashes artifacts for identification.
#[derive(Debug, Clone)]
pub struct IdentificationRunner {
    database: Database,
    objects: FilesystemStore,
    manifests: ManifestStore,
    handle: IdentificationHandle,
    settings: IdentificationSettings,
}

fn seconds(duration: Duration) -> i64 {
    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}

/// The byte ranges of a component that are CD tracks, with their numbers:
/// a track runs from its first byte in the file for its sectors, its pregap
/// in the file included, which is how Redump cuts a track's BIN.
fn track_ranges(
    manifest: &ArtifactManifest,
    component: tangible_domain::ComponentId,
) -> Vec<(u32, u64, u64)> {
    let Topology::CdTracks { tracks, .. } = &manifest.topology else {
        return Vec::new();
    };
    tracks
        .iter()
        .filter(|track| track.component_id == component)
        .filter_map(|track| {
            let sector = u64::from(tangible_domain::cd::sector_bytes(&track.mode)?);
            let length = track.sector_count.checked_mul(sector)?;
            let end = track.file_offset_bytes.checked_add(length)?;
            Some((track.number, track.file_offset_bytes, end))
        })
        .collect()
}

fn as_row(hashes: &PreservationHashes) -> Hashes<'_> {
    Hashes {
        crc32: &hashes.crc32,
        md5: &hashes.md5,
        sha1: &hashes.sha1,
        length: i64::try_from(hashes.length).unwrap_or(i64::MAX),
    }
}

impl IdentificationRunner {
    /// Assemble a runner.
    #[must_use]
    pub const fn new(
        database: Database,
        objects: FilesystemStore,
        manifests: ManifestStore,
        handle: IdentificationHandle,
        settings: IdentificationSettings,
    ) -> Self {
        Self {
            database,
            objects,
            manifests,
            handle,
            settings,
        }
    }

    /// Run until `shutdown` resolves.
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
                        tracing::error!(error = ?error, "the artifacts to hash could not be read");
                        false
                    }
                }
            },
            || async {
                tokio::select! {
                    () = self.handle.nudge.notified() => {}
                    () = tokio::time::sleep(self.settings.poll_interval) => {}
                }
            },
        )
        .await;
        tracing::info!("identification runner stopped");
    }

    /// Hash one pending artifact. Returns whether there was one.
    ///
    /// # Errors
    ///
    /// [`DbError`] if the catalogue cannot be read or written.
    pub async fn step(&self) -> Result<bool, DbError> {
        let pool = self.database.pool();
        let owner = &self.settings.owner;
        let Some(artifact) = claim_next_unhashed(pool, owner, seconds(self.settings.lease)).await?
        else {
            return Ok(false);
        };
        self.hash_claimed(artifact).await?;
        Ok(true)
    }

    /// Hash `artifact` now, if it is pending and nobody holds it: what a
    /// test, or anything that cannot wait for the queue, asks. Returns
    /// whether it was hashed or failed (rather than left).
    ///
    /// # Errors
    ///
    /// [`DbError`] if the catalogue cannot be read or written.
    pub async fn identify(&self, artifact: uuid::Uuid) -> Result<bool, DbError> {
        let pool = self.database.pool();
        if !claim_for_hashing(
            pool,
            artifact,
            &self.settings.owner,
            seconds(self.settings.lease),
        )
        .await?
        {
            return Ok(false);
        }
        self.hash_claimed(artifact).await?;
        Ok(true)
    }

    /// Hash an artifact this runner holds, and record what came of it.
    async fn hash_claimed(&self, artifact: uuid::Uuid) -> Result<(), DbError> {
        let pool = self.database.pool();
        let owner = &self.settings.owner;
        match self.hash(ArtifactId::from_uuid(artifact)).await {
            Ok((components, tracks)) => {
                let component_rows: Vec<_> = components
                    .iter()
                    .map(|(id, hashes)| (*id, as_row(hashes)))
                    .collect();
                let track_rows: Vec<_> = tracks
                    .iter()
                    .map(|(number, hashes)| (*number, as_row(hashes)))
                    .collect();
                if record_hashes(pool, artifact, owner, &component_rows, &track_rows).await? {
                    tracing::info!(
                        artifact_id = %artifact,
                        components = component_rows.len(),
                        tracks = track_rows.len(),
                        "preservation hashes recorded"
                    );
                }
            }
            Err(why) => {
                tracing::warn!(artifact_id = %artifact, why, "an artifact could not be hashed");
                fail_hashing(
                    pool,
                    artifact,
                    owner,
                    &why,
                    seconds(self.settings.retry_after),
                    self.settings.max_attempts,
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Every component's hashes and every track's, or why not.
    async fn hash(
        &self,
        artifact: ArtifactId,
    ) -> Result<
        (
            Vec<(uuid::Uuid, PreservationHashes)>,
            Vec<(u32, PreservationHashes)>,
        ),
        String,
    > {
        let manifest = self
            .manifests
            .read(artifact)
            .await
            .map_err(|error| format!("its manifest could not be read: {error}"))?;
        let mut components = Vec::with_capacity(manifest.components.len());
        let mut tracks = Vec::new();
        for component in &manifest.components {
            let ranges = track_ranges(&manifest, component.id);
            let spans: Vec<(u64, u64)> = ranges
                .iter()
                .map(|(_, start, end)| (*start, *end))
                .collect();
            let path = self.objects.object_path(&component.content.sha256);
            let found = hash_file(&path, &spans).await.map_err(|error| {
                format!(
                    "{} could not be read from the library: {error}",
                    component.logical_path.as_str()
                )
            })?;
            if found.whole.length != component.length_bytes {
                return Err(format!(
                    "{} is {} bytes in the library and its manifest says {}",
                    component.logical_path.as_str(),
                    found.whole.length,
                    component.length_bytes
                ));
            }
            for ((number, _, _), hashes) in ranges.iter().zip(found.ranges) {
                tracks.push((*number, hashes));
            }
            components.push((*component.id.as_uuid(), found.whole));
        }
        tracks.sort_by_key(|(number, _)| *number);
        Ok((components, tracks))
    }
}
