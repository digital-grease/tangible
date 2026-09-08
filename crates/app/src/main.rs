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
use tangible_api::{ApiState, ImportContext, ImportPipeline, ImportRunner, ImportRunnerSettings};
use tangible_db::{Database, DbConfig};
use tangible_storage::{FilesystemStore, ManifestStore, StagingManager, WatchRoots};

use crate::config::{BurnEngineKind, CommonConfig, ServeConfig, WorkerConfig};

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
    BurnWorker(Box<WorkerConfig>),
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
        Command::BurnWorker(worker) => burn_worker_command(&worker).await,
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

    // Open the library so the read API has something to serve. A storage
    // failure is fatal here rather than degraded: unlike the database, which
    // /readyz reports on and which may still be starting, an unopenable
    // library root is a configuration mistake the operator must see
    // immediately rather than discover from empty listings.
    let objects = FilesystemStore::open(&serve.storage_root)
        .await
        .with_context(|| {
            format!(
                "opening the library root at {}",
                serve.storage_root.display()
            )
        })?;
    let manifests = ManifestStore::open(objects.clone())
        .await
        .context("opening manifest storage")?;

    tracing::info!(
        storage_root = %serve.storage_root.display(),
        staging_root = %serve.staging_root.display(),
        "library opened"
    );

    let staging = StagingManager::open(&serve.staging_root)
        .await
        .with_context(|| {
            format!(
                "opening the staging root at {}",
                serve.staging_root.display()
            )
        })?;

    let roots = WatchRoots::parse(&serve.watch_roots);
    if roots.is_empty() {
        tracing::info!(
            "no watched roots configured; imports must be uploaded (set TANGIBLE_WATCH_ROOTS to add some)"
        );
    } else {
        tracing::info!(roots = ?roots.ids(), "watched roots configured");
    }

    let pipeline = ImportPipeline::new(staging, objects, manifests.clone());
    let state = ApiState::with_manifests(database.clone(), manifests).with_imports(ImportContext {
        pipeline: pipeline.clone(),
        roots: roots.clone(),
        max_upload_bytes: serve.max_upload_bytes,
    });

    // The import runner is a background task rather than a separate process:
    // one binary, one deployment, and the work is already leased in the
    // database so a second server can be added without changing anything.
    let runner = ImportRunner::new(
        database,
        pipeline,
        ImportRunnerSettings {
            roots,
            ..ImportRunnerSettings::default()
        },
    );
    let (stop_runner, runner_stopped) = tokio::sync::oneshot::channel();
    let runner = tokio::spawn(runner.run(async {
        let _ = runner_stopped.await;
    }));

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

    let served = tangible_api::serve(listener, state, shutdown_signal())
        .await
        .context("serving HTTP");

    // The import already running finishes before the process exits.
    // Abandoning it would leave staged bytes and a lease behind for no gain.
    let _ = stop_runner.send(());
    let _ = runner.await;

    served
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

/// Run one burn worker until it is asked to stop.
///
/// The worker owns a drive, so shutdown is not simply "exit": the runtime
/// finishes the step it is on. A burn already writing is never interrupted by
/// a redeploy, because stopping mid-write ruins the disc and the local
/// recovery record is what lets the next process reconcile rather than guess.
async fn burn_worker_command(worker: &WorkerConfig) -> Result<()> {
    use tangible_burn::runner::{WorkerRuntime, WorkerSettings};

    match worker.burn_engine {
        BurnEngineKind::Fake => {}
        // Refused rather than silently substituted. An operator who asked for
        // a real engine and got a simulation would believe a disc exists.
        other => anyhow::bail!(
            "the {other:?} engine is not implemented yet; it arrives with epic E6. \
             Remove TANGIBLE_BURN_ENGINE to run the hardware-free engine."
        ),
    }

    let settings = WorkerSettings {
        enrollment_token: worker.enrollment_token.clone(),
        device_alias: worker.device_alias.to_string_lossy().into_owned(),
        configured_name: worker.worker_name.clone(),
        ..WorkerSettings::new(
            worker.server_url.clone(),
            worker.worker_name.clone(),
            worker.state_dir.clone(),
        )
    };

    tracing::info!(
        server = %settings.server_url,
        worker = %settings.worker_name,
        alias = %settings.device_alias,
        state_dir = %settings.state_dir.display(),
        "starting the burn worker with the hardware-free engine"
    );

    // Simulated media live beside the worker's other state, so a restart
    // finds the same "disc" in the same "drive".
    let engine = tangible_burn::FakeEngine::new(settings.state_dir.join("media"));
    let mut runtime = WorkerRuntime::new(settings, engine).context("assembling the burn worker")?;

    runtime
        .run(shutdown_signal())
        .await
        .context("running the burn worker")
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
