// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! PostgreSQL access: connection pooling, migrations, and repositories.
//!
//! In place: the pool, the migration runner, the readiness probe, and the
//! initial schema, with every constraint covered by an integration test that
//! asserts it actually rejects. Typed repositories follow in epic E2.
//!
//! A query's text is a literal, or is put together from this crate's own
//! constants (column lists, table names, fixed state lists) and nothing
//! else; every value a caller or a client supplies goes through `.bind()`.
//! sqlx takes text it cannot see is literal only through
//! `sqlx::AssertSqlSafe`, and each use here is that promise: keep it.

pub mod accounts;
pub mod audit;
pub mod catalog_edits;
pub mod changes;
pub mod dashboard;
pub mod dats;
pub mod deletions;
pub mod derivations;
pub mod diagnostics;
pub mod drive_activity;
pub mod erasures;
pub mod gc;
pub mod identification;
pub mod integrations;
pub mod integrity;
pub mod library;
pub mod metrics;
pub mod oidc;
pub mod repositories;
pub mod restore;
pub mod rips;
pub mod romm;
pub mod romm_api;
pub mod source_removals;
pub mod uploads;
pub mod workers;

use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, PgPool};
use tangible_domain::SecretString;

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
    /// PostgreSQL connection URL. It carries the password, so it never
    /// prints.
    pub url: SecretString,
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
            url: SecretString::new(url),
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

/// How long the server lets a connection sit idle inside a transaction
/// before ending it. No transaction here waits on anything but the database
/// (AGENTS.md 8: none is held across a network call or a subprocess), so
/// one idle this long has been abandoned, and ending it frees its locks.
const IDLE_IN_TRANSACTION_TIMEOUT: &str = "300s";

/// The connection settings. Statement logging is downgraded to trace so
/// routine queries do not dominate normal logs and parameters stay out of
/// info-level output: logs add context without leaking secrets.
fn connect_options(config: &DbConfig) -> Result<PgConnectOptions, DbError> {
    Ok(config
        .url
        .expose()
        .parse::<PgConnectOptions>()
        .map_err(DbError::InvalidUrl)?
        .log_statements(tracing::log::LevelFilter::Trace)
        .options([(
            "idle_in_transaction_session_timeout",
            IDLE_IN_TRANSACTION_TIMEOUT,
        )]))
}

/// The pool settings, with a check of every connection given back.
///
/// sqlx 0.8 can give a connection back still inside a transaction: when a
/// `begin()` is dropped after sending `BEGIN` and before its answer (a
/// request whose client went away, a timeout), its guard rolls back only
/// transactions it has counted, and it has not counted this one yet. Every
/// statement the next borrower runs then joins that transaction, is never
/// committed, and holds its row locks: an import runner once waited forever
/// on a lock its own earlier, uncommitted progress update held. So each
/// connection given back is asked whether it is in a transaction block, and
/// one that is, is closed, which rolls the transaction back, rather than
/// lent again.
fn pool_options(config: &DbConfig) -> PgPoolOptions {
    PgPoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(config.acquire_timeout)
        .after_release(|connection, _| {
            Box::pin(async move {
                let open = in_transaction_block(connection).await?;
                if open {
                    tracing::warn!(
                        "a database connection was given back inside a transaction; \
                         closing it, which rolls the transaction back"
                    );
                }
                Ok(!open)
            })
        })
}

/// Whether `connection` is inside a transaction block. Outside one, a
/// simple query is its own transaction, which starts when the statement
/// does; inside one, the transaction started at an earlier statement,
/// `BEGIN`. It has to be a simple query: under the extended protocol, which
/// prepared statements use, the statement's time is reset at `Execute`,
/// after `Parse` started the transaction, so the two never agree.
async fn in_transaction_block(connection: &mut sqlx::PgConnection) -> Result<bool, sqlx::Error> {
    use sqlx::{Executor as _, Row as _};
    // A bare string, with no arguments, goes as a simple query.
    connection
        .fetch_one("SELECT transaction_timestamp() <> statement_timestamp()")
        .await?
        .try_get(0)
}

impl Database {
    /// Connect and establish the pool.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::InvalidUrl`] if the URL cannot be parsed and
    /// [`DbError::Connect`] if no connection can be established in time.
    pub async fn connect(config: &DbConfig) -> Result<Self, DbError> {
        let pool = pool_options(config)
            .connect_with(connect_options(config)?)
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
        let pool = pool_options(config).connect_lazy_with(connect_options(config)?);

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

    /// The versions of this build's migrations the database has not applied,
    /// oldest first. Empty when the schema is current.
    ///
    /// Production migrations are a deliberate step (`tangible migrate`), so a
    /// server whose schema is behind would otherwise start, report healthy
    /// and fail every request; this is how it can say what is wrong instead.
    ///
    /// # Errors
    ///
    /// [`DbError::Query`] on a database failure.
    pub async fn pending_migrations(&self) -> Result<Vec<i64>, DbError> {
        let table: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations')::text")
                .fetch_one(&self.pool)
                .await
                .map_err(DbError::Query)?;
        let applied: Vec<i64> = if table.is_some() {
            sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success")
                .fetch_all(&self.pool)
                .await
                .map_err(DbError::Query)?
        } else {
            Vec::new()
        };
        Ok(MIGRATOR
            .iter()
            .map(|migration| migration.version)
            .filter(|version| !applied.contains(version))
            .collect())
    }
}

pub use repositories::{ClaimOutcome, ClaimedJob, IncomingEvent};
