// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The portable artifact manifest.
//!
//! A manifest describes bytes that already exist. It carries no executable
//! instructions, and reading one must never cause anything to run.
//!
//! Its job is to make an artifact self-describing away from this database: the
//! catalog can be rebuilt from manifests, an artifact can be moved between
//! storage backends, and another tool can understand multi-file topology
//! without knowing anything about host paths.
//!
//! Three properties are enforced by the types rather than left to callers:
//!
//! - **Unknown fields are rejected.** Every struct is `deny_unknown_fields`
//!   except `extensions` and `attributes`. A manifest written by a newer
//!   version fails loudly instead of being silently half-understood, which for
//!   a preservation format is the difference between a clear error and quiet
//!   data loss.
//! - **Paths are validated on the way in.** `logical_path` is a
//!   [`LogicalPath`], so deserializing a manifest containing `../escape` fails
//!   at parse time rather than at materialization time.
//! - **SHA-256 is not optional.** It is a plain field, not an `Option`, so a
//!   component without one cannot be represented.
//!
//! Serialization is deterministic: struct field order is fixed, and the
//! free-form maps are [`BTreeMap`]s so their keys sort. The same artifact
//! always produces byte-identical JSON, which is what lets a manifest be
//! hashed and compared.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::digest::Sha256Digest;
use crate::enums::{
    ArtifactFormat, ArtifactKind, ArtifactOrigin, DiscRelationship, LossCharacter, MediaFamily,
    ValidationState,
};
use crate::id::{ArtifactId, ComponentId, DiscId};
use crate::logical_path::LogicalPath;

/// The only schema identifier this build reads or writes.
pub const SCHEMA_ID: &str = "org.tangible.artifact-manifest/v1alpha1";

/// Free-form namespaced data. Sorted so output is byte-stable.
pub type Extensions = BTreeMap<String, serde_json::Value>;

/// Why a manifest was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    /// The schema identifier is not one this build understands.
    #[error("unsupported manifest schema {found:?}, expected {SCHEMA_ID:?}")]
    UnsupportedSchema {
        /// Identifier found in the document.
        found: String,
    },

    /// Two components claim the same ordinal.
    #[error("duplicate component ordinal {ordinal}")]
    DuplicateOrdinal {
        /// The repeated ordinal.
        ordinal: u32,
    },

    /// Two components claim the same logical path.
    #[error("duplicate component path {path}")]
    DuplicatePath {
        /// The repeated path.
        path: String,
    },

    /// A component's stored object key disagrees with its digest.
    ///
    /// The key is redundant (it is derivable from the digest), and it is
    /// included only for portability. That redundancy is worth checking: a
    /// mismatch means the manifest was hand-edited or corrupted, and trusting
    /// the key would read the wrong object.
    #[error("component {path} has object key {key:?}, which does not match its digest")]
    ObjectKeyMismatch {
        /// Path of the offending component.
        path: String,
        /// The key recorded in the manifest.
        key: String,
    },

    /// The manifest declares a component count or size that disagrees with the
    /// components it actually lists.
    #[error("{field} is {declared} but the components total {actual}")]
    DeclaredTotalMismatch {
        /// Which field disagrees.
        field: &'static str,
        /// Value the manifest declares.
        declared: u64,
        /// Value computed from the component list.
        actual: u64,
    },

    /// A manifest with no components describes nothing.
    #[error("manifest lists no components")]
    NoComponents,

    /// A derivative's lineage names its own artifact as parent.
    #[error("lineage names the artifact as its own parent")]
    SelfParent,
}

