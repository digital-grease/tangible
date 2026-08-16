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
    }

    #[test]
    fn an_unknown_engine_is_rejected() {
        assert!(TestCli::try_parse_from(["tangible", "--burn-engine", "dd"]).is_err());
    }
}
