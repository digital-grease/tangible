// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Reading and writing artifact manifests on disk.
//!
//! Manifests live at `manifests/<artifact-uuid>.json` beside the object tree.
//!
//! Publication is ordered, not just atomic. A manifest is renamed into place
//! only after every component it references has been confirmed present in the
//! object store. Writing it first would create a window in which the manifest
//! promises bytes that do not exist yet; and since the manifest is what lets
//! the catalog be rebuilt, that window is exactly when a crash does the most
//! damage.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use tangible_domain::manifest::{ArtifactManifest, ManifestParseError};
use tangible_domain::{ArtifactId, Sha256Digest};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::filesystem::{FilesystemStore, StorageError};

/// Directory holding manifests, relative to the library root.
pub const MANIFESTS_DIR: &str = "manifests";

/// Failures specific to manifest persistence.
#[derive(Debug, thiserror::Error)]
pub enum ManifestStoreError {
    /// The manifest is malformed or internally inconsistent.
    #[error("manifest is not valid")]
    Invalid(#[from] ManifestParseError),

    /// A component the manifest references is not in the object store.
    ///
    /// Refused rather than warned about: a manifest naming absent bytes cannot
    /// rebuild anything, and publishing it would record a lie.
    #[error("manifest references object {digest} for {path}, which is not in the store")]
    MissingObject {
        /// The absent digest.
        digest: String,
        /// The component that references it.
        path: String,
    },

    /// A component's recorded length disagrees with the stored object.
    #[error("component {path} declares {declared} bytes but the stored object is {actual}")]
    LengthMismatch {
        /// The component.
        path: String,
        /// Length the manifest claims.
        declared: u64,
        /// Length on disk.
        actual: u64,
    },

    /// No manifest exists for that artifact.
    #[error("no manifest for artifact {artifact_id}")]
    NotFound {
        /// The artifact asked for.
        artifact_id: String,
    },

    /// An underlying storage failure.
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// An I/O failure.
    #[error("{operation} failed for {path}")]
    Io {
        /// What was attempted.
        operation: &'static str,
        /// Path involved.
        path: PathBuf,
        /// Cause.
        #[source]
        source: std::io::Error,
    },

    /// Serialization failed.
    #[error("manifest could not be serialized")]
    Serialize(#[source] serde_json::Error),
}

/// Manifest persistence beside an object store.
#[derive(Debug, Clone)]
pub struct ManifestStore {
    root: PathBuf,
    objects: FilesystemStore,
}

impl ManifestStore {
    /// Open manifest storage alongside an existing object store.
    ///
    /// # Errors
    ///
    /// [`ManifestStoreError::Io`] if the manifests directory cannot be created.
    pub async fn open(objects: FilesystemStore) -> Result<Self, ManifestStoreError> {
        let root = objects.root().to_path_buf();
        let dir = root.join(MANIFESTS_DIR);
        fs::create_dir_all(&dir)
            .await
            .map_err(|source| ManifestStoreError::Io {
                operation: "creating the manifests directory",
                path: dir,
                source,
            })?;
        Ok(Self { root, objects })
    }

    /// Path of one artifact's manifest.
    #[must_use]
    pub fn manifest_path(&self, artifact_id: ArtifactId) -> PathBuf {
        self.root
            .join(MANIFESTS_DIR)
            .join(format!("{artifact_id}.json"))
    }

    /// Validate, confirm every referenced object is present, then publish.
    ///
    /// The presence check is the point of this function. Anyone can serialize
    /// JSON; what makes a published manifest trustworthy is that its
    /// references were verified against the store at the moment it was
    /// written.
    ///
    /// # Errors
    ///
    /// [`ManifestStoreError::Invalid`] if the manifest breaks its own rules,
    /// [`ManifestStoreError::MissingObject`] or
    /// [`ManifestStoreError::LengthMismatch`] if a reference does not resolve,
    /// or [`ManifestStoreError::Io`] on a filesystem failure.
    pub async fn write(&self, manifest: &ArtifactManifest) -> Result<PathBuf, ManifestStoreError> {
        manifest
            .validate()
            .map_err(|error| ManifestStoreError::Invalid(ManifestParseError::Invalid(error)))?;

        // Every referenced object must exist, at the declared length, before
        // the manifest becomes visible.
        for component in &manifest.components {
            let digest: &Sha256Digest = &component.content.sha256;
            match self.objects.stat(digest).await? {
                None => {
                    return Err(ManifestStoreError::MissingObject {
                        digest: digest.to_hex(),
                        path: component.logical_path.to_string(),
                    });
                }
                Some(stat) if stat.size_bytes != component.length_bytes => {
                    return Err(ManifestStoreError::LengthMismatch {
                        path: component.logical_path.to_string(),
                        declared: component.length_bytes,
                        actual: stat.size_bytes,
                    });
                }
                Some(_) => {}
            }
        }

        let json = manifest.to_json().map_err(ManifestStoreError::Serialize)?;
        let destination = self.manifest_path(manifest.artifact_id);

        // Temporary file beside the destination so the rename is atomic, and
        // uniquely named so concurrent writers cannot collide.
        let temporary = destination.with_extension(format!("{}.part", uuid::Uuid::now_v7()));

        let write_result = async {
            let mut file =
                fs::File::create(&temporary)
                    .await
                    .map_err(|source| ManifestStoreError::Io {
                        operation: "creating a temporary manifest",
                        path: temporary.clone(),
                        source,
                    })?;
            file.write_all(json.as_bytes())
                .await
                .map_err(|source| ManifestStoreError::Io {
                    operation: "writing a temporary manifest",
                    path: temporary.clone(),
                    source,
                })?;
            file.sync_all()
                .await
                .map_err(|source| ManifestStoreError::Io {
                    operation: "syncing a temporary manifest",
                    path: temporary.clone(),
                    source,
                })?;
            Ok::<(), ManifestStoreError>(())
        }
        .await;

        if let Err(error) = write_result {
            let _ = fs::remove_file(&temporary).await;
            return Err(error);
        }

        fs::rename(&temporary, &destination)
            .await
            .map_err(|source| ManifestStoreError::Io {
                operation: "publishing the manifest",
                path: destination.clone(),
                source,
            })?;

        tracing::debug!(
            artifact_id = %manifest.artifact_id,
            components = manifest.components.len(),
            "published manifest"
        );

        Ok(destination)
    }

    /// Read and validate one artifact's manifest.
    ///
    /// # Errors
    ///
    /// [`ManifestStoreError::NotFound`] if absent,
    /// [`ManifestStoreError::Invalid`] if it fails its own rules, or
    /// [`ManifestStoreError::Io`] on a read failure.
    pub async fn read(
        &self,
        artifact_id: ArtifactId,
    ) -> Result<ArtifactManifest, ManifestStoreError> {
        let path = self.manifest_path(artifact_id);
        let text = match fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Err(ManifestStoreError::NotFound {
                    artifact_id: artifact_id.to_string(),
                });
            }
            Err(source) => {
                return Err(ManifestStoreError::Io {
                    operation: "reading a manifest",
                    path,
                    source,
                });
            }
        };
        Ok(ArtifactManifest::from_json(&text)?)
    }