/// A portable description of one immutable artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactManifest {
    /// Schema identifier. Always [`SCHEMA_ID`] for documents this build writes.
    pub schema: String,
    /// Catalog identity. Deliberately not a content digest: the same bytes may
    /// be catalogued more than once, and an artifact keeps its identity across
    /// metadata edits.
    pub artifact_id: ArtifactId,
    /// When the manifest was written.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Where the bytes came from.
    pub origin: Origin,
    /// What the artifact was detected to be.
    pub classification: Classification,
    /// The files making up the artifact, in ordinal order.
    pub components: Vec<Component>,
    /// Format-specific structure.
    pub topology: Topology,
    /// Structural validation outcome.
    pub validation: Validation,
    /// Present only on a derivative.
    #[serde(default)]
    pub lineage: Option<Lineage>,
    /// Links to logical discs.
    #[serde(default)]
    pub associations: Vec<Association>,
    /// What reproduction is expected to achieve.
    pub compatibility: Compatibility,
    /// Namespaced extension data. The one place unknown keys are allowed.
    #[serde(default)]
    pub extensions: Extensions,
}

/// Provenance of the bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Origin {
    /// Whether this is an original, a derivative, a dump, and so on.
    pub kind: ArtifactOrigin,
    /// The source mechanism, e.g. `watch_folder`.
    pub source_kind: String,
    /// A reference to the source that is safe to publish.
    ///
    /// Must never contain credentials, signed URLs, session tokens or private
    /// headers. A manifest travels with the artifact, so anything recorded
    /// here should be assumed to leave the operator's machine.
    #[serde(default)]
    pub source_reference: Option<String>,
    /// The filename as received, preserved because it often carries the only
    /// human-meaningful naming an import has.
    #[serde(default)]
    pub source_filename: Option<String>,
    /// When acquisition completed.
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub acquired_at: Option<OffsetDateTime>,
    /// Operator's free-text note.
    #[serde(default)]
    pub operator_note: Option<String>,
}

/// What the artifact was detected to be.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Classification {
    /// Shape of the artifact.
    pub artifact_kind: ArtifactKind,
    /// Container format.
    pub format: ArtifactFormat,
    /// How confident detection was, from 0 to 1.
    ///
    /// Diagnostic only. It never substitutes for validation, and a high score
    /// on an invalid image is still invalid.
    pub format_confidence: f32,
    /// What each detector saw. Evidence, not just a verdict.
    #[serde(default)]
    pub detectors: Vec<Detector>,
    /// Physical media family, when known.
    #[serde(default)]
    pub media_family: Option<MediaFamily>,
    /// Sum of all component lengths.
    pub total_bytes: u64,
}

/// One detector's finding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Detector {
    /// Detector name.
    pub name: String,
    /// Detector version, so an old verdict can be re-examined.
    pub version: String,
    /// What it observed, in human-readable fragments.
    #[serde(default)]
    pub evidence: Vec<String>,
}

/// One file within the artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Component {
    /// Stable component identity.
    pub id: ComponentId,
    /// Portable path inside the artifact. Validated on deserialization.
    pub logical_path: LogicalPath,
    /// Original filename, preserved separately because the logical path is
    /// normalized and may differ.
    #[serde(default)]
    pub source_filename: Option<String>,
    /// What part this file plays.
    pub role: crate::enums::ComponentRole,
    /// Position in the artifact. Unique and stable.
    pub ordinal: u32,
    /// Size in bytes.
    pub length_bytes: u64,
    /// Content digests.
    pub content: ContentHashes,
    /// Where the bytes live in the object store.
    pub storage: StorageRef,
    /// MIME type, if one applies.
    #[serde(default)]
    pub media_type: Option<String>,
    /// Format-specific extras.
    #[serde(default)]
    pub attributes: Extensions,
}

/// Digests of one component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentHashes {
    /// Mandatory. Not an `Option`, so a component without one cannot exist.
    pub sha256: Sha256Digest,
    /// Retained for matching preservation databases, never for identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha1: Option<String>,
    /// Retained for matching preservation databases, never for identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub md5: Option<String>,
    /// Retained for matching preservation databases, never for identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crc32: Option<String>,
}

