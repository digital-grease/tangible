// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The `tangible` executable: composition root and subcommands.
//!
//! This is the only crate that uses `anyhow`. Library crates return typed
//! errors via `thiserror`; the binary is where they become operator-facing
//! messages.

mod config;
mod telemetry;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tangible_api::ApiState;
use tangible_db::{Database, DbConfig};

use crate::config::{CommonConfig, ServeConfig};

/// Self-hosted disc-image preservation, management, and burning.
#[derive(Debug, Parser)]
#[command(name = "tangible", version, about, long_about = None)]
struct Cli {
    #[command(flatten)]
    common: CommonConfig,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the HTTP API and web server.
    Serve(Box<ServeConfig>),
    /// Run a burn worker bound to one optical drive.
    BurnWorker,
    /// Check the environment and report configuration problems.
    Doctor,
    /// Inspect and validate artifact manifests.
    Manifest,
    /// Apply pending database migrations.
    Migrate,
    /// Write the generated OpenAPI document to stdout.
    Openapi,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    telemetry::init(&cli.common)?;

    match cli.command {
        Command::Serve(serve) => serve_command(&cli.common, &serve).await,
        Command::Migrate => migrate_command(&cli.common).await,
        Command::Openapi => openapi_command(),
        Command::Doctor => doctor_command(&cli.common).await,
        Command::BurnWorker => {
            anyhow::bail!("`burn-worker` is not implemented yet; it arrives with epic E5")
        }
        Command::Manifest => {
            anyhow::bail!("`manifest` is not implemented yet; it arrives with epic E3")
        }
    }
}

async fn serve_command(common: &CommonConfig, serve: &ServeConfig) -> Result<()> {
    // Lazy pool: the listener must come up even if PostgreSQL is still
    // starting, so that /readyz can report the problem instead of the
    // process crash-looping.
    let database = Database::connect_lazy(&DbConfig::new(&common.database_url))
        .context("configuring the database pool")?;

    if serve.migrate_on_start {
        tracing::info!("applying pending migrations before serving");
        database
            .migrate()
            .await
            .context("applying migrations at startup")?;
    }

    let state = ApiState::new(database);

    let listener = tokio::net::TcpListener::bind(serve.bind)
        .await
        .with_context(|| format!("binding {}", serve.bind))?;

    let bound = listener
        .local_addr()
        .context("reading the bound socket address")?;
    tracing::info!(
        address = %bound,
        public_url = %serve.public_url,
        burn_engine = ?serve.burn_engine,
        storage_kind = ?serve.storage_kind,
        "tangible server listening"
    );

    tangible_api::serve(listener, state, shutdown_signal())
        .await
        .context("serving HTTP")
}

async fn migrate_command(common: &CommonConfig) -> Result<()> {
    // Eager connection: there is nothing useful to do without a database.
    let database = Database::connect(&DbConfig::new(&common.database_url))
        .await
        .context("connecting to the database")?;
    database.migrate().await.context("applying migrations")?;
    tracing::info!("migrations applied");
    Ok(())
}

fn openapi_command() -> Result<()> {
    let document = tangible_api::openapi_json().context("generating the OpenAPI document")?;
    println!("{document}");
    Ok(())
}

async fn doctor_command(common: &CommonConfig) -> Result<()> {
    let mut problems = 0_u32;

    match Database::connect(&DbConfig::new(&common.database_url)).await {
        Ok(database) => match database.ping().await {
            Ok(()) => tracing::info!("database: reachable"),
            Err(error) => {
                problems += 1;
                tracing::error!(error = ?error, "database: connected but not answering queries");
            }
        },
        Err(error) => {
            problems += 1;
            tracing::error!(error = ?error, "database: unreachable");
        }
    }

    if problems == 0 {
        tracing::info!("doctor: no problems found");
        Ok(())
    } else {
        anyhow::bail!("doctor found {problems} problem(s)")
    }
}

/// Resolve when the process is asked to stop, so in-flight requests finish.
///
/// A burn in progress must never be interrupted by an ordinary redeploy; the
/// worker protocol handles that separately, but the API should still drain
/// cleanly.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(error = ?error, "failed to install the Ctrl+C handler");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(error) => {
                tracing::error!(error = ?error, "failed to install the SIGTERM handler");
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => tracing::info!("received interrupt, shutting down"),
        () = terminate => tracing::info!("received SIGTERM, shutting down"),
    }
}
