// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! PostgreSQL access: connection pooling, migrations, and repositories.
//!
//! In place: the pool, the migration runner, the readiness probe, and the
//! initial schema, with every constraint covered by an integration test that
//! asserts it actually rejects. Typed repositories follow in epic E2.

pub mod accounts;
pub mod erasures;
pub mod repositories;
pub mod romm;

use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, PgPool};

/// Migrations are embedded at compile time so the shipped binary can run them
/// without carrying a separate SQL directory.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// Anything that can go wrong talking to PostgreSQL.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// The connection URL could not be parsed.
    #[error("database url is not valid")]
    InvalidUrl(#[source] sqlx::Error),
    /// A pool could not be established within the configured timeout.
    #[error("could not connect to the database")]
    Connect(#[source] sqlx::Error),
    /// A migration failed to apply.
    #[error("database migration failed")]
    Migrate(#[source] sqlx::migrate::MigrateError),
    /// A query failed.
    #[error("database query failed")]
    Query(#[source] sqlx::Error),
    /// A stored enum value is not one this build understands.
    ///
    /// Means the column's CHECK constraint and the domain enum have drifted
    /// apart. Reported rather than defaulted, because every default available
    /// here would be a guess about the state of a burn.
    #[error("stored {column} value is not one this build understands")]
    Enum {
        /// Which column.
        column: &'static str,
        /// What was stored. Enum values are not secrets.
        value: String,
    },
}

/// Pool configuration. Defaults suit development; production overrides come
/// from configuration rather than from code.
#[derive(Debug, Clone)]
pub struct DbConfig {
    /// PostgreSQL connection URL.
    pub url: String,
    /// Maximum pooled connections.
    pub max_connections: u32,
    /// How long to wait for a connection before giving up.
    pub acquire_timeout: Duration,
}

impl DbConfig {
    /// Build a configuration from a URL, using default pool sizing.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            max_connections: 10,
            acquire_timeout: Duration::from_secs(5),
        }
    }
}

/// A handle to the application database.
#[derive(Debug, Clone)]
pub struct Database {
    pool: PgPool,
}

impl Database {
    /// Connect and establish the pool.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::InvalidUrl`] if the URL cannot be parsed and
    /// [`DbError::Connect`] if no connection can be established in time.
    pub async fn connect(config: &DbConfig) -> Result<Self, DbError> {
        // Statement logging is downgraded to trace so routine queries do not
        // dominate normal logs and parameters stay out of info-level output:
        // logs add context without leaking secrets.
        let options: PgConnectOptions = config
            .url
            .parse::<PgConnectOptions>()
            .map_err(DbError::InvalidUrl)?
            .log_statements(tracing::log::LevelFilter::Trace);

        let pool = PgPoolOptions::new()
            .max_connections(config.max_connections)
            .acquire_timeout(config.acquire_timeout)
            .connect_with(options)
            .await
            .map_err(DbError::Connect)?;

        Ok(Self { pool })
    }

    /// Build the pool without contacting the server.
    ///
    /// `serve` uses this so the HTTP listener comes up even while PostgreSQL
    /// is still starting. The server then reports `degraded` from `/readyz`
    /// until the first connection succeeds, which is more useful to an
    /// operator than a crash loop. Commands that cannot do anything without a
    /// database (`migrate`, `doctor`) use [`Database::connect`] instead.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::InvalidUrl`] if the URL cannot be parsed.
    pub fn connect_lazy(config: &DbConfig) -> Result<Self, DbError> {
        let options: PgConnectOptions = config
            .url
            .parse::<PgConnectOptions>()
            .map_err(DbError::InvalidUrl)?
            .log_statements(tracing::log::LevelFilter::Trace);

        let pool = PgPoolOptions::new()
            .max_connections(config.max_connections)
            .acquire_timeout(config.acquire_timeout)
            .connect_lazy_with(options);

        Ok(Self { pool })
    }

    /// Borrow the underlying pool for repository use.
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Round-trip a trivial query to confirm the database is reachable.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::Query`] if the round trip fails.
    pub async fn ping(&self) -> Result<(), DbError> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map_err(DbError::Query)?;
        Ok(())
    }

    /// Apply all pending migrations.
    ///
    /// Migrations are forward-only in production; rollback is restore-based.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::Migrate`] if any migration fails to apply.
    pub async fn migrate(&self) -> Result<(), DbError> {
        MIGRATOR.run(&self.pool).await.map_err(DbError::Migrate)
    }
}

pub use repositories::{ClaimOutcome, ClaimedJob, IncomingEvent};