/// Where a component's bytes are stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageRef {
    /// Digest algorithm naming the object tree.
    pub algorithm: String,
    /// Object key, e.g. `sha256/ab/cd/<digest>`. Redundant but portable, and
    /// checked against the digest by [`ArtifactManifest::validate`].
    pub object_key: String,
}

impl StorageRef {
    /// The key a given digest should have.
    #[must_use]
    pub fn expected_key(digest: &Sha256Digest) -> String {
        let hex = digest.to_hex();
        format!("sha256/{}/{}/{hex}", &hex[0..2], &hex[2..4])
    }

    /// Build a reference for a digest.
    #[must_use]
    pub fn for_digest(digest: &Sha256Digest) -> Self {
        Self {
            algorithm: "sha256".to_owned(),
            object_key: Self::expected_key(digest),
        }
    }
}

/// Format-specific structure, discriminated by `kind`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Topology {
    /// A single flat block image such as an ISO.
    SingleTrackBlockImage {
        /// Logical block size, typically 2048.
        block_size: u32,
        /// Number of blocks.
        block_count: u64,
        /// Volume identifiers read from the filesystem.
        #[serde(default)]
        volume_labels: Vec<String>,
        /// Filesystems detected inside.
        #[serde(default)]
        filesystems: Vec<FilesystemEvidence>,
    },
    /// A CD described as tracks.
    CdTracks {
        /// The descriptor component, e.g. the CUE file.
        descriptor_component_id: ComponentId,
        /// Number of sessions.
        session_count: u32,
        /// Tracks in order.
        tracks: Vec<Track>,
        /// Subchannel data, when present.
        #[serde(default)]
        subchannel: Option<Subchannel>,
    },
    /// A directory tree such as BDMV or VIDEO_TS.
    DirectoryTree {
        /// What kind of tree, e.g. `bdmv`.
        root_role: String,
        /// Number of entries.
        entry_count: u64,
        /// Total logical size.
        logical_bytes: u64,
        /// Paths that must be present for the tree to be meaningful.
        #[serde(default)]
        required_paths: Vec<LogicalPath>,
    },
    /// Structure not determined. Honest rather than guessed.
    Unknown,
}

/// A filesystem found inside an image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesystemEvidence {
    /// Filesystem type, e.g. `udf`.
    #[serde(rename = "type")]
    pub filesystem_type: String,
    /// Version string, if the filesystem records one.
    #[serde(default)]
    pub version: Option<String>,
    /// How it was identified.
    pub evidence: String,
}

/// One CD track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Track {
    /// Track number, one-based.
    pub number: u32,
    /// Session this track belongs to.
    pub session: u32,
    /// Track mode, e.g. `MODE2/2352` or `AUDIO`.
    pub mode: String,
    /// Component holding this track's data.
    pub component_id: ComponentId,
    /// Byte offset within that component.
    pub file_offset_bytes: u64,
    /// Absolute start LBA.
    pub start_lba: u64,
    /// Length in sectors.
    pub sector_count: u64,
    /// Pregap length in sectors.
    #[serde(default)]
    pub pregap_sectors: u64,
    /// Index points.
    #[serde(default)]
    pub indexes: Vec<TrackIndex>,
    /// Per-track digests, used for preservation-database matching.
    #[serde(default)]
    pub hashes: BTreeMap<String, String>,
}

/// One index point within a track.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackIndex {
    /// Index number.
    pub number: u32,
    /// LBA relative to the track start.
    pub relative_lba: u64,
}

/// Subchannel data accompanying a CD image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subchannel {
    /// Whether subchannel data is present.
    pub present: bool,
    /// How it is stored, when present.
    #[serde(default)]
    pub representation: Option<String>,
}

/// Structural validation outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Validation {
    /// Overall state.
    pub state: ValidationState,
    /// When validation ran.
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub validated_at: Option<OffsetDateTime>,
    /// Individual validator results.
    #[serde(default)]
    pub validators: Vec<ValidatorResult>,
}

