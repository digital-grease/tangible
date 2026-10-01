// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Runtime configuration.
//!
//! Every setting is reachable from a `TANGIBLE_*` environment variable, which
//! is what `deploy/.env.example` documents. Defaults are chosen so that
//! development needs no configuration at all, while anything with a security
//! consequence (the burn engine in particular) defaults to the safe value.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, ValueEnum};

/// Which storage backend holds canonical bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum StorageKind {
    /// Content-addressed store on a local filesystem.
    Filesystem,
}

/// Which burn engine executes write operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum BurnEngineKind {
    /// Simulated engine. Touches no hardware.
    Fake,
    /// `xorriso`, for ISO-oriented CD, DVD and Blu-ray workflows.
    Xorriso,
    /// `cdrdao`, for DAO CD layouts, audio, mixed mode and pregaps.
    Cdrdao,
    /// Both: xorriso for prepared images and cdrdao for discs described as
    /// tracks, each plan going to the one its shape needs. What a worker with
    /// a real drive should normally run, because one drive then serves both.
    Auto,
}

/// Settings shared by every subcommand.
#[derive(Debug, Clone, Args)]
pub struct CommonConfig {
    /// PostgreSQL connection URL.
    #[arg(
        long,
        env = "TANGIBLE_DATABASE_URL",
        default_value = "postgres://tangible:tangible@localhost:5432/tangible"
    )]
    pub database_url: String,

    /// Tracing filter directive.
    #[arg(
        long,
        env = "TANGIBLE_LOG",
        default_value = "tangible=info,tower_http=info"
    )]
    pub log: String,

    /// Emit logs as JSON rather than human-readable text.
    #[arg(long, env = "TANGIBLE_LOG_JSON", default_value_t = false)]
    pub log_json: bool,
}

/// Settings for the HTTP server.
#[derive(Debug, Clone, Args)]
pub struct ServeConfig {
    /// Address to bind.
    #[arg(long, env = "TANGIBLE_BIND", default_value = "0.0.0.0:8080")]
    pub bind: SocketAddr,

    /// Externally reachable base URL, used when generating absolute links.
    #[arg(
        long,
        env = "TANGIBLE_PUBLIC_URL",
        default_value = "http://localhost:8080"
    )]
    pub public_url: String,

    /// Storage backend for canonical bytes.
    #[arg(long, env = "TANGIBLE_STORAGE_KIND", value_enum, default_value_t = StorageKind::Filesystem)]
    pub storage_kind: StorageKind,

    /// Root of the content-addressed store.
    #[arg(
        long,
        env = "TANGIBLE_STORAGE_ROOT",
        default_value = ".dev-data/library"
    )]
    pub storage_root: PathBuf,

    /// Root of the import staging area.
    #[arg(
        long,
        env = "TANGIBLE_STAGING_ROOT",
        default_value = ".dev-data/staging"
    )]
    pub staging_root: PathBuf,

    /// Burn engine. Defaults to `fake`: a real engine is opt-in, so a
    /// misconfigured deployment cannot reach a drive by accident.
    #[arg(long, env = "TANGIBLE_BURN_ENGINE", value_enum, default_value_t = BurnEngineKind::Fake)]
    pub burn_engine: BurnEngineKind,

    /// Apply pending migrations during startup instead of failing when the
    /// schema is behind. Off by default: production migrations are an
    /// explicit, reviewable step.
    #[arg(long, env = "TANGIBLE_MIGRATE_ON_START", default_value_t = false)]
    pub migrate_on_start: bool,

    /// Directories imports may be taken from, as `id=path,id=path`.
    ///
    /// Empty by default, and that is the safe value: with no roots
    /// configured, no request can name a path on this host at all. The
    /// identifiers are what the API accepts; the paths never leave the
    /// server.
    #[arg(long, env = "TANGIBLE_WATCH_ROOTS", default_value = "")]
    pub watch_roots: String,

    /// Largest upload accepted, in bytes.
    ///
    /// Defaults to 64 GiB, which covers a dual-layer Blu-ray with room to
    /// spare. Enforced on the bytes as they arrive rather than on a header.
    #[arg(long, env = "TANGIBLE_MAX_UPLOAD_BYTES", default_value_t = 64 * 1024 * 1024 * 1024)]
    pub max_upload_bytes: u64,
}

