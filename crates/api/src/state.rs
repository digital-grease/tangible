// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Shared handler state.

use std::sync::Arc;

use tangible_db::Database;
use tangible_storage::ManifestStore;

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
}

impl ApiState {
    /// Assemble state from its dependencies.
    #[must_use]
    pub fn new(database: Database) -> Self {
        Self {
            inner: Arc::new(Inner {
                database,
                manifests: None,
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
            }),
        }
    }

    /// The manifest store, when a library root is configured.
    #[must_use]
    pub fn manifests(&self) -> Option<&ManifestStore> {
        self.inner.manifests.as_ref()
    }

    /// The application database.
    #[must_use]
    pub fn database(&self) -> &Database {
        &self.inner.database
    }
}