/// One validator's finding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidatorResult {
    /// Validator name.
    pub name: String,
    /// Validator version.
    pub version: String,
    /// `pass`, `warning`, or `fail`.
    pub result: String,
    /// Stable machine-readable code. Prose may change; this may not.
    pub code: String,
    /// Structured detail.
    #[serde(default)]
    pub details: Extensions,
}

/// How a derivative relates to its parent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lineage {
    /// The artifact this was derived from.
    pub parent_artifact_id: ArtifactId,
    /// What transformation was applied.
    pub transformation: String,
    /// Which tool performed it.
    pub tool: ToolRef,
    /// Portable, normalized options. Never a raw command line, and never a
    /// host-specific absolute path.
    #[serde(default)]
    pub normalized_options: Extensions,
    /// Deterministic hash of the transformation specification, which makes
    /// derivative creation idempotent.
    pub command_fingerprint: String,
    /// How much fidelity was preserved.
    pub loss_character: LossCharacter,
    /// When the derivation started.
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub started_at: Option<OffsetDateTime>,
    /// When it completed.
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub completed_at: Option<OffsetDateTime>,
}

/// A tool and its version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRef {
    /// Tool name.
    pub name: String,
    /// Tool version, recorded so a derivative can be reproduced or explained.
    pub version: String,
}

/// A link from this artifact to a logical disc.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Association {
    /// The disc.
    pub disc_id: DiscId,
    /// Nature of the link.
    pub relationship: DiscRelationship,
    /// How confident the association is, from 0 to 1.
    pub confidence: f32,
    /// What supports it.
    #[serde(default)]
    pub evidence: Vec<String>,
}

/// What reproduction is expected to achieve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compatibility {
    /// Whether the structure parsed cleanly.
    pub structurally_valid: bool,
    /// Whether and how this can be burned.
    pub burn_support: BurnSupport,
    /// Per-target claims. Separate from burn support on purpose: that a disc
    /// can be written says nothing about whether a given player will accept it.
    #[serde(default)]
    pub target_claims: Vec<TargetClaim>,
}

/// Whether the artifact can be written to media.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BurnSupport {
    /// `supported`, `supported_with_warnings`, `unsupported`, or `unknown`.
    pub state: String,
    /// Media profiles that would work.
    #[serde(default)]
    pub profiles: Vec<String>,
    /// Engines capable of writing it.
    #[serde(default)]
    pub engines: Vec<String>,
    /// Caveats the operator should see before burning.
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// A claim about one playback or execution target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetClaim {
    /// What the claim is about, e.g. `generic-cd-player`.
    pub target: String,
    /// Claim state. Defaults to `unknown`, and anything stronger needs
    /// evidence: the project does not promise that a burned disc will satisfy
    /// console or player authentication.
    pub state: String,
    /// What supports the claim.
    #[serde(default)]
    pub evidence: Vec<String>,
}

