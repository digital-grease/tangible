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

use clap::{Args, Subcommand, ValueEnum};
use tangible_domain::SecretString;

/// Which storage backend holds canonical bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum StorageKind {
    /// Content-addressed store on a local filesystem.
    Filesystem,
}

/// Which engine makes derivatives, such as a CHD of a disc image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum DerivationEngineKind {
    /// None: derivatives cannot be asked for.
    None,
    /// Simulated engine: writes a small placeholder, runs no tool. For tests
    /// and development.
    Fake,
    /// chdman, from MAME: CHDs of CD images, checked by extracting them
    /// again and comparing every track with the original.
    Chdman,
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
    ///
    /// It carries the database password, so its value is hidden from
    /// `--help` and never prints.
    #[arg(
        long,
        env = "TANGIBLE_DATABASE_URL",
        hide_env_values = true,
        default_value = "postgres://tangible:tangible@localhost:5432/tangible"
    )]
    pub database_url: SecretString,

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

    /// The release tag the image was built from. Set by the image, not by
    /// an operator; issue reports and diagnostics name it.
    #[arg(long, env = "TANGIBLE_BUILD_VERSION", hide = true)]
    pub build_version: Option<String>,

    /// The commit the image was built from. Set by the image.
    #[arg(long, env = "TANGIBLE_BUILD_REVISION", hide = true)]
    pub build_revision: Option<String>,
}