    /// Digest of a published manifest, exactly as stored.
    ///
    /// A worker fetches the manifest over HTTP and stages from what it says,
    /// so it is told the digest when its work is leased and checks the bytes
    /// it received against it. That does not make the manifest trustworthy
    /// (it came from the same server), but it does catch a document altered or
    /// truncated between publication and the download.
    ///
    /// # Errors
    ///
    /// [`ManifestStoreError::NotFound`] if no manifest is published for the
    /// artifact, or [`ManifestStoreError::Io`] on a read failure.
    pub async fn digest(
        &self,
        artifact_id: ArtifactId,
    ) -> Result<tangible_domain::Sha256Digest, ManifestStoreError> {
        use sha2::Digest as _;

        let path = self.manifest_path(artifact_id);
        let bytes = match fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Err(ManifestStoreError::NotFound {
                    artifact_id: artifact_id.to_string(),
                });
            }
            Err(source) => {
                return Err(ManifestStoreError::Io {
                    operation: "reading a manifest",
                    path,
                    source,
                });
            }
        };

        let mut hasher = sha2::Sha256::new();
        hasher.update(&bytes);
        Ok(tangible_domain::Sha256Digest::from_bytes(
            hasher.finalize().into(),
        ))
    }

    /// Every artifact that has a manifest.
    ///
    /// Used to rebuild the catalog from storage alone. Entries that are not
    /// well-formed manifest filenames are skipped rather than failing the
    /// whole scan, so one stray file cannot block a rebuild.
    ///
    /// # Errors
    ///
    /// [`ManifestStoreError::Io`] if the directory cannot be read.
    pub async fn list(&self) -> Result<Vec<ArtifactId>, ManifestStoreError> {
        let dir = self.root.join(MANIFESTS_DIR);
        let mut entries = fs::read_dir(&dir)
            .await
            .map_err(|source| ManifestStoreError::Io {
                operation: "listing manifests",
                path: dir.clone(),
                source,
            })?;

        let mut found = Vec::new();
        while let Some(entry) =
            entries
                .next_entry()
                .await
                .map_err(|source| ManifestStoreError::Io {
                    operation: "walking the manifests directory",
                    path: dir.clone(),
                    source,
                })?
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                && let Ok(id) = stem.parse::<ArtifactId>()
            {
                found.push(id);
            }
        }
        found.sort_unstable();
        Ok(found)
    }

    /// The object store these manifests describe.
    #[must_use]
    pub const fn objects(&self) -> &FilesystemStore {
        &self.objects
    }

    /// The library root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}
