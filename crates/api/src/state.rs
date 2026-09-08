// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Shared handler state.

use std::sync::Arc;

use tangible_db::Database;
use tangible_storage::{ManifestStore, WatchRoots};

use crate::import::ImportPipeline;

/// State cloned into every request handler.
///
/// Cheap to clone: the inner value is shared, and the pool inside `Database`
/// is itself a handle rather than a connection.
#[derive(Debug, Clone)]
pub struct ApiState {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    database: Database,
    /// Absent until a library root is configured, which is why the accessor
    /// returns an option rather than panicking: the server must still start
    /// and report readiness when storage is misconfigured.
    manifests: Option<ManifestStore>,
    /// Absent for the same reason: importing needs storage, and a server
    /// without it should say so on the import route rather than fail to boot.
    imports: Option<ImportContext>,
}

/// What the import routes need beyond the database.
#[derive(Debug, Clone)]
pub struct ImportContext {
    /// Runs imports, and owns the staging area uploads land in.
    pub pipeline: ImportPipeline,
    /// Directories an administrator has said may be imported from.
    ///
    /// Empty is the safe default: with none configured, no watched-folder
    /// import can name a path at all.
    pub roots: WatchRoots,
    /// Largest upload accepted, in bytes.
    pub max_upload_bytes: u64,
}

impl ApiState {
    /// Assemble state from its dependencies.
    #[must_use]
    pub fn new(database: Database) -> Self {
        Self {
            inner: Arc::new(Inner {
                database,
                manifests: None,
                imports: None,
            }),
        }
    }

    /// Attach a manifest store, enabling the library routes.
    #[must_use]
    pub fn with_manifests(database: Database, manifests: ManifestStore) -> Self {
        Self {
            inner: Arc::new(Inner {
                database,
                manifests: Some(manifests),
                imports: None,
            }),
        }
    }

    /// Attach the import context, enabling the import routes.
    #[must_use]
    pub fn with_imports(self, imports: ImportContext) -> Self {
        Self {
            inner: Arc::new(Inner {
                database: self.inner.database.clone(),
                manifests: self.inner.manifests.clone(),
                imports: Some(imports),
            }),
        }
    }

    /// The manifest store, when a library root is configured.
    #[must_use]
    pub fn manifests(&self) -> Option<&ManifestStore> {
        self.inner.manifests.as_ref()
    }

    /// The import context, when storage is configured.
    #[must_use]
    pub fn imports(&self) -> Option<&ImportContext> {
        self.inner.imports.as_ref()
    }

    /// The application database.
    #[must_use]
    pub fn database(&self) -> &Database {
        &self.inner.database
    }
}
