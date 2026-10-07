// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The `tangible` executable: composition root and subcommands.
//!
//! This is the only crate that uses `anyhow`. Library crates return typed
//! errors via `thiserror`; the binary is where they become operator-facing
//! messages.

mod config;
mod telemetry;

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tangible_api::{
    ApiState, AuthSettings, DerivationPipeline, DerivationRunner, DerivationRunnerSettings,
    Derivations, ImportContext, ImportPipeline, ImportRunner, ImportRunnerSettings, RommExport,
    RommExporter, WebUi,
};
use tangible_db::{Database, DbConfig};
use tangible_storage::{FilesystemStore, ManifestStore, StagingManager, WatchRoots};

use crate::config::{
    BurnEngineKind, CommonConfig, DerivationEngineKind, ServeConfig, WorkerConfig,
};

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
    let database = Database::connect_lazy(&DbConfig::new(common.database_url.expose()))
        .context("configuring the database pool")?;

    if serve.migrate_on_start {
        tracing::info!("applying pending migrations before serving");
        database
            .migrate()
            .await
            .context("applying migrations at startup")?;
    } else {
        warn_if_schema_behind(&database).await;
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

    let pipeline = ImportPipeline::new(staging.clone(), objects.clone(), manifests.clone());
    let auth = AuthSettings::for_public_url(&serve.public_url);
    warn_if_plain_http(auth, &serve.public_url);
    warn_if_setup_needed(&database, &serve.public_url).await;

    let state = ApiState::with_manifests(database.clone(), manifests.clone())
        .with_imports(ImportContext {
            pipeline: pipeline.clone(),
            roots: roots.clone(),
            max_upload_bytes: serve.max_upload_bytes,
        })
        .with_auth(auth);
    let state = attach_web_ui(state, serve.web_root.as_deref()).await?;
    let (state, romm) = start_romm_export(
        state,
        &database,
        &manifests,
        serve.romm_export_root.as_deref(),
    )
    .await?;
    let (state, derivations) = start_derivations(
        state,
        &database,
        DerivationPipelineParts {
            staging,
            objects,
            manifests: manifests.clone(),
        },
        serve.derivation_engine,
    )
    .await;

    let imports = start_import_runner(database, pipeline, roots);

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

    // Work already running finishes before the process exits. Abandoning an
    // import would leave staged bytes and a lease behind for no gain.
    for (stop, task) in [Some(imports), romm, derivations].into_iter().flatten() {
        let _ = stop.send(());
        let _ = task.await;
    }

    served
}

/// Start the import runner.
///
/// A background task rather than a separate process: one binary, one
/// deployment, and the work is already leased in the database so a second
/// server can be added without changing anything.
fn start_import_runner(
    database: Database,
    pipeline: ImportPipeline,
    roots: WatchRoots,
) -> Stoppable {
    let runner = ImportRunner::new(
        database,
        pipeline,
        ImportRunnerSettings {
            roots,
            ..ImportRunnerSettings::default()
        },
    );
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(runner.run(async {
        let _ = stopped.await;
    }));
    (stop, task)
}

/// What a derivation pipeline is built from.
struct DerivationPipelineParts {
    staging: StagingManager,
    objects: FilesystemStore,
    manifests: ManifestStore,
}

/// Start the derivation runner when an engine is configured.
async fn start_derivations(
    state: ApiState,
    database: &Database,
    parts: DerivationPipelineParts,
    kind: DerivationEngineKind,
) -> (ApiState, Option<Stoppable>) {
    let engine: Arc<dyn tangible_image::derive::DerivationEngine> = match kind {
        DerivationEngineKind::None => {
            tracing::info!(
                "no derivation engine configured (TANGIBLE_DERIVATION_ENGINE); derivatives cannot be asked for"
            );
            return (state, None);
        }
        DerivationEngineKind::Fake => Arc::new(tangible_image::derive::FakeDeriver::default()),
        DerivationEngineKind::Chdman => {
            match tangible_image::chdman::Chdman::probe(tangible_image::chdman::CHDMAN).await {
                Ok(chdman) => Arc::new(chdman),
                Err(error) => {
                    // Not fatal: everything else works without derivatives,
                    // and the derivative route says why it refuses.
                    tracing::warn!(error = %error, "chdman cannot be run, so derivatives are disabled");
                    return (state, None);
                }
            }
        }
    };
    tracing::info!(
        tool = engine.tool_name(),
        version = %engine.tool_version(),
        "derivations enabled"
    );
    let derivations = Derivations::new(DerivationPipeline::new(
        parts.staging,
        parts.objects,
        parts.manifests,
        engine,
    ));
    let mut runner = DerivationRunner::new(
        database.clone(),
        derivations.clone(),
        DerivationRunnerSettings::default(),
    );
    if let Some(romm) = state.romm() {
        runner = runner.with_romm(romm.clone());
    }
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(runner.run(async {
        let _ = stopped.await;
    }));
    (state.with_derivations(derivations), Some((stop, task)))
}

