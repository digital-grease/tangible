// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Diagnostics: the state of a deployment as a checklist, for the operator
//! and for whoever helps them.
//!
//! The same report comes from `tangible doctor --report` and from the
//! administrators' Diagnostics page. It is written to be pasted into an
//! issue, so it carries no names from the library, no file names, no paths
//! and no failure details: workers and drives are numbered, failures are
//! codes with times, and storage is described by what it can do rather than
//! where it is.
//!
//! Collecting ([`collect`]) and judging ([`assess`]) are separate, so every
//! judgement is a pure function of what was found and can be tested without
//! a database or a disk.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;
use tangible_db::Database;
use tangible_db::diagnostics::{Failure, WorkerDiagnostic};
use tangible_db::erasures::DriveRecord;
use tangible_db::integrity::IntegritySummary;
use tangible_db::romm_api::{ScanRecord, ScanState};
use tangible_domain::IntegrityCheckState;
use tangible_storage::object_key::{INCOMING_DIR, OBJECTS_DIR};
use tangible_storage::space::{DirectoryCheck, Space, check_directory};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use utoipa::ToSchema;

use tangible_domain::report::ScrubContext;

use crate::error_report::{BUG_TEMPLATE, RELEASE_IMAGE};
use crate::report::{BuildIdentity, IssueReport, ReportSettings, Writer};

/// How long one database query may take before it is reported as not
/// answering.
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// How many recent failures are read.
pub const RECENT_FAILURES: i64 = 20;

/// Heartbeats a worker may miss before it counts as not heard from. Workers
/// send one every 20 seconds.
const SILENT_AFTER_SECONDS: i64 = 60;

/// Below this much free space a storage directory is a problem.
const PROBLEM_FREE_BYTES: u64 = 1024 * 1024 * 1024;

/// Below this much free space, or [`WARN_FREE_PERCENT`], it is a warning.
const WARN_FREE_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Below this share of the filesystem free, a warning.
const WARN_FREE_PERCENT: u64 = 10;

/// Most workers, and most drives, listed one by one. The rest are counted
/// in one line, so a report stays short enough to paste.
pub const MAX_LISTED: usize = 20;

/// The burn worker protocol this server speaks.
const PROTOCOL_VERSION: &str = "1alpha1";

/// How a check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// Nothing to do.
    Ok,
    /// Worth a look; things still work.
    Warn,
    /// Something does not work.
    Problem,
}

impl CheckStatus {
    /// The word a reader sees: status is never shown by colour alone.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Warn => "Warning",
            Self::Problem => "Problem",
        }
    }
}

/// One line of the checklist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct DiagnosticCheck {
    /// What was checked, such as `Schema` or `worker 1`.
    pub name: String,
    /// How it came out.
    pub status: CheckStatus,
    /// What was found, in words.
    pub detail: String,
}

/// A group of checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct DiagnosticSection {
    /// Such as `Database` or `Storage`.
    pub title: String,
    /// Its checks, in a stable order.
    pub checks: Vec<DiagnosticCheck>,
}

/// The whole report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Diagnostics {
    /// When it was collected, RFC 3339.
    pub generated_at: String,
    /// The build, as reports name it: `v0.1.0 (76fa515)`.
    pub build: String,
    /// The worst status of any check.
    pub status: CheckStatus,
    /// The checks, by section.
    pub sections: Vec<DiagnosticSection>,
}

/// Which derivation engine is configured, and whether it could be run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DerivationStatus {
    /// None configured.
    Off,
    /// The fake engine, which makes placeholders.
    Fake,
    /// A real tool, found and run.
    Tool {
        /// Such as `chdman`.
        name: String,
        /// What it says its version is.
        version: String,
    },
    /// A real tool was configured and could not be run.
    Unavailable {
        /// Such as `chdman`.
        name: String,
        /// Why, without paths.
        reason: String,
    },
}

/// The server's settings that diagnostics reports on. The paths are used to
/// look at the directories and never appear in the report.
#[derive(Debug, Clone)]
pub struct DiagnosticsSettings {
    /// The build.
    pub build: BuildIdentity,
    /// The library root.
    pub storage_root: PathBuf,
    /// The staging root.
    pub staging_root: PathBuf,
    /// RomM's `roms` directory, when exports are configured.
    pub romm_export_root: Option<PathBuf>,
    /// The derivation engine.
    pub derivation: DerivationStatus,
    /// Whether the public URL is `https://`.
    pub public_url_https: bool,
    /// Whether migrations are applied at start.
    pub migrate_on_start: bool,
    /// Issue report settings.
    pub reports: ReportSettings,
    /// Whether rips may decrypt (`TANGIBLE_ALLOW_DECRYPTION`).
    pub decryption_allowed: bool,
    /// Whether operators may import from a URL (`TANGIBLE_URL_IMPORTS`), and
    /// whether plain http too.
    pub url_imports: Option<UrlImportSetting>,
    /// The id of the key integration secrets are sealed with
    /// (`TANGIBLE_SECRET_KEY_FILE`), when one is loaded. Not secret: a
    /// digest's first bytes, used to tell keys apart.
    pub secret_key_id: Option<String>,
    /// Days after a full scrub ends before the next is queued
    /// (`TANGIBLE_SCRUB_INTERVAL_DAYS`); 0 is never.
    pub scrub_interval_days: u32,
    /// The watched roots Tangible may delete in
    /// (`TANGIBLE_REMOVABLE_ROOTS`, ADR 0017), to see that each is writable.
    pub removable_roots: Vec<PathBuf>,
}

/// How URL imports are set, when they are on (ADR 0016).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UrlImportSetting {
    /// Plain http allowed as well as https.
    pub allow_http: bool,
    /// How many address blocks an administrator allowed although private.
    pub allowed_blocks: usize,
    /// Whether fetches go through a proxy.
    pub proxy: bool,
}

/// What the database said, or why it said nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer<T> {
    /// It answered.
    Found(T),
    /// It could not be asked, and why.
    Unavailable(String),
}

/// Everything [`collect`] found, before any judgement.
#[derive(Debug, Clone)]
pub struct Facts {
    /// Whether the database answers.
    pub database: Answer<()>,
    /// Migrations this build has that the database has not applied.
    pub pending_migrations: Answer<Vec<i64>>,
    /// The library root.
    pub library: DirectoryCheck,
    /// The staging root.
    pub staging: DirectoryCheck,
    /// RomM's directory, when configured.
    pub romm: Option<DirectoryCheck>,
    /// Every worker.
    pub workers: Answer<Vec<WorkerDiagnostic>>,
    /// Every drive.
    pub drives: Answer<Vec<DriveRecord>>,
    /// Recent failures, newest first.
    pub failures: Answer<Vec<Failure>>,
    /// Stored integration secrets, counted by the id of the key that sealed
    /// them.
    pub sealed_secrets: Answer<Vec<(String, i64)>>,
    /// What integrity checks have found, summed up.
    pub integrity: Answer<IntegritySummary>,
    /// RomM's API, when an administrator configured it.
    pub romm_api: Answer<Option<RommApiFacts>>,
    /// Each watched root Tangible may delete in.
    pub removable_roots: Vec<DirectoryCheck>,
}

/// How RomM's API stands, from the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RommApiFacts {
    /// Whether it is in use.
    pub enabled: bool,
    /// `unconfigured`, `ok` or `failing`.
    pub status: String,
    /// Why it last failed, as a stable code.
    pub last_error_code: Option<String>,
    /// RomM's version, as last seen.
    pub version: Option<String>,
    /// Platform folders owed a scan.
    pub scans_owed: usize,
    /// The last scan asked for.
    pub last_scan: Option<ScanRecord>,
}

async fn romm_api_facts(database: &Database) -> Result<Option<RommApiFacts>, tangible_db::DbError> {
    let pool = database.pool();
    let Some(record) = tangible_db::integrations::romm_api_integration(pool).await? else {
        return Ok(None);
    };
    Ok(Some(RommApiFacts {
        enabled: record.enabled,
        status: record.status,
        last_error_code: record.last_error_code,
        version: record.client_version,
        scans_owed: tangible_db::romm_api::owed_scans(pool).await?.len(),
        last_scan: tangible_db::romm_api::last_scan(pool, None).await?,
    }))
}

