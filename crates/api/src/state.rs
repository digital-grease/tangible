// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Shared handler state.

use std::sync::Arc;

use tangible_db::Database;

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
}

impl ApiState {
    /// Assemble state from its dependencies.
    #[must_use]
    pub fn new(database: Database) -> Self {
        Self {
            inner: Arc::new(Inner { database }),
        }
    }

    /// The application database.
    #[must_use]
    pub fn database(&self) -> &Database {
        &self.inner.database
    }
}