/// Say loudly at start when the database schema is behind this build.
///
/// Not fatal: the database may still be starting, and the healthcheck
/// (`tangible doctor`) fails for as long as the schema is behind, which keeps
/// workers from starting against it. But a server that served quietly on an
/// empty schema would fail every request with nothing to say why.
async fn warn_if_schema_behind(database: &Database) {
    match database.pending_migrations().await {
        Ok(pending) if !pending.is_empty() => tracing::error!(
            pending = ?pending,
            "the database schema is behind this build by {} migration(s); nothing will work \
             until they are applied: run `tangible migrate` (with Compose: docker compose run \
             --rm server migrate)",
            pending.len()
        ),
        Ok(_) => {}
        Err(error) => tracing::debug!(error = ?error, "could not check the schema version yet"),
    }
}

/// Say at every start that a plain HTTP server sends passwords in the clear.
fn warn_if_plain_http(auth: AuthSettings, public_url: &str) {
    if !auth.secure_cookie {
        tracing::warn!(
            public_url = %public_url,
            "the public URL is plain HTTP, so session cookies are not marked Secure and \
             passwords cross the network unencrypted; use this only on a network you trust, \
             and put HTTPS in front of Tangible before exposing it further"
        );
    }
}

/// A background task and the means to stop it.
type Stoppable = (
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
);

/// Start exporting games to RomM, when an export root is configured.
async fn start_romm_export(
    state: ApiState,
    database: &Database,
    manifests: &ManifestStore,
    root: Option<&std::path::Path>,
) -> Result<(ApiState, Option<Stoppable>)> {
    let Some(root) = root else {
        tracing::info!(
            "no RomM export root configured (TANGIBLE_ROMM_EXPORT_ROOT); nothing exports"
        );
        return Ok((state, None));
    };
    if !tokio::fs::metadata(root)
        .await
        .is_ok_and(|metadata| metadata.is_dir())
    {
        // Not fatal: the exporter reports it against every export, where an
        // operator will see it, and recovers when the mount appears.
        tracing::warn!(root = %root.display(), "the RomM export root is not a directory");
    }
    // Configuration names the layout, never the host path.
    let integration = tangible_db::romm::ensure_integration(
        database.pool(),
        &serde_json::json!({ "kind": "romm_filesystem", "layout": "{platform}/{game}" }),
    )
    .await
    .context("recording the RomM export integration")?;

    let export = RommExport::new(root.to_path_buf());
    let exporter = RommExporter::new(
        export.clone(),
        database.clone(),
        manifests.clone(),
        integration,
    );
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(exporter.run(async {
        let _ = stopped.await;
    }));
    tracing::info!(root = %root.display(), "exporting games to RomM");
    Ok((state.with_romm(export), Some((stop, task))))
}

/// Serve the built web UI beside the API, when one is configured.
async fn attach_web_ui(state: ApiState, root: Option<&std::path::Path>) -> Result<ApiState> {
    let Some(root) = root else {
        tracing::info!("no web UI configured (TANGIBLE_WEB_ROOT); serving the API alone");
        return Ok(state);
    };
    let web = WebUi::open(root)
        .await
        .with_context(|| format!("opening the web UI at {}", root.display()))?;
    tracing::info!(web_root = %root.display(), "serving the web UI");
    Ok(state.with_web(web))
}