/// Look at everything and judge it, now.
pub async fn run(database: &Database, settings: &DiagnosticsSettings) -> Diagnostics {
    let facts = collect(database, settings).await;
    assess(settings, &facts, OffsetDateTime::now_utc())
}

/// Look at everything diagnostics reports on.
pub async fn collect(database: &Database, settings: &DiagnosticsSettings) -> Facts {
    let pool = database.pool();
    let database_answer = bounded(database.ping()).await.map(|()| ());
    // Once the database is known to be down, the queries that follow would
    // each wait out the timeout to say the same thing.
    let (pending_migrations, workers, drives, failures, sealed_secrets, integrity, romm_api) =
        if let Answer::Unavailable(reason) = &database_answer {
            (
                skipped(reason),
                skipped(reason),
                skipped(reason),
                skipped(reason),
                skipped(reason),
                skipped(reason),
                skipped(reason),
            )
        } else {
            (
                bounded(database.pending_migrations()).await,
                bounded(tangible_db::diagnostics::list_workers(pool)).await,
                bounded(tangible_db::erasures::list_drives(pool)).await,
                bounded(tangible_db::diagnostics::recent_failures(
                    pool,
                    RECENT_FAILURES,
                ))
                .await,
                bounded(tangible_db::integrations::secret_counts_by_key(pool)).await,
                bounded(tangible_db::integrity::integrity_summary(pool)).await,
                bounded(romm_api_facts(database)).await,
            )
        };
    let incoming = settings.storage_root.join(OBJECTS_DIR).join(INCOMING_DIR);
    let mut removable_roots = Vec::with_capacity(settings.removable_roots.len());
    for root in &settings.removable_roots {
        removable_roots.push(check_directory(root, None).await);
    }
    Facts {
        database: database_answer,
        pending_migrations,
        library: check_directory(&settings.storage_root, Some(&incoming)).await,
        staging: check_directory(&settings.staging_root, None).await,
        romm: match &settings.romm_export_root {
            Some(root) => Some(check_directory(root, None).await),
            None => None,
        },
        workers,
        drives,
        failures,
        sealed_secrets,
        integrity,
        romm_api,
        removable_roots,
    }
}

/// What a check says when the database was already known to be down.
fn skipped<T>(reason: &str) -> Answer<T> {
    Answer::Unavailable(format!("not checked: {reason}"))
}

/// A database call, with a time limit, as an [`Answer`]. The error's own
/// text never reaches the report: it can carry a connection string.
async fn bounded<T, E: std::fmt::Debug>(
    query: impl std::future::Future<Output = Result<T, E>>,
) -> Answer<T> {
    match tokio::time::timeout(QUERY_TIMEOUT, query).await {
        Ok(Ok(value)) => Answer::Found(value),
        Ok(Err(error)) => {
            tracing::warn!(error = ?error, "diagnostics: a database query failed");
            Answer::Unavailable("the database did not answer; see the server log".to_owned())
        }
        Err(_) => Answer::Unavailable(format!(
            "the database did not answer within {} seconds",
            QUERY_TIMEOUT.as_secs()
        )),
    }
}

impl<T> Answer<T> {
    fn map<U>(self, f: impl FnOnce(T) -> U) -> Answer<U> {
        match self {
            Self::Found(value) => Answer::Found(f(value)),
            Self::Unavailable(reason) => Answer::Unavailable(reason),
        }
    }
}

/// Judge what was found.
#[must_use]
pub fn assess(settings: &DiagnosticsSettings, facts: &Facts, now: OffsetDateTime) -> Diagnostics {
    let sections = vec![
        DiagnosticSection {
            title: "Database".to_owned(),
            checks: database_checks(facts),
        },
        DiagnosticSection {
            title: "Storage".to_owned(),
            checks: vec![
                directory_check("Library", &facts.library),
                directory_check("Staging", &facts.staging),
                integrity_check(settings.scrub_interval_days, &facts.integrity, now),
                removable_roots_check(&facts.removable_roots),
            ],
        },
        DiagnosticSection {
            title: "Tools".to_owned(),
            checks: tool_checks(settings, facts),
        },
        DiagnosticSection {
            title: "Workers".to_owned(),
            checks: worker_checks(&settings.build, &facts.workers, now),
        },
        DiagnosticSection {
            title: "Drives".to_owned(),
            checks: drive_checks(&facts.workers, &facts.drives),
        },
        DiagnosticSection {
            title: "Recent failures".to_owned(),
            checks: failure_checks(&facts.failures),
        },
        DiagnosticSection {
            title: "Settings".to_owned(),
            checks: {
                let mut checks = setting_checks(settings);
                checks.push(secret_check(
                    settings.secret_key_id.as_deref(),
                    &facts.sealed_secrets,
                ));
                checks
            },
        },
    ];
    let status = sections
        .iter()
        .flat_map(|section| &section.checks)
        .map(|check| check.status)
        .max()
        .unwrap_or(CheckStatus::Ok);
    Diagnostics {
        generated_at: timestamp(now),
        build: settings.build.describe(),
        status,
        sections,
    }
}

/// RFC 3339 to the second: finer says nothing to a reader.
fn timestamp(at: OffsetDateTime) -> String {
    at.replace_nanosecond(0)
        .unwrap_or(at)
        .format(&Rfc3339)
        .unwrap_or_default()
}

fn check(
    name: impl Into<String>,
    status: CheckStatus,
    detail: impl Into<String>,
) -> DiagnosticCheck {
    DiagnosticCheck {
        name: one_line(&name.into()),
        status,
        detail: one_line(&detail.into()),
    }
}