impl ArtifactManifest {
    /// Check every internal consistency rule.
    ///
    /// Deserialization already guarantees shape, path safety, and that
    /// SHA-256 is present. This checks the cross-field rules that serde
    /// cannot: schema identity, uniqueness, derivable keys, declared totals,
    /// and lineage sanity.
    ///
    /// # Errors
    ///
    /// The specific [`ManifestError`] describing the first violation found.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.schema != SCHEMA_ID {
            return Err(ManifestError::UnsupportedSchema {
                found: self.schema.clone(),
            });
        }
        if self.components.is_empty() {
            return Err(ManifestError::NoComponents);
        }

        let mut seen_ordinals = std::collections::BTreeSet::new();
        let mut seen_paths = std::collections::BTreeSet::new();
        let mut total: u64 = 0;

        for component in &self.components {
            if !seen_ordinals.insert(component.ordinal) {
                return Err(ManifestError::DuplicateOrdinal {
                    ordinal: component.ordinal,
                });
            }
            if !seen_paths.insert(component.logical_path.as_str()) {
                return Err(ManifestError::DuplicatePath {
                    path: component.logical_path.to_string(),
                });
            }
            let expected = StorageRef::expected_key(&component.content.sha256);
            if component.storage.object_key != expected {
                return Err(ManifestError::ObjectKeyMismatch {
                    path: component.logical_path.to_string(),
                    key: component.storage.object_key.clone(),
                });
            }
            total = total.saturating_add(component.length_bytes);
        }

        let declared_count = self.components.len() as u64;
        if self.classification.total_bytes != total {
            return Err(ManifestError::DeclaredTotalMismatch {
                field: "classification.total_bytes",
                declared: self.classification.total_bytes,
                actual: total,
            });
        }
        debug_assert_eq!(declared_count, self.components.len() as u64);

        if let Some(lineage) = &self.lineage
            && lineage.parent_artifact_id == self.artifact_id
        {
            return Err(ManifestError::SelfParent);
        }

        Ok(())
    }

    /// Serialize deterministically.
    ///
    /// Field order is fixed by the struct definitions and the free-form maps
    /// sort their keys, so the same artifact always produces identical bytes.
    /// That is what allows a manifest to be hashed and compared across
    /// machines.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Parse and validate a manifest document.
    ///
    /// # Errors
    ///
    /// A parse error for malformed JSON, an unknown field, or an unsafe path;
    /// otherwise whatever [`ArtifactManifest::validate`] rejects.
    pub fn from_json(text: &str) -> Result<Self, ManifestParseError> {
        let manifest: Self = serde_json::from_str(text).map_err(ManifestParseError::Decode)?;
        manifest.validate().map_err(ManifestParseError::Invalid)?;
        Ok(manifest)
    }
}