/// Say loudly that the server has no accounts yet.
///
/// Until the first administrator exists, whoever reaches the setup page
/// first becomes one. That is the intended way in, and also the reason to do
/// it promptly on a fresh server.
async fn warn_if_setup_needed(database: &Database, public_url: &str) {
    match tangible_db::accounts::count_users(database.pool()).await {
        Ok(0) => tracing::warn!(
            setup = %format!("{}/api/v1/setup", public_url.trim_end_matches('/')),
            "no accounts exist yet: whoever completes setup first becomes the administrator, \
             through the web UI's setup page or a POST to the setup route, so do it now"
        ),
        Ok(_) => {}
        Err(error) => tracing::warn!(error = ?error, "could not count accounts at startup"),
    }
}

async fn migrate_command(common: &CommonConfig) -> Result<()> {
    // Eager connection: there is nothing useful to do without a database.
    let database = Database::connect(&DbConfig::new(common.database_url.expose()))
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

    match Database::connect(&DbConfig::new(common.database_url.expose())).await {
        Ok(database) => match database.ping().await {
            Ok(()) => {
                tracing::info!("database: reachable");
                match database.pending_migrations().await {
                    Ok(pending) if pending.is_empty() => tracing::info!("database: schema current"),
                    Ok(pending) => {
                        problems += 1;
                        tracing::error!(
                            pending = ?pending,
                            "database: {} migration(s) not applied; run `tangible migrate` \
                             (with Compose: docker compose run --rm server migrate)",
                            pending.len()
                        );
                    }
                    Err(error) => {
                        problems += 1;
                        tracing::error!(error = ?error, "database: could not read the schema version");
                    }
                }
            }
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
    use tangible_burn::runner::WorkerSettings;
    use tangible_burn::{CdrdaoEngine, CombinedEngine, FakeEngine, XorrisoEngine};

    // Absolute, because the engines refuse to hand a tool a relative path
    // that could be read as an option, and because the working directory of a
    // container is not something to depend on.
    let state_dir = std::path::absolute(&worker.state_dir)
        .with_context(|| format!("resolving {}", worker.state_dir.display()))?;

    let settings = WorkerSettings {
        enrollment_token: worker.enrollment_token.clone(),
        device_alias: worker.device_alias.to_string_lossy().into_owned(),
        configured_name: worker.worker_name.clone(),
        ..WorkerSettings::new(
            worker.server_url.clone(),
            worker.worker_name.clone(),
            state_dir.clone(),
        )
    };

    tracing::info!(
        server = %settings.server_url,
        worker = %settings.worker_name,
        alias = %settings.device_alias,
        state_dir = %settings.state_dir.display(),
        engine = ?worker.burn_engine,
        "starting the burn worker"
    );

    // Tables of contents and read-back chunks live with the worker's other
    // state, on the volume that survives a restart, rather than in a
    // container's temporary directory.
    let xorriso = || async {
        let mut engine = XorrisoEngine::new().with_scratch_dir(state_dir.join("readback"));
        tokio::fs::create_dir_all(state_dir.join("readback"))
            .await
            .context("creating the read-back directory")?;
        let version = engine
            .probe_version()
            .await
            .context("xorriso did not answer; the worker image may be broken")?;
        tracing::info!(version = %version.version, "xorriso is ready");
        anyhow::Ok(engine)
    };
    let cdrdao = || async {
        let mut engine = CdrdaoEngine::new(state_dir.join("toc"));
        let version = engine
            .probe_version()
            .await
            .context("cdrdao did not answer; the worker image may be broken")?;
        tracing::info!(%version, "cdrdao is ready");
        // Said at startup because it is a property of every disc this worker
        // writes from a track layout, not of any one of them.
        tracing::info!(
            "discs written from a track layout are read back track by track: data tracks \
             in MODE1/2048 are compared, and audio and raw data tracks are checked for \
             length and readability, which records the disc as partially verified"
        );
        anyhow::Ok(engine)
    };

    match worker.burn_engine {
        BurnEngineKind::Fake => {
            // Simulated media live beside the worker's other state, so a
            // restart finds the same "disc" in the same "drive".
            run_worker(settings, FakeEngine::new(state_dir.join("media"))).await
        }
        BurnEngineKind::Xorriso => run_worker(settings, xorriso().await?).await,
        BurnEngineKind::Cdrdao => run_worker(settings, cdrdao().await?).await,
        BurnEngineKind::Auto => {
            let engine = CombinedEngine::new(xorriso().await?, cdrdao().await?);
            run_worker(settings, engine).await
        }
    }
}

/// Run a worker with the engine it was given until it is asked to stop.
async fn run_worker<E: tangible_burn::BurnEngine + 'static>(
    settings: tangible_burn::runner::WorkerSettings,
    engine: E,
) -> Result<()> {
    let mut runtime = tangible_burn::runner::WorkerRuntime::new(settings, engine)
        .context("assembling the burn worker")?;
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

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// Variables whose value is a secret: a password inside a URL, a token.
    /// Their values must never appear in `--help`, which shows the live value
    /// of every variable that is set unless told not to.
    const SECRET_ENV: &[&str] = &["TANGIBLE_DATABASE_URL", "TANGIBLE_ENROLLMENT_TOKEN"];

    /// Every other variable, each one looked at and found not to be secret.
    const PUBLIC_ENV: &[&str] = &[
        "TANGIBLE_BIND",
        "TANGIBLE_BLOCK_DEVICE",
        "TANGIBLE_BURN_ENGINE",
        "TANGIBLE_DERIVATION_ENGINE",
        "TANGIBLE_LOG",
        "TANGIBLE_LOG_JSON",
        "TANGIBLE_MAX_UPLOAD_BYTES",
        "TANGIBLE_MIGRATE_ON_START",
        "TANGIBLE_PUBLIC_URL",
        "TANGIBLE_ROMM_EXPORT_ROOT",
        "TANGIBLE_SERVER_URL",
        "TANGIBLE_STAGING_ROOT",
        "TANGIBLE_STORAGE_KIND",
        "TANGIBLE_STORAGE_ROOT",
        "TANGIBLE_WATCH_ROOTS",
        "TANGIBLE_WEB_ROOT",
        "TANGIBLE_WORKER_NAME",
        "TANGIBLE_WORKER_STATE_DIR",
    ];

    fn every_arg(command: &clap::Command, found: &mut Vec<(String, bool)>) {
        for arg in command.get_arguments() {
            if let Some(env) = arg.get_env() {
                found.push((
                    env.to_string_lossy().into_owned(),
                    arg.is_hide_env_values_set(),
                ));
            }
        }
        for sub in command.get_subcommands() {
            every_arg(sub, found);
        }
    }

    #[test]
    fn no_secret_variable_shows_its_value_in_help() {
        let mut found = Vec::new();
        every_arg(&Cli::command(), &mut found);
        assert!(!found.is_empty());
        for (env, hidden) in found {
            if SECRET_ENV.contains(&env.as_str()) {
                assert!(hidden, "{env} holds a secret, so its value must be hidden");
            } else {
                assert!(
                    PUBLIC_ENV.contains(&env.as_str()),
                    "{env} is new: decide whether it holds a secret and add it to \
                     SECRET_ENV (with hide_env_values) or PUBLIC_ENV"
                );
            }
        }
    }

    #[test]
    fn a_parsed_configuration_never_prints_the_database_password() {
        let cli = Cli::try_parse_from([
            "tangible",
            "--database-url",
            "postgres://user:placeholder-password@db/tangible",
            "doctor",
        ])
        .expect("parse");
        let shown = format!("{:?}", cli.common);
        assert!(!shown.contains("placeholder-password"), "{shown}");
        assert_eq!(
            cli.common.database_url.expose(),
            "postgres://user:placeholder-password@db/tangible"
        );
    }

    #[test]
    fn a_worker_configuration_never_prints_its_enrollment_token() {
        let cli = Cli::try_parse_from([
            "tangible",
            "burn-worker",
            "--enrollment-token",
            "tgw_enroll_placeholder",
        ])
        .expect("parse");
        let shown = format!("{:?}", cli.command);
        assert!(!shown.contains("tgw_enroll_placeholder"), "{shown}");
    }
}
