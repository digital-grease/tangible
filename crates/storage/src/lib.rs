// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Content-addressed storage, staging, and atomic writes.
//!
//! Canonical bytes are written to a temporary file inside the objects tree,
//! fsynced, and then atomically renamed into place, so a crash can never leave
//! a partially written object visible under a digest that claims to describe it.
//!
//! The digest is always computed from the bytes actually read. A caller cannot
//! supply one, which makes "never trust a client-provided digest" a property
//! of the API rather than a rule to remember.
//!
//! In place: the filesystem CAS, manifest persistence, and staging.
//! Materialization and garbage collection follow.

pub mod filesystem;
pub mod manifest_store;
pub mod object_key;
pub mod staging;

pub use filesystem::{
    FilesystemStore, IngestLimits, Ingested, ObjectStat, STORAGE_VERSION, StorageError,
};
pub use manifest_store::{ManifestStore, ManifestStoreError};
pub use object_key::ObjectKey;
pub use staging::{StagingArea, StagingError, StagingKind, StagingManager};