/// Text on one line. Some of it comes from workers, and a line break in a
/// drive's model name must not be able to add lines to the report.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn database_checks(facts: &Facts) -> Vec<DiagnosticCheck> {
    let reachable = match &facts.database {
        Answer::Found(()) => check("Connection", CheckStatus::Ok, "reachable"),
        Answer::Unavailable(reason) => check("Connection", CheckStatus::Problem, reason.clone()),
    };
    let schema = match &facts.pending_migrations {
        Answer::Found(pending) if pending.is_empty() => {
            check("Schema", CheckStatus::Ok, "every migration applied")
        }
        Answer::Found(pending) => check(
            "Schema",
            CheckStatus::Problem,
            format!(
                "{} migration(s) not applied ({}); run `tangible migrate` (with Compose: \
                 docker compose run --rm server migrate)",
                pending.len(),
                pending
                    .iter()
                    .map(i64::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
        Answer::Unavailable(reason) => check("Schema", CheckStatus::Problem, reason.clone()),
    };
    vec![reachable, schema]
}

fn directory_check(name: &str, directory: &DirectoryCheck) -> DiagnosticCheck {
    if !directory.present {
        return check(name, CheckStatus::Problem, "missing, or not a directory");
    }
    let mut status = CheckStatus::Ok;
    let mut parts = vec!["present".to_owned()];
    match directory.writable {
        Some(true) => parts.push("writable".to_owned()),
        Some(false) => {
            status = CheckStatus::Problem;
            parts.push("not writable".to_owned());
        }
        None => {}
    }
    if let Some(space) = directory.space {
        status = status.max(space_status(space));
        parts.push(describe_space(space));
    } else {
        status = status.max(CheckStatus::Warn);
        parts.push("free space unknown".to_owned());
    }
    check(name, status, parts.join(", "))
}

/// What integrity checks have found, and whether full scrubs happen as
/// often as they were set to. Counts and times only: no digest or name.
fn integrity_check(
    scrub_interval_days: u32,
    integrity: &Answer<IntegritySummary>,
    now: OffsetDateTime,
) -> DiagnosticCheck {
    let summary = match integrity {
        Answer::Found(summary) => summary,
        Answer::Unavailable(reason) => {
            return check("Integrity", CheckStatus::Warn, reason.clone());
        }
    };
    let mut status = CheckStatus::Ok;
    let mut parts = Vec::new();
    if summary.damaged_artifacts > 0 {
        status = CheckStatus::Problem;
        parts.push(format!(
            "{} artifact(s) damaged, and nothing was repaired: restore their objects from a \
             backup, then check them again",
            summary.damaged_artifacts
        ));
    }
    match summary.active_scrub {
        Some((read, Some(total))) => parts.push(format!(
            "a full scrub is running, {read} of {total} objects read"
        )),
        Some((_, None)) => parts.push("a full scrub is queued".to_owned()),
        None => {}
    }
    match &summary.last_scrub {
        None => parts.push("no full scrub has finished yet".to_owned()),
        Some(last) => {
            let when = timestamp(last.completed_at);
            parts.push(match last.state {
                IntegrityCheckState::Complete if last.problem_count == 0 => format!(
                    "the last full scrub, finished {when}, read {} object(s) and found nothing \
                     wrong",
                    last.objects_checked
                ),
                IntegrityCheckState::Complete => format!(
                    "the last full scrub, finished {when}, read {} object(s) and found {} \
                     problem(s)",
                    last.objects_checked, last.problem_count
                ),
                IntegrityCheckState::Failed => {
                    status = status.max(CheckStatus::Warn);
                    format!(
                        "the last full scrub failed {when} ({}); the server log says why",
                        last.error_code.as_deref().unwrap_or("no code recorded")
                    )
                }
                _ => format!("the last full scrub was withdrawn {when}"),
            });
        }
    }
    if scrub_interval_days == 0 {
        parts.push("none scheduled (TANGIBLE_SCRUB_INTERVAL_DAYS)".to_owned());
    } else {
        parts.push(format!(
            "one is queued {scrub_interval_days} day(s) after the last ends"
        ));
        // A day's grace past the interval, for a scrub still reading.
        let due_by = time::Duration::days(i64::from(scrub_interval_days) + 1);
        let overdue = summary.active_scrub.is_none()
            && summary
                .last_scrub
                .as_ref()
                .is_none_or(|last| now - last.completed_at > due_by);
        if overdue {
            status = status.max(CheckStatus::Warn);
            parts.push("and one is overdue; the server queues it while it runs".to_owned());
        }
    }
    check("Integrity", status, parts.join("; "))
}

fn space_status(space: Space) -> CheckStatus {
    let percent = percent_free(space);
    if space.free_bytes < PROBLEM_FREE_BYTES {
        CheckStatus::Problem
    } else if space.free_bytes < WARN_FREE_BYTES || percent < WARN_FREE_PERCENT {
        CheckStatus::Warn
    } else {
        CheckStatus::Ok
    }
}

fn percent_free(space: Space) -> u64 {
    if space.total_bytes == 0 {
        return 0;
    }
    let percent = u128::from(space.free_bytes) * 100 / u128::from(space.total_bytes);
    u64::try_from(percent).unwrap_or(100)
}

fn describe_space(space: Space) -> String {
    format!(
        "{} free of {} ({}%)",
        bytes(space.free_bytes),
        bytes(space.total_bytes),
        percent_free(space)
    )
}

/// A size in binary units, one decimal place: `120.5 GiB`.
#[must_use]
pub fn bytes(value: u64) -> String {
    const UNITS: [&str; 6] = ["bytes", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if value < 1024 {
        return format!("{value} bytes");
    }
    let mut unit = 0;
    let mut whole = value;
    while whole >= 1024 * 1024 && unit < UNITS.len() - 2 {
        whole /= 1024;
        unit += 1;
    }
    // `whole` is now below 1024² in units of UNITS[unit]; one more step with
    // a tenth kept.
    let tenths = whole * 10 / 1024;
    format!("{}.{} {}", tenths / 10, tenths % 10, UNITS[unit + 1])
}

fn tool_checks(settings: &DiagnosticsSettings, facts: &Facts) -> Vec<DiagnosticCheck> {
    let derivations = match &settings.derivation {
        DerivationStatus::Off => check(
            "Derivations",
            CheckStatus::Ok,
            "turned off (TANGIBLE_DERIVATION_ENGINE=none)",
        ),
        DerivationStatus::Fake => check(
            "Derivations",
            CheckStatus::Warn,
            "the fake engine, which makes placeholders, not real derivatives",
        ),
        DerivationStatus::Tool { name, version } => {
            check("Derivations", CheckStatus::Ok, format!("{name} {version}"))
        }
        DerivationStatus::Unavailable { name, reason } => check(
            "Derivations",
            CheckStatus::Warn,
            format!("{name} could not be run, so derivatives are off: {reason}"),
        ),
    };
    let romm = match &facts.romm {
        None => check("RomM export", CheckStatus::Ok, "not configured"),
        Some(directory) if !directory.present => check(
            "RomM export",
            CheckStatus::Problem,
            "configured, but the directory is missing",
        ),
        Some(directory) if directory.writable == Some(false) => check(
            "RomM export",
            CheckStatus::Problem,
            "configured, but the directory is not writable",
        ),
        Some(_) => check("RomM export", CheckStatus::Ok, "configured and writable"),
    };
    vec![derivations, romm, romm_api_check(&facts.romm_api)]
}

/// How RomM's API stands. RomM's own words about a scan stay on the RomM
/// page: they can name its host.
fn romm_api_check(romm_api: &Answer<Option<RommApiFacts>>) -> DiagnosticCheck {
    const NAME: &str = "RomM API";
    let facts = match romm_api {
        Answer::Unavailable(reason) => return check(NAME, CheckStatus::Warn, reason.clone()),
        Answer::Found(None) => return check(NAME, CheckStatus::Ok, "not configured"),
        Answer::Found(Some(facts)) => facts,
    };
    if !facts.enabled {
        return check(NAME, CheckStatus::Ok, "configured and turned off");
    }
    if facts.status == "failing" {
        return check(
            NAME,
            CheckStatus::Warn,
            format!(
                "failing ({}); try it on the Integrations page",
                facts
                    .last_error_code
                    .as_deref()
                    .unwrap_or("no code recorded")
            ),
        );
    }
    let mut status = CheckStatus::Ok;
    let mut parts = vec![match &facts.version {
        Some(version) => format!("RomM {version}"),
        None => "configured, not yet used".to_owned(),
    }];
    if facts.scans_owed > 0 {
        parts.push(format!(
            "{} platform folder(s) waiting for a scan",
            facts.scans_owed
        ));
    }
    if let Some(scan) = &facts.last_scan {
        let said = match scan.state {
            ScanState::Refused => Some("RomM refused the last scan asked for"),
            ScanState::Failed => Some("the last scan failed in RomM"),
            ScanState::Canceled => Some("the last scan was stopped in RomM"),
            ScanState::Lost => Some("RomM lost track of the last scan"),
            ScanState::TimedOut => {
                Some("the last scan had not ended when Tangible stopped waiting")
            }
            ScanState::Queued | ScanState::Running => {
                parts.push("a scan is under way".to_owned());
                None
            }
            ScanState::Finished => {
                parts.push("the last scan finished".to_owned());
                None
            }
        };
        if let Some(said) = said {
            status = CheckStatus::Warn;
            parts.push(format!("{said}; the RomM page says why"));
        }
    }
    check(NAME, status, parts.join(", "))
}

/// `worker 1`, `worker 2`, … in the order they enrolled.
fn worker_labels(workers: &[WorkerDiagnostic]) -> BTreeMap<uuid::Uuid, String> {
    workers
        .iter()
        .enumerate()
        .map(|(index, worker)| (*worker.id.as_uuid(), format!("worker {}", index + 1)))
        .collect()
}

fn worker_checks(
    build: &BuildIdentity,
    workers: &Answer<Vec<WorkerDiagnostic>>,
    now: OffsetDateTime,
) -> Vec<DiagnosticCheck> {
    let workers = match workers {
        Answer::Found(workers) => workers,
        Answer::Unavailable(reason) => {
            return vec![check("Workers", CheckStatus::Warn, reason.clone())];
        }
    };
    if workers.iter().all(|worker| worker.status == "revoked") {
        return vec![check(
            "Workers",
            CheckStatus::Warn,
            "none enrolled: nothing can burn until a burn worker is",
        )];
    }
    let labels = worker_labels(workers);
    let mut checks = Vec::new();
    let mut revoked = 0_usize;
    for worker in workers {
        if worker.status == "revoked" {
            revoked += 1;
            continue;
        }
        let label = labels
            .get(worker.id.as_uuid())
            .cloned()
            .unwrap_or_else(|| "worker".to_owned());
        checks.push(worker_check(label, build, worker, now));
    }
    let mut checks = cap(checks, "worker(s)");
    if revoked > 0 {
        checks.push(check(
            "Revoked",
            CheckStatus::Ok,
            format!("{revoked} worker(s) revoked, not shown"),
        ));
    }
    checks
}

fn worker_check(
    label: String,
    build: &BuildIdentity,
    worker: &WorkerDiagnostic,
    now: OffsetDateTime,
) -> DiagnosticCheck {
    let mut status = CheckStatus::Ok;
    let mut parts = vec![worker.status.clone()];
    match worker.status.as_str() {
        "incompatible" => status = CheckStatus::Problem,
        "pending" | "offline" | "draining" => status = CheckStatus::Warn,
        _ => {}
    }
    parts.push(format!("software {}", worker.software_version));
    if worker.software_version != build.version {
        status = status.max(CheckStatus::Warn);
        parts.push(format!("differs from the server's {}", build.version));
    }
    parts.push(format!("protocol {}", worker.protocol_version));
    if worker.protocol_version != PROTOCOL_VERSION {
        status = status.max(CheckStatus::Problem);
        parts.push(format!("the server speaks {PROTOCOL_VERSION}"));
    }
    match worker.last_seen_at {
        Some(seen) => {
            let seconds = (now - seen).whole_seconds().max(0);
            parts.push(format!("last heard from {} ago", ago(seconds)));
            if seconds > SILENT_AFTER_SECONDS && worker.status == "online" {
                status = status.max(CheckStatus::Warn);
                parts.push("its heartbeats have stopped".to_owned());
            }
        }
        None => parts.push("never heard from".to_owned()),
    }
    parts.push(format!("engines: {}", engines(&worker.engines)));
    check(label, status, parts.join(", "))
}

fn ago(seconds: i64) -> String {
    match seconds {
        0..60 => format!("{seconds} s"),
        60..3600 => format!("{} min", seconds / 60),
        3600..86_400 => format!("{} h", seconds / 3600),
        _ => format!("{} days", seconds / 86_400),
    }
}

/// `xorriso 1.5.6, cdrdao 1.2.4`, from what a worker reported. Only plain
/// names and versions are repeated.
fn engines(reported: &serde_json::Value) -> String {
    let plain = |value: &serde_json::Value| {
        value
            .as_str()
            .filter(|text| {
                !text.is_empty()
                    && text.len() <= 64
                    && text
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
            })
            .map(str::to_owned)
    };
    let names: Vec<String> = reported
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|engine| {
            let name = plain(&engine["name"])?;
            Some(match plain(&engine["version"]) {
                Some(version) => format!("{name} {version}"),
                None => name,
            })
        })
        .collect();
    if names.is_empty() {
        "none reported".to_owned()
    } else {
        names.join(", ")
    }
}

fn drive_checks(
    workers: &Answer<Vec<WorkerDiagnostic>>,
    drives: &Answer<Vec<DriveRecord>>,
) -> Vec<DiagnosticCheck> {
    let drives = match drives {
        Answer::Found(drives) => drives,
        Answer::Unavailable(reason) => {
            return vec![check("Drives", CheckStatus::Warn, reason.clone())];
        }
    };
    let labels = match workers {
        Answer::Found(workers) => worker_labels(workers),
        Answer::Unavailable(_) => BTreeMap::new(),
    };
    let drives: Vec<&DriveRecord> = drives
        .iter()
        .filter(|drive| drive.worker_status != "revoked")
        .collect();
    if drives.is_empty() {
        return vec![check(
            "Drives",
            CheckStatus::Warn,
            "none reported by any worker",
        )];
    }
    let checks = drives
        .iter()
        .enumerate()
        .map(|(index, drive)| {
            let status = match drive.status.as_str() {
                "error" | "missing" => CheckStatus::Problem,
                "unknown" | "disabled" => CheckStatus::Warn,
                _ => CheckStatus::Ok,
            };
            let hardware = [&drive.vendor, &drive.model]
                .into_iter()
                .flatten()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            let mut parts = vec![if hardware.is_empty() {
                "hardware not reported".to_owned()
            } else {
                hardware
            }];
            if let Some(firmware) = &drive.firmware {
                parts.push(format!("firmware {firmware}"));
            }
            parts.push(match drive.connection.as_str() {
                "unknown" => "connection unknown".to_owned(),
                connection => connection.to_owned(),
            });
            parts.push(match drive.status.as_str() {
                "unknown" => "status unknown".to_owned(),
                status => status.replace('_', " "),
            });
            if let Some(worker) = labels.get(drive.worker_id.as_uuid()) {
                parts.push(format!("on {worker}"));
            }
            check(format!("drive {}", index + 1), status, parts.join(", "))
        })
        .collect();
    cap(checks, "drive(s)")
}

/// The first [`MAX_LISTED`] checks, and one line counting the rest by
/// status, carrying the worst of them.
fn cap(mut checks: Vec<DiagnosticCheck>, noun: &str) -> Vec<DiagnosticCheck> {
    if checks.len() <= MAX_LISTED {
        return checks;
    }
    let rest = checks.split_off(MAX_LISTED);
    let count = |status| rest.iter().filter(|check| check.status == status).count();
    let worst = rest
        .iter()
        .map(|check| check.status)
        .max()
        .unwrap_or(CheckStatus::Ok);
    checks.push(check(
        "More",
        worst,
        format!(
            "{} more {noun} not listed: {} OK, {} with warnings, {} with problems",
            rest.len(),
            count(CheckStatus::Ok),
            count(CheckStatus::Warn),
            count(CheckStatus::Problem)
        ),
    ));
    checks
}

fn failure_checks(failures: &Answer<Vec<Failure>>) -> Vec<DiagnosticCheck> {
    let failures = match failures {
        Answer::Found(failures) => failures,
        Answer::Unavailable(reason) => {
            return vec![check("Failures", CheckStatus::Warn, reason.clone())];
        }
    };
    if failures.is_empty() {
        return vec![check("Failures", CheckStatus::Ok, "none recorded")];
    }
    // By kind and code, in the order each was first seen: newest first.
    let mut groups: Vec<(&str, &str, usize, OffsetDateTime)> = Vec::new();
    for failure in failures {
        let kind = failure.kind.as_str();
        match groups
            .iter_mut()
            .find(|(k, code, _, _)| *k == kind && *code == failure.code)
        {
            Some(group) => group.2 += 1,
            None => groups.push((kind, failure.code.as_str(), 1, failure.at)),
        }
    }
    groups
        .into_iter()
        .map(|(kind, code, count, latest)| {
            check(
                format!("{kind} {code}"),
                CheckStatus::Warn,
                format!(
                    "{count} of the last {} failures, most recently {}",
                    failures.len(),
                    timestamp(latest)
                ),
            )
        })
        .collect()
}

fn setting_checks(settings: &DiagnosticsSettings) -> Vec<DiagnosticCheck> {
    vec![
        if settings.public_url_https {
            check("Public URL", CheckStatus::Ok, "https")
        } else {
            check(
                "Public URL",
                CheckStatus::Warn,
                "plain http: passwords and session cookies cross the network unencrypted",
            )
        },
        check(
            "Issue reports",
            CheckStatus::Ok,
            match (
                settings.reports.enabled(),
                settings.reports.scrub_addresses(),
            ) {
                (false, _) => "turned off",
                (true, true) => "offered, with addresses scrubbed",
                (true, false) => "offered, with addresses kept",
            },
        ),
        check(
            "Rip decryption",
            CheckStatus::Ok,
            if settings.decryption_allowed {
                "allowed, on workers that can"
            } else {
                "off"
            },
        ),
        check(
            "URL imports",
            CheckStatus::Ok,
            &match settings.url_imports {
                None => "off".to_owned(),
                Some(setting) => format!(
                    "on: {}, {}{}",
                    if setting.allow_http {
                        "https and http"
                    } else {
                        "https only"
                    },
                    match setting.allowed_blocks {
                        0 => "public addresses only".to_owned(),
                        1 => "public addresses and 1 allowed block".to_owned(),
                        n => format!("public addresses and {n} allowed blocks"),
                    },
                    if setting.proxy {
                        ", through a proxy"
                    } else {
                        ""
                    }
                ),
            },
        ),
        check(
            "Migrate on start",
            CheckStatus::Ok,
            if settings.migrate_on_start {
                "on"
            } else {
                "off"
            },
        ),
    ]
}

/// Whether Tangible can delete where it was told it may: each removable
/// root there and writable. Counted, never named.
fn removable_roots_check(roots: &[DirectoryCheck]) -> DiagnosticCheck {
    let total = roots.len();
    let unusable = roots
        .iter()
        .filter(|root| !root.present || root.writable != Some(true))
        .count();
    match (total, unusable) {
        (0, _) => check(
            "Removable roots",
            CheckStatus::Ok,
            "none: every watched root is only read",
        ),
        (_, 0) => check(
            "Removable roots",
            CheckStatus::Ok,
            format!(
                "{total} {}, writable",
                if total == 1 { "root" } else { "roots" }
            ),
        ),
        _ => check(
            "Removable roots",
            CheckStatus::Warn,
            format!(
                "{unusable} of {total} missing or read-only: Tangible cannot delete imported \
                 downloads there; mount them read-write"
            ),
        ),
    }
}

/// Whether the integration secrets stored can be opened with the key held.
fn secret_check(key_id: Option<&str>, sealed: &Answer<Vec<(String, i64)>>) -> DiagnosticCheck {
    const NAME: &str = "Integration secrets";
    let counts = match sealed {
        Answer::Found(counts) => counts,
        Answer::Unavailable(reason) => return check(NAME, CheckStatus::Warn, reason.clone()),
    };
    let total: i64 = counts.iter().map(|(_, n)| n).sum();
    let elsewhere: i64 = counts
        .iter()
        .filter(|(id, _)| Some(id.as_str()) != key_id)
        .map(|(_, n)| n)
        .sum();
    match (key_id, total, elsewhere) {
        (None, 0, _) => check(
            NAME,
            CheckStatus::Ok,
            "no key: integrations cannot hold a password until TANGIBLE_SECRET_KEY_FILE is set",
        ),
        (None, _, _) => check(
            NAME,
            CheckStatus::Problem,
            format!(
                "{total} stored and no key to open them: set TANGIBLE_SECRET_KEY_FILE to the key \
                 they were sealed with"
            ),
        ),
        (Some(_), _, 0) => check(
            NAME,
            CheckStatus::Ok,
            format!("key loaded; {total} stored, all sealed with it"),
        ),
        (Some(_), _, _) => check(
            NAME,
            CheckStatus::Problem,
            format!(
                "{elsewhere} of {total} stored were sealed with another key: restore that key \
                 file, run tangible secrets rotate with it as the old key, or enter them again"
            ),
        ),
    }
}

/// The report as Markdown, for a terminal, a file, or an issue.
#[must_use]
pub fn markdown(report: &Diagnostics) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "## Tangible diagnostics\n");
    let _ = writeln!(out, "- Build: {}", report.build);
    let _ = writeln!(out, "- Collected: {}", report.generated_at);
    let _ = writeln!(out, "- Overall: {}", report.status.label());
    for section in &report.sections {
        let _ = writeln!(out, "\n### {}\n", section.title);
        for check in &section.checks {
            let _ = writeln!(
                out,
                "- **{}** {}: {}",
                check.status.label(),
                check.name,
                check.detail
            );
        }
    }
    out
}