/// Settings for a burn worker.
///
/// One worker process drives one optical drive. Everything here has a
/// `TANGIBLE_*` variable because a worker is normally a container with no
/// command line of its own.
#[derive(Debug, Clone, Args)]
pub struct WorkerConfig {
    /// Base URL of the Tangible server this worker reports to.
    #[arg(
        long,
        env = "TANGIBLE_SERVER_URL",
        default_value = "http://localhost:8080"
    )]
    pub server_url: String,

    /// Name this worker enrolls under, and the name an operator sees.
    #[arg(long, env = "TANGIBLE_WORKER_NAME", default_value = "burn-worker")]
    pub worker_name: String,

    /// One-use enrollment token.
    ///
    /// Needed only until this worker holds a credential; after that the token
    /// is spent and the variable can be removed. Never logged.
    #[arg(long, env = "TANGIBLE_ENROLLMENT_TOKEN", hide_env_values = true)]
    pub enrollment_token: Option<String>,

    /// Where the credential, recovery record and staging cache live.
    ///
    /// Must survive a restart. A worker that loses this cannot tell whether
    /// an interrupted attempt wrote a disc, which is the one question it must
    /// always be able to answer.
    #[arg(
        long,
        env = "TANGIBLE_WORKER_STATE_DIR",
        default_value = ".dev-data/worker"
    )]
    pub state_dir: PathBuf,

    /// The drive's block device as the worker sees it.
    ///
    /// In a container this is the in-container path, which the hardware
    /// Compose file fixes at `/dev/sr0` whatever the host calls the drive, so
    /// the same configuration works on every machine. It must be a name
    /// libburn enumerates as a drive: xorriso refuses any other path as "not
    /// MMC", which the first end-to-end run found with an alias.
    #[arg(long, env = "TANGIBLE_BLOCK_DEVICE", default_value = "/dev/sr0")]
    pub device_alias: PathBuf,

    /// Burn engine. Defaults to `fake`, so a misconfigured worker cannot
    /// reach a drive by accident.
    #[arg(long, env = "TANGIBLE_BURN_ENGINE", value_enum, default_value_t = BurnEngineKind::Fake)]
    pub burn_engine: BurnEngineKind,
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        serve: ServeConfig,
    }

    fn parse() -> ServeConfig {
        TestCli::try_parse_from(["tangible"])
            .expect("serve defaults must parse with no arguments")
            .serve
    }

    #[test]
    fn burn_engine_defaults_to_fake() {
        // No configuration path may reach real hardware without the operator
        // explicitly asking for it.
        assert_eq!(parse().burn_engine, BurnEngineKind::Fake);
    }

    #[test]
    fn migration_on_start_is_off_by_default() {
        // Production migrations are an explicit step.
        assert!(!parse().migrate_on_start);
    }

    #[test]
    fn a_real_engine_can_be_selected_explicitly() {
        let cli = TestCli::try_parse_from(["tangible", "--burn-engine", "xorriso"])
            .expect("explicit engine parses");
        assert_eq!(cli.serve.burn_engine, BurnEngineKind::Xorriso);
        let cli = TestCli::try_parse_from(["tangible", "--burn-engine", "auto"])
            .expect("auto is a burn engine");
        assert_eq!(cli.serve.burn_engine, BurnEngineKind::Auto);
    }

    #[test]
    fn no_watched_root_is_configured_by_default() {
        // The safe value. With none configured, no request can name a path on
        // this host at all.
        assert_eq!(parse().watch_roots, "");
    }

    #[test]
    fn the_upload_limit_covers_a_dual_layer_blu_ray() {
        // 50 GB of disc, plus room for an image that is not perfectly packed.
        assert!(parse().max_upload_bytes >= 50_000_000_000);
    }

    #[test]
    fn an_unknown_engine_is_rejected() {
        assert!(TestCli::try_parse_from(["tangible", "--burn-engine", "dd"]).is_err());
    }

    #[derive(Parser)]
    struct WorkerCli {
        #[command(flatten)]
        worker: WorkerConfig,
    }

    fn worker() -> WorkerConfig {
        WorkerCli::try_parse_from(["tangible"])
            .expect("worker defaults must parse with no arguments")
            .worker
    }

    #[test]
    fn a_worker_defaults_to_the_fake_engine_too() {
        // The same rule as the server: reaching hardware is opt-in.
        assert_eq!(worker().burn_engine, BurnEngineKind::Fake);
    }

    #[test]
    fn a_worker_needs_no_enrollment_token_once_it_has_a_credential() {
        assert!(worker().enrollment_token.is_none());
    }

    #[test]
    fn a_worker_addresses_its_drive_by_alias_rather_than_host_path() {
        // Drive identity is worker plus configured alias. A host device node
        // is not stable across reboots, let alone across machines.
        assert_eq!(worker().device_alias, PathBuf::from("/dev/sr0"));
    }
}