/// Settings for the HTTP server.
// Each flag is a separate setting an administrator turns on or off by
// environment variable; they are read one by one, not as one state.
#[allow(clippy::struct_excessive_bools)]
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

    /// Watched roots Tangible may delete in, as `id,id`: a SABnzbd or NZBGet
    /// download folder whose completed files are deleted once imported, for
    /// an integration set to remove after import (ADR 0017).
    ///
    /// Empty by default: every watched root is only read. Each id must be in
    /// `TANGIBLE_WATCH_ROOTS`, and its folder mounted read-write.
    #[arg(long, env = "TANGIBLE_REMOVABLE_ROOTS", default_value = "")]
    pub removable_roots: String,

    /// Largest upload accepted, in bytes.
    ///
    /// Defaults to 64 GiB, which covers a dual-layer Blu-ray with room to
    /// spare. Enforced on the bytes as they arrive rather than on a header.
    #[arg(long, env = "TANGIBLE_MAX_UPLOAD_BYTES", default_value_t = 64 * 1024 * 1024 * 1024)]
    pub max_upload_bytes: u64,

    /// Directory holding the built web UI, served from the same origin as
    /// the API.
    ///
    /// Unset by default, which serves the API alone: in development the UI
    /// runs from its own dev server. The container image sets it. A path that
    /// is set but holds no build stops the server rather than starting one
    /// that silently has no UI.
    #[arg(long, env = "TANGIBLE_WEB_ROOT")]
    pub web_root: Option<PathBuf>,

    /// RomM's `roms` directory, which game exports are written into as
    /// `{platform}/{game}/`.
    ///
    /// Unset by default, and then nothing is exported. Tangible writes only
    /// folders it marks as its own and never touches anything else in it.
    #[arg(long, env = "TANGIBLE_ROMM_EXPORT_ROOT")]
    pub romm_export_root: Option<PathBuf>,

    /// Engine that makes derivatives. `chdman` by default, which the image
    /// carries; nothing is derived until somebody asks for a derivative of a
    /// particular artifact. A server where chdman cannot be run says so at
    /// start and carries on without derivations.
    #[arg(
        long,
        env = "TANGIBLE_DERIVATION_ENGINE",
        value_enum,
        default_value_t = DerivationEngineKind::Chdman
    )]
    pub derivation_engine: DerivationEngineKind,

    /// The issue form operators' reports open, such as a GitHub repository's
    /// `…/issues/new`.
    ///
    /// The server never sends a report: it prepares scrubbed text, and the
    /// operator reads it and opens the form from their own browser. Set it
    /// empty to stop offering reports, or point it at your own tracker if
    /// you run a modified Tangible. Must be `https://`, with no query.
    #[arg(
        long,
        env = "TANGIBLE_REPORT_ISSUES_URL",
        default_value = tangible_api::report::DEFAULT_ISSUES_URL
    )]
    pub report_issues_url: String,

    /// Replace IP addresses and private host names (`.lan`, `.local` and
    /// the like) in issue reports, as well as credentials, paths, serial
    /// numbers and library file names, which are always replaced.
    #[arg(
        long,
        env = "TANGIBLE_REPORT_SCRUB_ADDRESSES",
        default_value_t = true,
        action = clap::ArgAction::Set
    )]
    pub report_scrub_addresses: bool,

    /// Let operators ask a rip to decrypt an encrypted disc (ADR 0013), on
    /// workers whose image was built to. Off unless set. Whether ripping a
    /// disc is lawful where you are is yours to determine.
    #[arg(
        long,
        env = "TANGIBLE_ALLOW_DECRYPTION",
        default_value_t = false,
        action = clap::ArgAction::Set
    )]
    pub allow_decryption: bool,

    /// Let operators import a file from a URL the server fetches (ADR 0016).
    /// Off unless set. Fetches reach only public addresses over https,
    /// unless the settings below allow more.
    #[arg(
        long,
        env = "TANGIBLE_URL_IMPORTS",
        default_value_t = false,
        action = clap::ArgAction::Set
    )]
    pub url_imports: bool,

    /// Let URL imports use plain http as well as https.
    #[arg(
        long,
        env = "TANGIBLE_URL_IMPORTS_ALLOW_HTTP",
        default_value_t = false,
        action = clap::ArgAction::Set
    )]
    pub url_imports_allow_http: bool,

    /// Address blocks outbound fetches may reach although they are private or
    /// reserved, comma-separated, such as `192.168.1.0/24,fd00::/8`. Empty by
    /// default: only public addresses.
    #[arg(long, env = "TANGIBLE_OUTBOUND_ALLOW", default_value = "")]
    pub outbound_allow: String,

    /// A proxy every outbound fetch goes through, when the deployment needs
    /// one. The environment's `HTTP_PROXY` is never used. A proxy resolves
    /// names itself, so it must enforce what Tangible then cannot. A secret:
    /// its URL may carry a username and password.
    #[arg(long, env = "TANGIBLE_OUTBOUND_PROXY", hide_env_values = true)]
    pub outbound_proxy: Option<SecretString>,

    /// A file holding the key integration secrets are sealed with (ADR 0016):
    /// 32 random bytes, base64, as `tangible secrets generate` prints. Without
    /// one the server runs, and integrations cannot hold a password. The path
    /// is not secret; the file is.
    #[arg(long, env = "TANGIBLE_SECRET_KEY_FILE")]
    pub secret_key_file: Option<PathBuf>,

    /// Days after a full integrity scrub ends before the next is queued, up
    /// to 3650. 0, the default, never queues one: a scrub reads every byte
    /// in the library, a load on the disks an administrator decides on.
    #[arg(
        long,
        env = "TANGIBLE_SCRUB_INTERVAL_DAYS",
        default_value_t = 0,
        value_parser = clap::value_parser!(u32).range(0..=3650)
    )]
    pub scrub_interval_days: u32,

    /// Whether to compute every artifact's preservation hashes (CRC32, MD5
    /// and SHA-1 of its files and CD tracks) in the background, for matching
    /// against DATs. On by default; each artifact's stored bytes are read
    /// once, those already in the library included.
    #[arg(
        long,
        env = "TANGIBLE_PRESERVATION_HASHES",
        default_value_t = true,
        action = clap::ArgAction::Set
    )]
    pub preservation_hashes: bool,

    /// Address to serve Prometheus metrics on (`GET /metrics`), such as
    /// `0.0.0.0:9464`. Unset, the default, serves none. A listener of its
    /// own, so it can stay unpublished; it serves nothing but metrics.
    #[arg(long, env = "TANGIBLE_METRICS_BIND")]
    pub metrics_bind: Option<SocketAddr>,

    /// A file holding the token a scrape must send as
    /// `Authorization: Bearer <token>`. Unset, anyone who can reach the
    /// metrics listener may read it. The path is not secret; the file is.
    #[arg(long, env = "TANGIBLE_METRICS_TOKEN_FILE")]
    pub metrics_token_file: Option<PathBuf>,
}

