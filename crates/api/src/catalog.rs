// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Putting a stored artifact into the catalog.
//!
//! The import pipeline stores bytes and publishes a manifest; the manifest is
//! the durable, portable record and the catalog can be rebuilt from it. But a
//! burn job references an artifact by identity, and the catalog is where
//! identity is enforced, so an artifact nobody registered can be listed in the
//! library and still not be burnable.
//!
//! This module is the join: it reads one manifest and writes the rows that
//! describe it. Nothing here decides anything: every value comes from the
//! manifest, which is why the mapping is a pure function with the database
//! call around it.

use tangible_db::repositories::{
    ArtifactRegistration, ComponentRegistration, RegisteredArtifact, register_artifact,
};
use tangible_db::{Database, DbError};
use tangible_domain::manifest::ArtifactManifest;
use tangible_domain::{ArtifactId, ComponentId};

/// A manifest flattened into owned values the repository can borrow.
///
/// Owned because the registration structs borrow strings, and the pieces they
/// borrow (enum names, hex digests, rendered paths) do not exist as strings
/// anywhere in the manifest.
#[derive(Debug, Clone)]
pub struct CatalogRows {
    artifact_id: ArtifactId,
    artifact_kind: String,
    format: String,
    origin: String,
    manifest_version: String,
    total_bytes: i64,
    validation_state: String,
    validation_summary: Option<serde_json::Value>,
    primary_sha256: Option<String>,
    components: Vec<ComponentRow>,
}

#[derive(Debug, Clone)]
struct ComponentRow {
    component_id: ComponentId,
    logical_path: String,
    role: String,
    ordinal: i32,
    length_bytes: i64,
    sha256: String,
    source_filename: Option<String>,
    media_type: Option<String>,
}

/// Flatten a manifest into the rows that describe it.
///
/// Pure, so what the catalog will say about an artifact can be asserted
/// without a database.
#[must_use]
pub fn rows_for(manifest: &ArtifactManifest) -> CatalogRows {
    let components: Vec<ComponentRow> = manifest
        .components
        .iter()
        .map(|component| ComponentRow {
            component_id: component.id,
            logical_path: component.logical_path.to_string(),
            role: component.role.to_string(),
            // Ordinals are u32 in the manifest and INTEGER in the column.
            // Saturating rather than wrapping: an ordinal that large is
            // impossible, and wrapping it would collide with another
            // component's.
            ordinal: i32::try_from(component.ordinal).unwrap_or(i32::MAX),
            length_bytes: i64::try_from(component.length_bytes).unwrap_or(i64::MAX),
            sha256: component.content.sha256.to_hex(),
            source_filename: component.source_filename.clone(),
            media_type: component.media_type.clone(),
        })
        .collect();

    // The component that stands for the artifact: the primary image if one is
    // named, otherwise the first. A single-file ISO has exactly one either
    // way; a CUE/BIN set has a descriptor and its data tracks, and the data
    // is what a lookup by digest should find.
    let primary = manifest
        .components
        .iter()
        .find(|component| component.role == tangible_domain::ComponentRole::PrimaryImage)
        .or_else(|| manifest.components.first());

    // Validators are kept only when they had something to say. An empty
    // summary and a null one mean the same thing, and null is smaller.
    let validation_summary = if manifest.validation.validators.is_empty() {
        None
    } else {
        serde_json::to_value(&manifest.validation.validators).ok()
    };

    CatalogRows {
        artifact_id: manifest.artifact_id,
        artifact_kind: manifest.classification.artifact_kind.to_string(),
        format: manifest.classification.format.to_string(),
        origin: manifest.origin.kind.to_string(),
        manifest_version: manifest.schema.clone(),
        total_bytes: i64::try_from(manifest.classification.total_bytes).unwrap_or(i64::MAX),
        validation_state: manifest.validation.state.to_string(),
        validation_summary,
        primary_sha256: primary.map(|component| component.content.sha256.to_hex()),
        components,
    }
}

