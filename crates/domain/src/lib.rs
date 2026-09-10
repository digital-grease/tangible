// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Tangible domain model: entities, value objects, and state machines.
//!
//! This crate is deliberately free of database, HTTP, filesystem and
//! subprocess concerns. It performs no I/O. Everything here is pure data and
//! pure rules, so domain invariants can be unit tested without a database or
//! a disc drive.
//!
//! In place: identifiers, digests, the validated [`LogicalPath`] trust
//! boundary, the closed enum sets, and the import and burn state machines.
//! The entity structs that carry these values are added alongside the
//! repositories in epic E2.

pub mod burn;
pub mod cd;
pub mod digest;
pub mod enums;
pub mod id;
pub mod import;
pub mod logical_path;
pub mod manifest;

pub use burn::{BurnAttemptState, BurnJobState, BurnTransitionError};
pub use digest::{DigestParseError, Sha256Digest};
pub use enums::{
    ArtifactFormat, ArtifactKind, ArtifactOrigin, CompatibilityClaim, ComponentRole,
    DiscRelationship, DriveStatus, EjectPolicy, EnumParseError, HashAlgorithm, IntegrationKind,
    LossCharacter, MediaFamily, PhysicalCopyStatus, QuarantineState, SetKind, TitleKind,
    ValidationState, VerificationStep, WorkerStatus,
};
pub use id::{
    ArtifactId, BurnAttemptId, BurnJobId, ComponentId, DiscId, DiscSetId, DriveId, EditionId,
    EventId, IdParseError, ImportJobId, IntegrationId, PhysicalCopyId, TitleId, WorkerId,
};
pub use import::{ImportState, ImportTransitionError};
pub use logical_path::{LogicalPath, LogicalPathError};
pub use manifest::{ArtifactManifest, ManifestError, ManifestParseError, SCHEMA_ID};