/// The report as a bug report about the deployment, scrubbed like any other
/// report before anyone sees it. Diagnostics already leave out names and
/// paths; scrubbing is the second line.
#[must_use]
pub fn issue_report(
    report: &Diagnostics,
    build: &BuildIdentity,
    context: ScrubContext,
) -> IssueReport {
    let count = |status| {
        report
            .sections
            .iter()
            .flat_map(|section| &section.checks)
            .filter(|check| check.status == status)
            .count()
    };
    let (problems, warnings) = (count(CheckStatus::Problem), count(CheckStatus::Warn));
    let mut writer = Writer::new(context);
    writer.plain("version", "Tangible version", &report.build);
    writer.plain(
        "deployment",
        "Deployment",
        if build.release.is_some() {
            RELEASE_IMAGE
        } else {
            ""
        },
    );
    writer.plain("host", "Host", "");
    writer.plain("area", "Area", "deployment");
    writer.plain(
        "what_happened",
        "What happened?",
        &format!(
            "Diagnostics found {problems} problem(s) and {warnings} warning(s). The full \
             report is under Logs."
        ),
    );
    writer.plain("what_expected", "What did you expect to happen?", "");
    writer.plain("steps", "Steps to reproduce", "");
    writer.scrubbed("logs", "Logs", &markdown(report));
    let title = if problems > 0 {
        Some(format!("Bug: diagnostics show {problems} problem(s)"))
    } else if warnings > 0 {
        Some(format!("Bug: diagnostics show {warnings} warning(s)"))
    } else {
        None
    };
    writer.finish(BUG_TEMPLATE, title, Some("logs"))
}