/// Settings for `tangible scrub`.
#[derive(Debug, Clone, Args)]
pub struct ScrubConfig {
    /// Check one artifact, by id, rather than the whole library.
    #[arg(long)]
    pub artifact: Option<tangible_domain::ArtifactId>,

    /// Queue the check and return at once, for the server to read.
    #[arg(long)]
    pub no_wait: bool,

    /// Root of the content-addressed store.
    #[arg(
        long,
        env = "TANGIBLE_STORAGE_ROOT",
        default_value = ".dev-data/library"
    )]
    pub storage_root: PathBuf,
}

/// Settings for `tangible gc`.
#[derive(Debug, Clone, Args)]
pub struct GcConfig {
    /// Keep an unreferenced object younger than this many days.
    #[arg(
        long,
        default_value_t = tangible_api::gc::DEFAULT_GRACE_DAYS,
        value_parser = clap::value_parser!(u32).range(0..=3650)
    )]
    pub grace_days: u32,

    /// Name at most this many candidates and missing objects, largest
    /// first; all are counted. 0 names every one.
    #[arg(long, default_value_t = 20)]
    pub limit: usize,

    /// Print the plan as JSON, as `GET /api/v1/storage/gc-plan` answers it.
    #[arg(long)]
    pub json: bool,

    /// Root of the content-addressed store.
    #[arg(
        long,
        env = "TANGIBLE_STORAGE_ROOT",
        default_value = ".dev-data/library"
    )]
    pub storage_root: PathBuf,
}

/// `tangible secrets`: the key integration secrets are sealed with.
#[derive(Debug, Clone, Subcommand)]
pub enum SecretsCommand {
    /// Print a new random key, base64, to keep in the file
    /// `TANGIBLE_SECRET_KEY_FILE` names.
    Generate,
    /// Seal every stored integration secret again under the key in
    /// `TANGIBLE_SECRET_KEY_FILE`, opening each with the old key. All or
    /// nothing, and safe to run again.
    Rotate(RotateConfig),
}

/// Settings for `tangible secrets rotate`.
#[derive(Debug, Clone, Args)]
pub struct RotateConfig {
    /// The file holding the key the secrets are sealed with now.
    #[arg(long)]
    pub old_key_file: PathBuf,

    /// The file holding the key to seal them with from now on.
    #[arg(long, env = "TANGIBLE_SECRET_KEY_FILE")]
    pub key_file: PathBuf,
}

/// Settings for `tangible doctor`.
///
/// Plain `doctor` is the container healthcheck and stays fast: the database
/// answers and its schema is current. `--report` looks at everything
/// diagnostics covers, which is why the server's own settings are read too.
#[derive(Debug, Clone, Args)]
pub struct DoctorConfig {
    /// Print the full diagnostics report as Markdown, followed by a link to
    /// the issue form when reports are on. Exits non-zero when any check is
    /// a problem.
    #[arg(long)]
    pub report: bool,

    /// With `--report`, print the report as JSON instead.
    #[arg(long, requires = "report")]
    pub json: bool,

    #[command(flatten)]
    pub serve: ServeConfig,
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
    pub enrollment_token: Option<SecretString>,

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
        assert_eq!(parse().removable_roots, "");
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

    #[test]
    fn scrubs_are_never_scheduled_unless_asked_for() {
        // A scrub reads every byte; the load is an administrator's choice.
        assert_eq!(parse().scrub_interval_days, 0);
        let cli = TestCli::try_parse_from(["tangible", "--scrub-interval-days", "30"])
            .expect("a month parses");
        assert_eq!(cli.serve.scrub_interval_days, 30);
        for refused in ["-1", "3651", "weekly"] {
            assert!(
                TestCli::try_parse_from(["tangible", "--scrub-interval-days", refused]).is_err(),
                "{refused}"
            );
        }
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