/// Register an artifact described by a manifest.
///
/// Idempotent, like the registration underneath it: an import that stored its
/// bytes and then failed before answering can run this again.
///
/// # Errors
///
/// [`DbError::Query`] on a database failure.
pub async fn register_manifest(
    database: &Database,
    manifest: &ArtifactManifest,
) -> Result<RegisteredArtifact, DbError> {
    let rows = rows_for(manifest);
    let components: Vec<ComponentRegistration<'_>> = rows
        .components
        .iter()
        .map(|component| ComponentRegistration {
            component_id: component.component_id,
            logical_path: &component.logical_path,
            role: &component.role,
            ordinal: component.ordinal,
            length_bytes: component.length_bytes,
            sha256: &component.sha256,
            source_filename: component.source_filename.as_deref(),
            media_type: component.media_type.as_deref(),
        })
        .collect();

    let registered = register_artifact(
        database.pool(),
        ArtifactRegistration {
            artifact_id: rows.artifact_id,
            artifact_kind: &rows.artifact_kind,
            format: &rows.format,
            origin: &rows.origin,
            manifest_version: &rows.manifest_version,
            total_bytes: rows.total_bytes,
            validation_state: &rows.validation_state,
            validation_summary: rows.validation_summary.as_ref(),
            primary_sha256: rows.primary_sha256.as_deref(),
            components: &components,
        },
    )
    .await?;

    if registered.created {
        tracing::info!(
            artifact_id = %manifest.artifact_id,
            components = rows.components.len(),
            "artifact registered in the catalog"
        );
    } else {
        tracing::info!(
            artifact_id = %manifest.artifact_id,
            "artifact was already registered"
        );
    }
    Ok(registered)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use tangible_domain::manifest::{
        BurnSupport, Classification, Compatibility, Component, ContentHashes, Origin, StorageRef,
        Topology, Validation,
    };
    use tangible_domain::{
        ArtifactFormat, ArtifactId, ArtifactKind, ArtifactOrigin, ComponentId, ComponentRole,
        LogicalPath, Sha256Digest, ValidationState,
    };
    use time::OffsetDateTime;

    fn digest(byte: u8) -> Sha256Digest {
        Sha256Digest::from_bytes([byte; 32])
    }

    fn component(path: &str, role: ComponentRole, ordinal: u32, byte: u8) -> Component {
        Component {
            id: ComponentId::generate(),
            logical_path: LogicalPath::parse(path).expect("a valid path"),
            source_filename: Some(path.to_owned()),
            role,
            ordinal,
            length_bytes: 2048,
            content: ContentHashes {
                sha256: digest(byte),
                sha1: None,
                md5: None,
                crc32: None,
            },
            storage: StorageRef::for_digest(&digest(byte)),
            media_type: None,
            attributes: BTreeMap::default(),
        }
    }

    fn manifest(components: Vec<Component>) -> ArtifactManifest {
        let total: u64 = components.iter().map(|c| c.length_bytes).sum();
        ArtifactManifest {
            schema: tangible_domain::manifest::SCHEMA_ID.to_owned(),
            artifact_id: ArtifactId::generate(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            origin: Origin {
                kind: ArtifactOrigin::ImportedOriginal,
                source_kind: "upload".to_owned(),
                source_reference: None,
                source_filename: Some("disc.iso".to_owned()),
                acquired_at: None,
                operator_note: None,
            },
            classification: Classification {
                artifact_kind: ArtifactKind::SingleFileImage,
                format: ArtifactFormat::Iso,
                format_confidence: 1.0,
                detectors: vec![],
                media_family: None,
                total_bytes: total,
            },
            components,
            topology: Topology::Unknown,
            validation: Validation {
                state: ValidationState::Valid,
                validators: vec![],
                validated_at: None,
            },
            lineage: None,
            associations: vec![],
            compatibility: Compatibility {
                structurally_valid: true,
                burn_support: BurnSupport {
                    state: "unknown".to_owned(),
                    profiles: vec![],
                    engines: vec![],
                    warnings: vec![],
                },
                target_claims: vec![],
            },
            extensions: BTreeMap::default(),
        }
    }

    #[test]
    fn every_component_becomes_a_row() {
        let rows = rows_for(&manifest(vec![
            component("disc.cue", ComponentRole::Descriptor, 0, 1),
            component("disc.bin", ComponentRole::TrackData, 1, 2),
        ]));
        assert_eq!(rows.components.len(), 2);
        assert_eq!(rows.total_bytes, 4096);
    }

    #[test]
    fn the_primary_image_is_what_the_artifact_is_looked_up_by() {
        // For a CUE/BIN set the descriptor comes first, but the data is what a
        // digest lookup should find.
        let rows = rows_for(&manifest(vec![
            component("disc.cue", ComponentRole::Descriptor, 0, 1),
            component("disc.bin", ComponentRole::PrimaryImage, 1, 2),
        ]));
        assert_eq!(
            rows.primary_sha256.as_deref(),
            Some(digest(2).to_hex().as_str())
        );
    }

    #[test]
    fn an_artifact_without_a_named_primary_falls_back_to_its_first_component() {
        let rows = rows_for(&manifest(vec![component(
            "disc.iso",
            ComponentRole::Unknown,
            0,
            7,
        )]));
        assert_eq!(
            rows.primary_sha256.as_deref(),
            Some(digest(7).to_hex().as_str())
        );
    }

    #[test]
    fn enum_values_are_written_as_the_text_the_schema_checks() {
        // The columns have CHECK constraints listing these exact strings, so a
        // mapping that invented its own spelling would fail at the database.
        let rows = rows_for(&manifest(vec![component(
            "disc.iso",
            ComponentRole::PrimaryImage,
            0,
            1,
        )]));
        assert_eq!(rows.format, "iso");
        assert_eq!(rows.artifact_kind, "single_file_image");
        assert_eq!(rows.origin, "imported_original");
        assert_eq!(rows.validation_state, "valid");
        assert_eq!(rows.components[0].role, "primary_image");
    }

    #[test]
    fn a_clean_validation_stores_no_summary() {
        // An empty summary and a null one mean the same thing.
        let rows = rows_for(&manifest(vec![component(
            "disc.iso",
            ComponentRole::PrimaryImage,
            0,
            1,
        )]));
        assert!(rows.validation_summary.is_none());
    }

    #[test]
    fn digests_are_written_as_lowercase_hex() {
        let rows = rows_for(&manifest(vec![component(
            "disc.iso",
            ComponentRole::PrimaryImage,
            0,
            0xab,
        )]));
        let hex = &rows.components[0].sha256;
        assert_eq!(hex.len(), 64);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
    }
}