/// A link to the issue form with every field filled in except the long
/// one, which the person pastes: a terminal has no clipboard to put it on,
/// and a whole report rarely fits a link.
#[must_use]
pub fn form_link(issues_url: &str, report: &IssueReport) -> String {
    let mut link = format!("{issues_url}?template={}", encode(&report.template));
    if let Some(title) = &report.title {
        let _ = write!(link, "&title={}", encode(title));
    }
    for field in &report.fields {
        if field.value.is_empty() || report.long_field.as_deref() == Some(field.id.as_str()) {
            continue;
        }
        let _ = write!(link, "&{}={}", encode(&field.id), encode(&field.value));
    }
    link
}

/// Percent-encoding for a query value: only unreserved characters stay.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

    use super::*;
    use std::path::Path;
    use tangible_db::diagnostics::FailureKind;
    use tangible_domain::{DriveId, WorkerId};

    const GIB: u64 = 1024 * 1024 * 1024;

    /// 2026-10-17T12:00:00Z.
    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_792_238_400).unwrap()
    }

    fn mentions(text: &str, path: &Path) -> bool {
        text.contains(&path.display().to_string())
    }

    fn build() -> BuildIdentity {
        BuildIdentity {
            version: "0.1.0".to_owned(),
            release: Some("v0.1.0".to_owned()),
            revision: Some("76fa5157aea012aae85461e694082d553071a606".to_owned()),
        }
    }

    fn settings() -> DiagnosticsSettings {
        DiagnosticsSettings {
            build: build(),
            storage_root: PathBuf::from("/srv/private-library-root"),
            staging_root: PathBuf::from("/srv/private-staging-root"),
            romm_export_root: None,
            derivation: DerivationStatus::Tool {
                name: "chdman".to_owned(),
                version: "0.251".to_owned(),
            },
            public_url_https: true,
            migrate_on_start: false,
            reports: ReportSettings::disabled(build()),
            decryption_allowed: false,
            url_imports: None,
            secret_key_id: None,
            scrub_interval_days: 0,
            removable_roots: Vec::new(),
        }
    }

    fn roomy() -> DirectoryCheck {
        DirectoryCheck {
            present: true,
            writable: Some(true),
            space: Some(Space {
                free_bytes: 500 * GIB,
                total_bytes: 1000 * GIB,
            }),
        }
    }

    fn worker(status: &str) -> WorkerDiagnostic {
        WorkerDiagnostic {
            id: WorkerId::generate(),
            status: status.to_owned(),
            software_version: "0.1.0".to_owned(),
            protocol_version: "1alpha1".to_owned(),
            last_seen_at: Some(now() - time::Duration::seconds(5)),
            engines: serde_json::json!([
                { "name": "xorriso", "version": "1.5.6" },
                { "name": "cdrdao", "version": "1.2.4" }
            ]),
        }
    }

    fn drive(worker: &WorkerDiagnostic, status: &str) -> DriveRecord {
        DriveRecord {
            id: DriveId::generate(),
            configured_name: "living-room-burner".to_owned(),
            device_alias: "/dev/sr0".to_owned(),
            vendor: Some("Slimtype".to_owned()),
            model: Some("DVD A DS8A8SH".to_owned()),
            firmware: Some("KS21".to_owned()),
            connection: "usb".to_owned(),
            status: status.to_owned(),
            last_seen_at: Some(now()),
            worker_id: worker.id,
            worker_name: "burner-in-the-den".to_owned(),
            worker_status: worker.status.clone(),
            worker_last_seen_at: worker.last_seen_at,
            can_rip: true,
            can_decrypt: false,
            has_redumper: false,
        }
    }

    fn healthy() -> Facts {
        let one = worker("online");
        let drives = vec![drive(&one, "ready_empty")];
        Facts {
            database: Answer::Found(()),
            pending_migrations: Answer::Found(Vec::new()),
            library: roomy(),
            staging: roomy(),
            romm: None,
            workers: Answer::Found(vec![one]),
            drives: Answer::Found(drives),
            failures: Answer::Found(Vec::new()),
            sealed_secrets: Answer::Found(Vec::new()),
            integrity: Answer::Found(IntegritySummary {
                last_scrub: None,
                active_scrub: None,
                damaged_artifacts: 0,
            }),
            romm_api: Answer::Found(None),
            removable_roots: Vec::new(),
        }
    }

    fn find<'a>(report: &'a Diagnostics, name: &str) -> &'a DiagnosticCheck {
        report
            .sections
            .iter()
            .flat_map(|section| &section.checks)
            .find(|check| check.name == name)
            .unwrap_or_else(|| panic!("no check named {name}"))
    }

    #[test]
    fn a_healthy_deployment_is_all_ok() {
        let report = assess(&settings(), &healthy(), now());
        assert_eq!(report.status, CheckStatus::Ok, "{report:#?}");
        assert_eq!(report.build, "v0.1.0 (76fa515)");
        assert_eq!(report.generated_at, "2026-10-17T12:00:00Z");
        assert_eq!(
            find(&report, "worker 1").detail,
            "online, software 0.1.0, protocol 1alpha1, last heard from 5 s ago, \
             engines: xorriso 1.5.6, cdrdao 1.2.4"
        );
        assert_eq!(
            find(&report, "drive 1").detail,
            "Slimtype DVD A DS8A8SH, firmware KS21, usb, ready empty, on worker 1"
        );
        assert_eq!(
            find(&report, "Library").detail,
            "present, writable, 500.0 GiB free of 1000.0 GiB (50%)"
        );
    }

    #[test]
    fn romms_api_says_how_it_stands_and_never_romms_words() {
        let scan = |state| ScanRecord {
            id: uuid::Uuid::nil(),
            integration_id: uuid::Uuid::nil(),
            platforms: vec!["psx".to_owned()],
            task_id: None,
            state,
            detail: Some("RomM could not be reached: romm.private.example".to_owned()),
            counts: None,
            requested_at: now(),
            ended_at: None,
        };
        let working = RommApiFacts {
            enabled: true,
            status: "ok".to_owned(),
            last_error_code: None,
            version: Some("5.4.1".to_owned()),
            scans_owed: 0,
            last_scan: Some(scan(ScanState::Finished)),
        };
        let judged = |facts: Option<RommApiFacts>| {
            let mut all = healthy();
            all.romm_api = Answer::Found(facts);
            let report = assess(&settings(), &all, now());
            let found = find(&report, "RomM API").clone();
            (found.status, found.detail)
        };
        assert_eq!(judged(None), (CheckStatus::Ok, "not configured".to_owned()));
        assert_eq!(
            judged(Some(working.clone())),
            (
                CheckStatus::Ok,
                "RomM 5.4.1, the last scan finished".to_owned()
            )
        );
        assert_eq!(
            judged(Some(RommApiFacts {
                scans_owed: 2,
                last_scan: Some(scan(ScanState::Running)),
                ..working.clone()
            })),
            (
                CheckStatus::Ok,
                "RomM 5.4.1, 2 platform folder(s) waiting for a scan, a scan is under way"
                    .to_owned()
            )
        );
        for state in [
            ScanState::Refused,
            ScanState::Failed,
            ScanState::Canceled,
            ScanState::Lost,
            ScanState::TimedOut,
        ] {
            let (status, detail) = judged(Some(RommApiFacts {
                last_scan: Some(scan(state)),
                ..working.clone()
            }));
            assert_eq!(status, CheckStatus::Warn, "{state:?}");
            assert!(detail.ends_with("the RomM page says why"), "{detail}");
            assert!(!detail.contains("example"), "{detail}");
        }
        assert_eq!(
            judged(Some(RommApiFacts {
                status: "failing".to_owned(),
                last_error_code: Some("INTEGRATION_UNREACHABLE".to_owned()),
                ..working.clone()
            })),
            (
                CheckStatus::Warn,
                "failing (INTEGRATION_UNREACHABLE); try it on the Integrations page".to_owned()
            )
        );
        assert_eq!(
            judged(Some(RommApiFacts {
                enabled: false,
                ..working
            }))
            .1,
            "configured and turned off"
        );
    }

    #[test]
    fn the_report_carries_no_names_or_paths() {
        let mut settings = settings();
        settings.romm_export_root = Some(PathBuf::from("/srv/private-romm-root"));
        let mut facts = healthy();
        facts.romm = Some(roomy());
        let text = markdown(&assess(&settings, &facts, now()));
        for path in [
            &settings.storage_root,
            &settings.staging_root,
            settings.romm_export_root.as_ref().unwrap(),
        ] {
            assert!(!mentions(&text, path), "{text}");
        }
        assert!(!text.contains("burner-in-the-den"), "{text}");
        assert!(!text.contains("living-room-burner"), "{text}");
        assert!(!text.contains("/dev/sr0"), "{text}");
    }

    #[test]
    fn a_database_that_does_not_answer_is_a_problem_and_the_rest_is_not_checked() {
        let mut facts = healthy();
        let reason = "not checked: the database did not answer within 5 seconds".to_owned();
        facts.database = Answer::Unavailable("the database did not answer within 5 seconds".into());
        facts.pending_migrations = Answer::Unavailable(reason.clone());
        facts.workers = Answer::Unavailable(reason.clone());
        facts.drives = Answer::Unavailable(reason.clone());
        facts.failures = Answer::Unavailable(reason);
        let report = assess(&settings(), &facts, now());
        assert_eq!(report.status, CheckStatus::Problem);
        assert_eq!(find(&report, "Connection").status, CheckStatus::Problem);
        assert_eq!(find(&report, "Workers").status, CheckStatus::Warn);
        assert!(find(&report, "Workers").detail.starts_with("not checked"));
    }

    #[test]
    fn pending_migrations_are_a_problem_naming_the_fix() {
        let mut facts = healthy();
        facts.pending_migrations = Answer::Found(vec![9, 10]);
        let schema = find(&assess(&settings(), &facts, now()), "Schema").clone();
        assert_eq!(schema.status, CheckStatus::Problem);
        assert!(
            schema
                .detail
                .starts_with("2 migration(s) not applied (9, 10)")
        );
        assert!(schema.detail.contains("tangible migrate"));
    }

    #[test]
    fn removable_roots_are_counted_and_one_that_cannot_be_written_is_a_warning() {
        let mut facts = healthy();
        let report = assess(&settings(), &facts, now());
        let none = find(&report, "Removable roots");
        assert_eq!(none.status, CheckStatus::Ok);
        assert_eq!(none.detail, "none: every watched root is only read");

        facts.removable_roots = vec![roomy(), roomy()];
        let both = assess(&settings(), &facts, now());
        assert_eq!(find(&both, "Removable roots").detail, "2 roots, writable");

        let mut read_only = roomy();
        read_only.writable = Some(false);
        facts.removable_roots = vec![roomy(), read_only];
        let report = assess(&settings(), &facts, now());
        let warned = find(&report, "Removable roots");
        assert_eq!(warned.status, CheckStatus::Warn);
        assert!(
            warned.detail.starts_with("1 of 2 missing or read-only"),
            "{}",
            warned.detail
        );
    }

    #[test]
    fn storage_is_judged_by_room_and_writability() {
        let at = |free: u64, total: u64| {
            let mut facts = healthy();
            facts.staging = DirectoryCheck {
                space: Some(Space {
                    free_bytes: free,
                    total_bytes: total,
                }),
                ..roomy()
            };
            find(&assess(&settings(), &facts, now()), "Staging").status
        };
        assert_eq!(at(500 * GIB, 1000 * GIB), CheckStatus::Ok);
        assert_eq!(at(50 * GIB, 1000 * GIB), CheckStatus::Warn, "5% free");
        assert_eq!(at(4 * GIB, 8 * GIB), CheckStatus::Warn, "under 5 GiB");
        assert_eq!(at(GIB / 2, GIB), CheckStatus::Problem, "under 1 GiB");

        let mut facts = healthy();
        facts.library = DirectoryCheck {
            writable: Some(false),
            ..roomy()
        };
        let library = find(&assess(&settings(), &facts, now()), "Library").clone();
        assert_eq!(library.status, CheckStatus::Problem);
        assert!(library.detail.contains("not writable"));

        facts.library = DirectoryCheck {
            present: false,
            writable: None,
            space: None,
        };
        let library = find(&assess(&settings(), &facts, now()), "Library").clone();
        assert_eq!(library.status, CheckStatus::Problem);
        assert_eq!(library.detail, "missing, or not a directory");
    }

    #[test]
    fn a_worker_that_stopped_sending_heartbeats_is_a_warning() {
        let mut facts = healthy();
        let mut quiet = worker("online");
        quiet.last_seen_at = Some(now() - time::Duration::minutes(10));
        facts.workers = Answer::Found(vec![quiet]);
        let check = find(&assess(&settings(), &facts, now()), "worker 1").clone();
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.detail.contains("last heard from 10 min ago"));
        assert!(check.detail.contains("its heartbeats have stopped"));
    }

    #[test]
    fn worker_versions_are_compared_with_the_server() {
        let mut facts = healthy();
        let mut older = worker("online");
        older.software_version = "0.0.9".to_owned();
        let mut foreign = worker("incompatible");
        foreign.protocol_version = "2".to_owned();
        facts.workers = Answer::Found(vec![older, foreign]);
        let report = assess(&settings(), &facts, now());
        let first = find(&report, "worker 1");
        assert_eq!(first.status, CheckStatus::Warn);
        assert!(first.detail.contains("differs from the server's 0.1.0"));
        let second = find(&report, "worker 2");
        assert_eq!(second.status, CheckStatus::Problem);
        assert!(second.detail.contains("the server speaks 1alpha1"));
    }

    #[test]
    fn revoked_workers_and_their_drives_are_left_out() {
        let mut facts = healthy();
        let gone = worker("revoked");
        facts.drives = Answer::Found(vec![drive(&gone, "ready_empty")]);
        facts.workers = Answer::Found(vec![gone]);
        let report = assess(&settings(), &facts, now());
        assert_eq!(find(&report, "Workers").status, CheckStatus::Warn);
        assert_eq!(
            find(&report, "Drives").detail,
            "none reported by any worker"
        );
    }

    #[test]
    fn a_long_list_of_workers_is_cut_short_and_counted() {
        let mut facts = healthy();
        let mut workers: Vec<WorkerDiagnostic> = (0..25).map(|_| worker("online")).collect();
        workers[22].status = "incompatible".to_owned();
        facts.workers = Answer::Found(workers);
        let report = assess(&settings(), &facts, now());
        let section = report
            .sections
            .iter()
            .find(|section| section.title == "Workers")
            .unwrap();
        assert_eq!(section.checks.len(), MAX_LISTED + 1);
        let more = section.checks.last().unwrap();
        assert_eq!(more.name, "More");
        assert_eq!(more.status, CheckStatus::Problem);
        assert_eq!(
            more.detail,
            "5 more worker(s) not listed: 4 OK, 0 with warnings, 1 with problems"
        );
    }

    #[test]
    fn a_drive_in_error_is_a_problem() {
        let mut facts = healthy();
        let Answer::Found(workers) = &facts.workers else {
            unreachable!()
        };
        let mut broken = drive(&workers[0], "error");
        broken.connection = "unknown".to_owned();
        facts.drives = Answer::Found(vec![broken]);
        let check = find(&assess(&settings(), &facts, now()), "drive 1").clone();
        assert_eq!(check.status, CheckStatus::Problem);
        assert!(
            check.detail.contains("connection unknown, error"),
            "{}",
            check.detail
        );
    }

    #[test]
    fn text_from_a_worker_cannot_add_lines() {
        let mut facts = healthy();
        let Answer::Found(workers) = &facts.workers else {
            unreachable!()
        };
        let mut odd = drive(&workers[0], "ready_empty");
        odd.model = Some("Model\n- **OK** Forged: line".to_owned());
        facts.drives = Answer::Found(vec![odd]);
        let text = markdown(&assess(&settings(), &facts, now()));
        assert!(!text.contains("\n- **OK** Forged"), "{text}");
        assert!(text.contains("Model - **OK** Forged: line"));
    }

    #[test]
    fn engine_reports_repeat_only_plain_names_and_versions() {
        assert_eq!(
            engines(&serde_json::json!([
                { "name": "xorriso", "version": "1.5.6" },
                { "name": "evil name with spaces", "version": "1" },
                { "name": "cdrdao", "version": "/home/someone/cdrdao" },
                "not an object"
            ])),
            "xorriso 1.5.6, cdrdao"
        );
        assert_eq!(engines(&serde_json::json!({})), "none reported");
    }

    #[test]
    fn failures_are_grouped_by_code_newest_first() {
        let mut facts = healthy();
        let failure = |kind, code: &str, minutes| Failure {
            kind,
            code: code.to_owned(),
            at: now() - time::Duration::minutes(minutes),
        };
        facts.failures = Answer::Found(vec![
            failure(FailureKind::Burn, "ENGINE_FAILED", 1),
            failure(FailureKind::Import, "IMPORT_STORAGE_FAILED", 2),
            failure(FailureKind::Burn, "ENGINE_FAILED", 3),
        ]);
        let report = assess(&settings(), &facts, now());
        let section = report
            .sections
            .iter()
            .find(|section| section.title == "Recent failures")
            .unwrap();
        let names: Vec<&str> = section
            .checks
            .iter()
            .map(|check| check.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["burn ENGINE_FAILED", "import IMPORT_STORAGE_FAILED"]
        );
        assert_eq!(
            section.checks[0].detail,
            "2 of the last 3 failures, most recently 2026-10-17T11:59:00Z"
        );
        assert_eq!(report.status, CheckStatus::Warn);
    }

    #[test]
    fn settings_say_what_they_are() {
        let mut settings = settings();
        settings.public_url_https = false;
        settings.derivation = DerivationStatus::Unavailable {
            name: "chdman".to_owned(),
            reason: "not found".to_owned(),
        };
        let mut facts = healthy();
        settings.romm_export_root = Some(PathBuf::from("/x"));
        facts.romm = Some(DirectoryCheck {
            present: false,
            writable: None,
            space: None,
        });
        let report = assess(&settings, &facts, now());
        assert_eq!(find(&report, "Public URL").status, CheckStatus::Warn);
        assert_eq!(find(&report, "Derivations").status, CheckStatus::Warn);
        assert_eq!(find(&report, "RomM export").status, CheckStatus::Problem);
        assert_eq!(find(&report, "Issue reports").detail, "turned off");
        assert_eq!(report.status, CheckStatus::Problem);
    }

    #[test]
    fn integration_secrets_say_whether_the_key_opens_them() {
        let mut settings = settings();
        let mut facts = healthy();
        let row = |settings: &DiagnosticsSettings, facts: &Facts| {
            find(&assess(settings, facts, now()), "Integration secrets").clone()
        };
        let none = row(&settings, &facts);
        assert_eq!(none.status, CheckStatus::Ok);
        assert!(
            none.detail.contains("TANGIBLE_SECRET_KEY_FILE"),
            "{}",
            none.detail
        );

        facts.sealed_secrets = Answer::Found(vec![("aaaaaaaaaaaaaaaa".to_owned(), 2)]);
        assert_eq!(row(&settings, &facts).status, CheckStatus::Problem);

        settings.secret_key_id = Some("aaaaaaaaaaaaaaaa".to_owned());
        let held = row(&settings, &facts);
        assert_eq!(held.status, CheckStatus::Ok);
        assert_eq!(held.detail, "key loaded; 2 stored, all sealed with it");
        assert!(
            !held.detail.contains("aaaa"),
            "the key id stays out of the report"
        );

        facts.sealed_secrets = Answer::Found(vec![
            ("aaaaaaaaaaaaaaaa".to_owned(), 2),
            ("bbbbbbbbbbbbbbbb".to_owned(), 1),
        ]);
        let mixed = row(&settings, &facts);
        assert_eq!(mixed.status, CheckStatus::Problem);
        assert!(
            mixed.detail.starts_with("1 of 3 stored"),
            "{}",
            mixed.detail
        );
    }

    #[test]
    fn url_imports_say_how_far_they_reach() {
        let mut settings = settings();
        let facts = healthy();
        assert_eq!(
            find(&assess(&settings, &facts, now()), "URL imports").detail,
            "off"
        );
        settings.url_imports = Some(UrlImportSetting {
            allow_http: false,
            allowed_blocks: 0,
            proxy: false,
        });
        assert_eq!(
            find(&assess(&settings, &facts, now()), "URL imports").detail,
            "on: https only, public addresses only"
        );
        settings.url_imports = Some(UrlImportSetting {
            allow_http: true,
            allowed_blocks: 2,
            proxy: true,
        });
        let report = assess(&settings, &facts, now());
        let row = find(&report, "URL imports");
        assert_eq!(
            row.detail,
            "on: https and http, public addresses and 2 allowed blocks, through a proxy"
        );
        assert_eq!(row.status, CheckStatus::Ok);
    }

    #[test]
    fn the_issue_report_fills_the_bug_form_and_scrubs_the_log() {
        let mut facts = healthy();
        facts.pending_migrations = Answer::Found(vec![10]);
        facts.drives = Answer::Found(Vec::new());
        let report = assess(&settings(), &facts, now());
        let issue = issue_report(&report, &build(), ScrubContext::new());
        crate::report::forms::assert_fits(&issue, &crate::report::forms::bug());
        let field = |id: &str| {
            issue
                .fields
                .iter()
                .find(|field| field.id == id)
                .unwrap()
                .value
                .clone()
        };
        assert_eq!(field("area"), "deployment");
        assert_eq!(field("version"), "v0.1.0 (76fa515)");
        assert_eq!(field("deployment"), RELEASE_IMAGE);
        assert_eq!(
            field("what_happened"),
            "Diagnostics found 1 problem(s) and 1 warning(s). The full report is under Logs."
        );
        assert_eq!(
            issue.title.as_deref(),
            Some("Bug: diagnostics show 1 problem(s)")
        );
        assert_eq!(issue.long_field.as_deref(), Some("logs"));
        assert!(field("logs").contains("- **Problem** Schema: 1 migration(s) not applied (10)"));
    }

    #[test]
    fn a_clean_report_suggests_no_title() {
        let issue = issue_report(
            &assess(&settings(), &healthy(), now()),
            &build(),
            ScrubContext::new(),
        );
        assert!(issue.title.is_none());
    }

    #[test]
    fn the_form_link_carries_every_short_field_and_leaves_the_log_to_paste() {
        let mut facts = healthy();
        facts.pending_migrations = Answer::Found(vec![10]);
        let issue = issue_report(
            &assess(&settings(), &facts, now()),
            &build(),
            ScrubContext::new(),
        );
        let link = form_link(
            "https://github.com/digital-grease/tangible/issues/new",
            &issue,
        );
        assert!(link.starts_with(
            "https://github.com/digital-grease/tangible/issues/new?template=bug_report.yml\
             &title=Bug%3A%20diagnostics%20show%201%20problem%28s%29"
        ));
        assert!(link.contains("&version=v0.1.0%20%2876fa515%29"));
        assert!(link.contains("&area=deployment"));
        assert!(!link.contains("&logs="));
        assert!(!link.contains("&host="), "empty fields are left out");
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(bytes(0), "0 bytes");
        assert_eq!(bytes(1023), "1023 bytes");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(5 * GIB), "5.0 GiB");
        assert_eq!(bytes(1800 * GIB), "1.7 TiB");
        assert_eq!(bytes(u64::MAX), "16383.9 PiB");
    }

    #[test]
    fn markdown_lists_every_check_with_its_status_in_words() {
        let mut facts = healthy();
        facts.pending_migrations = Answer::Found(vec![10]);
        let text = markdown(&assess(&settings(), &facts, now()));
        assert!(text.starts_with("## Tangible diagnostics\n\n- Build: v0.1.0 (76fa515)\n"));
        assert!(text.contains("- Overall: Problem\n"));
        assert!(text.contains("\n### Database\n\n- **OK** Connection: reachable\n"));
        assert!(text.contains("- **Problem** Schema: 1 migration(s) not applied (10)"));
    }

    fn scrubbed(
        state: IntegrityCheckState,
        days_ago: i64,
        problems: i32,
    ) -> tangible_db::integrity::ScrubSummary {
        tangible_db::integrity::ScrubSummary {
            state,
            completed_at: now() - time::Duration::days(days_ago),
            objects_checked: 1200,
            problem_count: problems,
            error_code: (state == IntegrityCheckState::Failed)
                .then(|| "storage_unavailable".to_owned()),
        }
    }

    #[test]
    fn integrity_says_what_scrubs_found_and_damage_is_a_problem() {
        let mut facts = healthy();
        let report = assess(&settings(), &facts, now());
        assert_eq!(
            find(&report, "Integrity").detail,
            "no full scrub has finished yet; none scheduled (TANGIBLE_SCRUB_INTERVAL_DAYS)"
        );

        facts.integrity = Answer::Found(IntegritySummary {
            last_scrub: Some(scrubbed(IntegrityCheckState::Complete, 3, 0)),
            active_scrub: None,
            damaged_artifacts: 0,
        });
        let line = find(&assess(&settings(), &facts, now()), "Integrity").clone();
        assert_eq!(line.status, CheckStatus::Ok);
        assert!(
            line.detail.starts_with(
                "the last full scrub, finished 2026-10-14T12:00:00Z, read 1200 object(s) \
                 and found nothing wrong"
            ),
            "{}",
            line.detail
        );

        facts.integrity = Answer::Found(IntegritySummary {
            last_scrub: Some(scrubbed(IntegrityCheckState::Complete, 3, 2)),
            active_scrub: Some((40, Some(1200))),
            damaged_artifacts: 2,
        });
        let report = assess(&settings(), &facts, now());
        let line = find(&report, "Integrity");
        assert_eq!(line.status, CheckStatus::Problem);
        assert_eq!(report.status, CheckStatus::Problem);
        assert!(
            line.detail.starts_with("2 artifact(s) damaged"),
            "{}",
            line.detail
        );
        assert!(
            line.detail
                .contains("a full scrub is running, 40 of 1200 objects read")
        );
        assert!(line.detail.contains("found 2 problem(s)"));

        facts.integrity = Answer::Found(IntegritySummary {
            last_scrub: Some(scrubbed(IntegrityCheckState::Failed, 1, 0)),
            active_scrub: None,
            damaged_artifacts: 0,
        });
        let line = find(&assess(&settings(), &facts, now()), "Integrity").clone();
        assert_eq!(line.status, CheckStatus::Warn);
        assert!(
            line.detail
                .contains("failed 2026-10-16T12:00:00Z (storage_unavailable)")
        );

        facts.integrity = Answer::Unavailable("not checked: down".to_owned());
        let line = find(&assess(&settings(), &facts, now()), "Integrity").clone();
        assert_eq!(line.status, CheckStatus::Warn);
    }

    #[test]
    fn a_scheduled_scrub_that_has_not_happened_is_a_warning() {
        let mut weekly = settings();
        weekly.scrub_interval_days = 7;
        let mut facts = healthy();
        let on_time = |facts: &Facts| find(&assess(&weekly, facts, now()), "Integrity").clone();

        // None has ever finished and none is running.
        let line = on_time(&facts);
        assert_eq!(line.status, CheckStatus::Warn);
        assert!(
            line.detail
                .contains("one is queued 7 day(s) after the last ends")
        );
        assert!(line.detail.contains("overdue"));

        // Within the week and a day's grace.
        facts.integrity = Answer::Found(IntegritySummary {
            last_scrub: Some(scrubbed(IntegrityCheckState::Complete, 8, 0)),
            active_scrub: None,
            damaged_artifacts: 0,
        });
        assert_eq!(on_time(&facts).status, CheckStatus::Ok);

        // Past it.
        facts.integrity = Answer::Found(IntegritySummary {
            last_scrub: Some(scrubbed(IntegrityCheckState::Complete, 9, 0)),
            active_scrub: None,
            damaged_artifacts: 0,
        });
        assert_eq!(on_time(&facts).status, CheckStatus::Warn);

        // Past it, and running now.
        facts.integrity = Answer::Found(IntegritySummary {
            last_scrub: Some(scrubbed(IntegrityCheckState::Complete, 9, 0)),
            active_scrub: Some((0, None)),
            damaged_artifacts: 0,
        });
        let line = on_time(&facts);
        assert_eq!(line.status, CheckStatus::Ok);
        assert!(line.detail.contains("a full scrub is queued"));
    }
}