/// Failure to read a manifest document.
#[derive(Debug, thiserror::Error)]
pub enum ManifestParseError {
    /// The document is not valid JSON, has an unknown field, or contains an
    /// unsafe value such as a traversing path.
    #[error("manifest could not be decoded")]
    Decode(#[source] serde_json::Error),
    /// The document decoded but breaks an internal consistency rule.
    #[error("manifest is internally inconsistent")]
    Invalid(#[source] ManifestError),
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::enums::ComponentRole;

    fn digest(byte: u8) -> Sha256Digest {
        Sha256Digest::from_bytes([byte; 32])
    }

    fn component(ordinal: u32, path: &str, byte: u8, len: u64) -> Component {
        let sha256 = digest(byte);
        Component {
            id: ComponentId::generate(),
            logical_path: LogicalPath::parse(path).expect("valid path"),
            source_filename: Some(path.to_owned()),
            role: ComponentRole::PrimaryImage,
            ordinal,
            length_bytes: len,
            storage: StorageRef::for_digest(&sha256),
            content: ContentHashes {
                sha256,
                sha1: None,
                md5: None,
                crc32: None,
            },
            media_type: None,
            attributes: Extensions::new(),
        }
    }

    fn manifest(components: Vec<Component>) -> ArtifactManifest {
        let total = components.iter().map(|c| c.length_bytes).sum();
        ArtifactManifest {
            schema: SCHEMA_ID.to_owned(),
            artifact_id: ArtifactId::generate(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            origin: Origin {
                kind: ArtifactOrigin::ImportedOriginal,
                source_kind: "upload".to_owned(),
                source_reference: None,
                source_filename: None,
                acquired_at: None,
                operator_note: None,
            },
            classification: Classification {
                artifact_kind: ArtifactKind::SingleFileImage,
                format: ArtifactFormat::Iso,
                format_confidence: 0.99,
                detectors: vec![],
                media_family: Some(MediaFamily::Dvd),
                total_bytes: total,
            },
            components,
            topology: Topology::Unknown,
            validation: Validation {
                state: ValidationState::Valid,
                validated_at: None,
                validators: vec![],
            },
            lineage: None,
            associations: vec![],
            compatibility: Compatibility {
                structurally_valid: true,
                burn_support: BurnSupport {
                    state: "supported".to_owned(),
                    profiles: vec![],
                    engines: vec![],
                    warnings: vec![],
                },
                target_claims: vec![],
            },
            extensions: Extensions::new(),
        }
    }

    #[test]
    fn a_well_formed_manifest_round_trips() {
        let original = manifest(vec![component(0, "disc.iso", 0xaa, 100)]);
        let json = original.to_json().expect("serialize");
        let parsed = ArtifactManifest::from_json(&json).expect("parse");
        assert_eq!(parsed, original);
    }

    #[test]
    fn serialization_is_byte_stable() {
        // A manifest that hashes differently on each write could not be used
        // to detect tampering.
        let m = manifest(vec![
            component(0, "b.bin", 0xbb, 10),
            component(1, "a.cue", 0xcc, 20),
        ]);
        assert_eq!(m.to_json().expect("first"), m.to_json().expect("second"));
    }

    #[test]
    fn unknown_top_level_fields_are_rejected() {
        // A newer writer's field must fail loudly rather than be dropped.
        let mut value: serde_json::Value = serde_json::from_str(
            &manifest(vec![component(0, "d.iso", 1, 5)])
                .to_json()
                .unwrap(),
        )
        .unwrap();
        value["surprise"] = serde_json::json!("value");
        let error = ArtifactManifest::from_json(&value.to_string()).expect_err("must reject");
        assert!(matches!(error, ManifestParseError::Decode(_)));
    }

    #[test]
    fn extensions_accept_arbitrary_namespaced_keys() {
        // The one place forward-compatibility is allowed.
        let mut m = manifest(vec![component(0, "d.iso", 1, 5)]);
        m.extensions.insert(
            "org.tangible.redump".to_owned(),
            serde_json::json!({"match_state": "unmatched"}),
        );
        let json = m.to_json().expect("serialize");
        let parsed = ArtifactManifest::from_json(&json).expect("parse");
        assert_eq!(parsed.extensions, m.extensions);
    }

    #[test]
    fn a_traversing_component_path_fails_at_parse_time() {
        // The path type is the boundary, so an unsafe manifest cannot even be
        // represented in memory.
        let json = manifest(vec![component(0, "safe.iso", 1, 5)])
            .to_json()
            .unwrap()
            .replace("\"safe.iso\"", "\"../escape.iso\"");
        let error = ArtifactManifest::from_json(&json).expect_err("must reject");
        assert!(matches!(error, ManifestParseError::Decode(_)));
    }

    #[test]
    fn a_wrong_schema_identifier_is_rejected() {
        let mut m = manifest(vec![component(0, "d.iso", 1, 5)]);
        m.schema = "org.example.something/v2".to_owned();
        assert!(matches!(
            m.validate(),
            Err(ManifestError::UnsupportedSchema { .. })
        ));
    }

    #[test]
    fn duplicate_ordinals_are_rejected() {
        let m = manifest(vec![
            component(0, "a.bin", 1, 5),
            component(0, "b.bin", 2, 5),
        ]);
        assert_eq!(
            m.validate(),
            Err(ManifestError::DuplicateOrdinal { ordinal: 0 })
        );
    }

    #[test]
    fn duplicate_paths_are_rejected() {
        let mut second = component(1, "a.bin", 2, 5);
        second.logical_path = LogicalPath::parse("a.bin").unwrap();
        let m = manifest(vec![component(0, "a.bin", 1, 5), second]);
        assert!(matches!(
            m.validate(),
            Err(ManifestError::DuplicatePath { .. })
        ));
    }

    #[test]
    fn an_object_key_that_does_not_match_its_digest_is_rejected() {
        // The key is derivable, so a mismatch means the manifest was edited or
        // corrupted. Trusting it would read the wrong object.
        let mut m = manifest(vec![component(0, "d.iso", 0xaa, 5)]);
        m.components[0].storage.object_key = "sha256/00/00/deadbeef".to_owned();
        assert!(matches!(
            m.validate(),
            Err(ManifestError::ObjectKeyMismatch { .. })
        ));
    }

    #[test]
    fn object_keys_are_derived_the_same_way_the_store_lays_them_out() {
        let d = digest(0xab);
        let hex = d.to_hex();
        assert_eq!(
            StorageRef::expected_key(&d),
            format!("sha256/{}/{}/{hex}", &hex[0..2], &hex[2..4])
        );
    }

    #[test]
    fn a_declared_total_that_disagrees_with_the_components_is_rejected() {
        let mut m = manifest(vec![component(0, "d.iso", 1, 100)]);
        m.classification.total_bytes = 999;
        assert_eq!(
            m.validate(),
            Err(ManifestError::DeclaredTotalMismatch {
                field: "classification.total_bytes",
                declared: 999,
                actual: 100,
            })
        );
    }

    #[test]
    fn a_manifest_with_no_components_is_rejected() {
        assert_eq!(
            manifest(vec![]).validate(),
            Err(ManifestError::NoComponents)
        );
    }

    #[test]
    fn an_artifact_cannot_be_its_own_parent() {
        let mut m = manifest(vec![component(0, "d.iso", 1, 5)]);
        m.lineage = Some(Lineage {
            parent_artifact_id: m.artifact_id,
            transformation: "chd_create".to_owned(),
            tool: ToolRef {
                name: "chdman".to_owned(),
                version: "0.1".to_owned(),
            },
            normalized_options: Extensions::new(),
            command_fingerprint: "a".repeat(64),
            loss_character: LossCharacter::StructurallyEquivalent,
            started_at: None,
            completed_at: None,
        });
        assert_eq!(m.validate(), Err(ManifestError::SelfParent));
    }

    #[test]
    fn timestamps_serialize_as_rfc_3339() {
        let json = manifest(vec![component(0, "d.iso", 1, 5)])
            .to_json()
            .expect("serialize");
        assert!(
            json.contains("\"created_at\": \"1970-01-01T00:00:00Z\""),
            "expected an RFC 3339 UTC timestamp, got: {json}"
        );
    }

    #[test]
    fn cd_topology_round_trips() {
        let mut m = manifest(vec![component(0, "d.cue", 1, 5)]);
        let component_id = m.components[0].id;
        m.topology = Topology::CdTracks {
            descriptor_component_id: component_id,
            session_count: 1,
            tracks: vec![Track {
                number: 1,
                session: 1,
                mode: "MODE2/2352".to_owned(),
                component_id,
                file_offset_bytes: 0,
                start_lba: 0,
                sector_count: 250_000,
                pregap_sectors: 150,
                indexes: vec![TrackIndex {
                    number: 1,
                    relative_lba: 0,
                }],
                hashes: BTreeMap::new(),
            }],
            subchannel: Some(Subchannel {
                present: false,
                representation: None,
            }),
        };
        let parsed = ArtifactManifest::from_json(&m.to_json().expect("serialize")).expect("parse");
        assert_eq!(parsed.topology, m.topology);
    }

    #[test]
    fn topology_is_discriminated_by_kind() {
        let mut m = manifest(vec![component(0, "d.iso", 1, 5)]);
        m.topology = Topology::SingleTrackBlockImage {
            block_size: 2048,
            block_count: 100,
            volume_labels: vec!["EXAMPLE".to_owned()],
            filesystems: vec![],
        };
        let json = m.to_json().expect("serialize");
        assert!(json.contains("\"kind\": \"single_track_block_image\""));
    }

    #[test]
    fn sha256_cannot_be_omitted() {
        // Not an Option in the type, so a manifest without one fails to parse
        // rather than producing a component with no identity.
        let json = manifest(vec![component(0, "d.iso", 1, 5)])
            .to_json()
            .unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["components"][0]["content"]
            .as_object_mut()
            .unwrap()
            .remove("sha256");
        assert!(ArtifactManifest::from_json(&value.to_string()).is_err());
    }
}
