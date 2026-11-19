// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Restoring a copy of the database from the Backups page (ADR 0026,
//! docs/plans/backups-ui.md, D4).
//!
//! The route checks everything that can refuse a restore before anything
//! changes: the copy, the typed confirmation, the tools, whether the
//! database user may create databases and roles, work under way, and the
//! copy's migrations, read from the file without connecting. Then the
//! server is in maintenance: every API route but the restore's own progress
//! answers 503, and the composition root stops the background runners and
//! runs [`run`]:
//!
//! 1. a copy of the database as it is, first;
//! 2. a new database beside it, with this version's migrations up to the
//!    copy's newest, so its schema is Tangible's own, never the copy's;
//! 3. the copy's rows, loaded as a role that can only insert them
//!    (`tangible_db::restores`), and nothing else from the copy;
//! 4. the foreign keys checked, the rest of the migrations, every session
//!    ended, and the restore check against the library, kept with a record
//!    of the restore in the new database;
//! 5. the two databases swap names.
//!
//! Then the server exits, and Compose starts it on the restored database. A
//! failure before the swap leaves the database as it was, records why in
//! it, and the server exits and starts again the same way.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::Serialize;
use tangible_db::restores::{self, CopyVersion, RecordedMigration, RestoreRecord};
use tangible_domain::SecretString;
use time::OffsetDateTime;
use utoipa::ToSchema;

use crate::backups::{BackupError, Backups, CopyEntry, Operation, Turn, run_limited};
use crate::restore_check::{RestoreCheckSettings, restore_check};
use crate::state::ApiState;

/// How long loading a copy's rows may take.
const LOAD_TIMEOUT: Duration = Duration::from_hours(6);

/// How long reading a copy's contents may take.
const READ_TIMEOUT: Duration = Duration::from_mins(5);

/// The most of a copy's table of contents, or of its migrations' rows, read.
const READ_LIMIT: usize = 8 * 1024 * 1024;

/// How many of each kind of finding the restore check's report lists.
const LISTED: usize = 50;

/// Where a restore is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Letting the background work finish.
    Stopping,
    /// Taking a copy of the database as it is.
    SafetyCopy,
    /// Making the new database's schema.
    Schema,
    /// Loading the copy's rows.
    Rows,
    /// Checking the rows, migrating, and the restore check.
    Checking,
    /// Putting the restored database in place.
    Swapping,
    /// Done: the server is starting again.
    Restarting,
}

impl Step {
    /// Its place, from 1.
    #[must_use]
    pub const fn number(self) -> u8 {
        match self {
            Self::Stopping => 1,
            Self::SafetyCopy => 2,
            Self::Schema => 3,
            Self::Rows => 4,
            Self::Checking => 5,
            Self::Swapping => 6,
            Self::Restarting => 7,
        }
    }
}

/// How many steps there are.
pub const STEPS: u8 = 7;

/// A restore under way, as anyone may see it while the server answers
/// nothing else: no names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Progress {
    /// Where it is.
    pub step: Step,
    /// Its place, from 1.
    pub step_number: u8,
    /// How many steps there are.
    pub steps: u8,
    /// When it began.
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
}

/// Whether the server is restoring, shared by every clone of its state.
#[derive(Debug, Clone, Default)]
pub struct Maintenance {
    inner: Arc<RwLock<Option<Progress>>>,
}

impl Maintenance {
    /// The restore under way, if one is.
    #[must_use]
    pub fn current(&self) -> Option<Progress> {
        self.inner.read().map_or(None, |current| current.clone())
    }

    /// Begin, or move on to `step`.
    pub fn step(&self, step: Step) {
        if let Ok(mut current) = self.inner.write() {
            let started_at = current
                .as_ref()
                .map_or_else(OffsetDateTime::now_utc, |progress| progress.started_at);
            *current = Some(Progress {
                step,
                step_number: step.number(),
                steps: STEPS,
                started_at,
            });
        }
    }

    /// End maintenance.
    pub fn end(&self) {
        if let Ok(mut current) = self.inner.write() {
            *current = None;
        }
    }
}

/// Why a restore was refused before anything changed.
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    /// As copies are refused.
    #[error(transparent)]
    Backup(#[from] BackupError),
    /// The confirmation was not the copy's name.
    #[error("type the copy's name, {0}, to confirm")]
    NotConfirmed(String),
    /// The database user may not create databases and roles.
    #[error(
        "the database user this server connects as may not create databases and roles, \
         which restoring here takes; restore at the machine instead (deploy/README.md, \
         \"Backups\")"
    )]
    NotPermitted,
    /// The database's name leaves no room for its neighbour's.
    #[error("the database's name is longer than {max} bytes, too long to restore beside")]
    NameTooLong {
        /// The longest allowed.
        max: usize,
    },
    /// Work is under way that a restore would cut off.
    #[error("{0}")]
    WorkUnderWay(String),
    /// The copy is not one this version restores.
    #[error("{0}")]
    NotRestorable(String),
}

/// A restore checked and ready to run, holding the turn.
pub struct RestoreOrder {
    turn: Turn,
    /// The copy.
    pub copy: CopyEntry,
    path: PathBuf,
    /// Who asked.
    pub by: String,
    newest: i64,
    contents: String,
    started_at: OffsetDateTime,
}

impl std::fmt::Debug for RestoreOrder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RestoreOrder")
            .field("copy", &self.copy.name)
            .field("by", &self.by)
            .field("newest", &self.newest)
            .finish_non_exhaustive()
    }
}

/// Check everything that can refuse restoring `name`, as `by` confirmed
/// with `confirm`, before anything changes.
///
/// # Errors
///
/// A [`Refusal`] saying why.
pub async fn prepare(
    state: &ApiState,
    backups: &Backups,
    name: &str,
    confirm: &str,
    by: &str,
) -> Result<RestoreOrder, Refusal> {
    let copy = backups.find(name).await?;
    if confirm.trim() != copy.name {
        return Err(Refusal::NotConfirmed(copy.name));
    }
    let tools = backups.tools(state.database()).await;
    if !tools.ready {
        return Err(BackupError::ToolsUnavailable(
            tools
                .problem
                .unwrap_or_else(|| "the dump tools cannot restore this database".to_owned()),
        )
        .into());
    }
    let pool = state.database().pool();
    if !restores::may_restore(pool)
        .await
        .map_err(BackupError::Database)?
    {
        return Err(Refusal::NotPermitted);
    }
    if backups.settings().connection.database.len() > restores::MAX_DATABASE_NAME {
        return Err(Refusal::NameTooLong {
            max: restores::MAX_DATABASE_NAME,
        });
    }
    let busy = restores::work_under_way(pool)
        .await
        .map_err(BackupError::Database)?;
    if !busy.is_empty() {
        return Err(Refusal::WorkUnderWay(under_way(&busy)));
    }
    let turn = backups.begin(Operation::Restoring {
        started_at: OffsetDateTime::now_utc(),
        by: by.to_owned(),
    })?;
    let path = backups.path_of(&copy);
    let contents = read(backups, &["--list", &path.display().to_string()]).await?;
    let rows = read(
        backups,
        &[
            "--data-only",
            "--table=_sqlx_migrations",
            "--file=-",
            &path.display().to_string(),
        ],
    )
    .await?;
    let recorded = recorded_migrations(&rows).map_err(Refusal::NotRestorable)?;
    let newest = match restores::copy_version(&recorded) {
        CopyVersion::Known { newest } => newest,
        CopyVersion::Newer(versions) => {
            return Err(Refusal::NotRestorable(format!(
                "the copy was taken by a later version of Tangible: it has migrations this \
                 one does not, {versions:?}; restore it with that version"
            )));
        }
        CopyVersion::Foreign(why) => {
            return Err(Refusal::NotRestorable(format!(
                "the copy is not of a Tangible database this version restores: {why}"
            )));
        }
    };
    Ok(RestoreOrder {
        turn,
        copy,
        path,
        by: by.to_owned(),
        newest,
        contents,
        started_at: OffsetDateTime::now_utc(),
    })
}

/// What is under way, in words.
fn under_way(busy: &[(&str, String, i64)]) -> String {
    let named: Vec<String> = busy
        .iter()
        .map(|(kind, state, count)| {
            let kind = kind.replace('_', " ");
            if *count == 1 {
                format!("a {kind} {state}")
            } else {
                format!("{count} {kind}s {state}")
            }
        })
        .collect();
    format!(
        "work is under way that a restore would cut off: {}; restore once it is done",
        named.join(", ")
    )
}

/// What `pg_restore` prints of the copy, reading the file alone.
async fn read(backups: &Backups, args: &[&str]) -> Result<String, Refusal> {
    let program = backups.settings().tools.join("pg_restore");
    let output = run_limited(&program, args, &[], READ_TIMEOUT, READ_LIMIT)
        .await
        .map_err(|error| BackupError::Tool {
            tool: "pg_restore",
            detail: error.to_string(),
        })?;
    if !output.success {
        return Err(Refusal::NotRestorable(format!(
            "pg_restore could not read the copy: {}",
            crate::backups::last_lines(&output)
        )));
    }
    if output.truncated {
        return Err(Refusal::NotRestorable(format!(
            "the copy's contents are larger than the {READ_LIMIT} bytes a restore reads"
        )));
    }
    Ok(output.stdout)
}

/// The migrations a copy records, from `pg_restore --data-only
/// --table=_sqlx_migrations --file=-`: its `COPY` block, as text.
///
/// # Errors
///
/// Why it does not read as one.
pub fn recorded_migrations(text: &str) -> Result<Vec<RecordedMigration>, String> {
    let mut lines = text.lines();
    let columns = lines
        .by_ref()
        .find_map(|line| {
            line.strip_prefix("COPY public._sqlx_migrations (")
                .and_then(|rest| rest.strip_suffix(") FROM stdin;"))
        })
        .ok_or("the copy holds no record of its migrations, so it is not a Tangible database")?;
    let columns: Vec<&str> = columns.split(", ").collect();
    let at = |name: &str| {
        columns
            .iter()
            .position(|column| *column == name)
            .ok_or(format!("the copy's record of its migrations has no {name}"))
    };
    let (version_at, success_at, checksum_at) = (at("version")?, at("success")?, at("checksum")?);
    let mut recorded = Vec::new();
    for line in lines {
        if line == "\\." {
            return Ok(recorded);
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let field = |index: usize| {
            fields
                .get(index)
                .copied()
                .ok_or("a row of the copy's migrations is short".to_owned())
        };
        let version = field(version_at)?
            .parse::<i64>()
            .map_err(|_| "a migration's version is not a number".to_owned())?;
        let success = match field(success_at)? {
            "t" => true,
            "f" => false,
            _ => return Err("a migration's outcome is not true or false".to_owned()),
        };
        // bytea in hex, its backslash doubled by COPY's text format.
        let checksum = field(checksum_at)?
            .strip_prefix("\\\\x")
            .and_then(|hex_digits| hex::decode(hex_digits).ok())
            .ok_or("a migration's checksum is not hex".to_owned())?;
        recorded.push(RecordedMigration {
            version,
            success,
            checksum,
        });
    }
    Err("the copy's record of its migrations ends early".to_owned())
}

/// What kind of row-loading entry a line of a copy's table of contents is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DataKind {
    Table,
    Sequence,
}

/// The entries of `contents` that load rows into, or set, the tables and
/// sequences named, as `--use-list` takes them; nothing else.
fn row_entries(contents: &str, tables: &[String], sequences: &[String]) -> Vec<String> {
    contents
        .lines()
        .filter(|line| {
            entry_of(line).is_some_and(|(kind, name)| match kind {
                DataKind::Table => tables.iter().any(|table| table == name),
                DataKind::Sequence => sequences.iter().any(|sequence| sequence == name),
            })
        })
        .map(str::to_owned)
        .collect()
}

/// `3360; 0 16386 TABLE DATA public artifacts tangible`: the entry's kind
/// and name, when it loads a table's rows or sets a sequence in `public`
/// and its name is a plain one.
fn entry_of(line: &str) -> Option<(DataKind, &str)> {
    let (id, rest) = line.split_once("; ")?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut parts = rest.splitn(3, ' ');
    let numbers = [parts.next()?, parts.next()?];
    if !numbers
        .iter()
        .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let tail = parts.next()?;
    let (kind, tail) = if let Some(tail) = tail.strip_prefix("TABLE DATA public ") {
        (DataKind::Table, tail)
    } else {
        (
            DataKind::Sequence,
            tail.strip_prefix("SEQUENCE SET public ")?,
        )
    };
    let (name, owner) = tail.split_once(' ')?;
    let plain = |text: &str| {
        !text.is_empty()
            && text
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    };
    (plain(name) && !owner.is_empty() && !owner.contains(' ')).then_some((kind, name))
}

/// Why a restore failed after it began, which leaves the database as it was.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Failed(pub String);

/// Run the restore `order` describes. The composition root calls this once
/// the background runners have stopped, and exits after, so the server
/// starts again on whichever database is in place.
///
/// # Errors
///
/// [`Failed`] before the swap, the database left as it was and the failure
/// recorded in it.
pub async fn run(order: RestoreOrder, state: &ApiState) -> Result<RestoreRecord, Failed> {
    let maintenance = state.maintenance();
    let Some(backups) = state.backups().cloned() else {
        return Err(Failed("this server keeps no copies".to_owned()));
    };
    let connection = backups.settings().connection.clone();
    let mut role = None;
    let mut safety = None;
    let attempt = attempt(&order, state, &backups, &mut role, &mut safety).await;
    match attempt {
        Ok(record) => {
            maintenance.step(Step::Restarting);
            tracing::info!(
                copy = %order.copy.name,
                by = %order.by,
                safety_copy = ?record.safety_copy,
                "the database was restored from a copy; the server starts again on it"
            );
            Ok(record)
        }
        Err(error) => {
            tracing::error!(copy = %order.copy.name, error = %error, "a restore failed; the database is as it was");
            if let Err(cleanup) = restores::abandon(&connection, role.as_deref()).await {
                tracing::error!(error = ?cleanup, "what a failed restore made could not all be removed");
            }
            let failure = RestoreRecord {
                id: uuid::Uuid::now_v7(),
                requested_by: order.by.clone(),
                copy_name: order.copy.name.clone(),
                safety_copy: safety,
                outcome: "failed".to_owned(),
                error: Some(truncated(&error.0, 4000)),
                report: None,
                started_at: order.started_at,
                finished_at: OffsetDateTime::now_utc(),
            };
            if let Err(recording) =
                restores::record(&connection, &connection.database, &failure).await
            {
                tracing::error!(error = ?recording, "a failed restore could not be recorded");
            }
            maintenance.step(Step::Restarting);
            Err(error)
        }
    }
}

async fn attempt(
    order: &RestoreOrder,
    state: &ApiState,
    backups: &Backups,
    role: &mut Option<String>,
    safety: &mut Option<String>,
) -> Result<RestoreRecord, Failed> {
    let maintenance = state.maintenance();
    let connection = backups.settings().connection.clone();
    let failed = |what: &str| {
        let what = what.to_owned();
        move |error: tangible_db::DbError| Failed(format!("{what}: {error}"))
    };

    maintenance.step(Step::SafetyCopy);
    let before = backups
        .take_before_restore(&order.turn)
        .await
        .map_err(|error| Failed(format!("the copy of the database as it is failed: {error}")))?;
    *safety = Some(before.name.clone());

    maintenance.step(Step::Schema);
    let restoring = restores::create_restoring(&connection)
        .await
        .map_err(failed("making the database to restore into"))?;
    restores::migrate_to(&connection, &restoring, order.newest)
        .await
        .map_err(failed("making its schema"))?;

    maintenance.step(Step::Rows);
    let password = random_password().map_err(Failed)?;
    let loader = restores::prepare_loader(&connection, &restoring, password)
        .await
        .map_err(failed("readying it for the copy's rows"))?;
    *role = Some(loader.role.clone());
    let entries = row_entries(&order.contents, &loader.tables, &loader.sequences);
    load_rows(backups, order, &restoring, &loader, &entries).await?;
    restores::finish_loader(&connection, &restoring, &loader)
        .await
        .map_err(failed("the copy's rows do not hold together"))?;
    *role = None;

    maintenance.step(Step::Checking);
    restores::migrate_all(&connection, &restoring)
        .await
        .map_err(failed("migrating the restored database"))?;
    let ended = restores::end_sessions(&connection, &restoring)
        .await
        .map_err(failed("ending its sessions"))?;
    let report = match state.manifests() {
        Some(manifests) => {
            let restored = restores::open(&connection, &restoring)
                .await
                .map_err(failed("opening the restored database"))?;
            let report = restore_check(
                &restored,
                manifests.objects(),
                manifests,
                RestoreCheckSettings {
                    register: false,
                    list: LISTED,
                },
            )
            .await
            .map_err(|error| Failed(format!("the restore check failed: {error}")))?;
            restored.pool().close().await;
            serde_json::to_value(&report).ok()
        }
        None => None,
    };
    let record = RestoreRecord {
        id: uuid::Uuid::now_v7(),
        requested_by: order.by.clone(),
        copy_name: order.copy.name.clone(),
        safety_copy: Some(before.name.clone()),
        outcome: "restored".to_owned(),
        error: None,
        report,
        started_at: order.started_at,
        finished_at: OffsetDateTime::now_utc(),
    };
    restores::record(&connection, &restoring, &record)
        .await
        .map_err(failed("recording the restore"))?;
    tracing::info!(
        sessions_ended = ended,
        rows = entries.len(),
        "the restored database is ready"
    );

    maintenance.step(Step::Swapping);
    state.database().pool().close().await;
    if let Some(left) = restores::swap(&connection)
        .await
        .map_err(failed("putting the restored database in place"))?
    {
        tracing::warn!(database = %left, "drop the database the restore replaced once you are sure of it");
    }
    Ok(record)
}

/// Load the copy's rows for `entries` as the loader's role.
async fn load_rows(
    backups: &Backups,
    order: &RestoreOrder,
    restoring: &str,
    loader: &restores::Loader,
    entries: &[String],
) -> Result<(), Failed> {
    let settings = backups.settings();
    let list = settings
        .dir
        .join(format!(".server-restore-{}.list", uuid::Uuid::now_v7()));
    let mut text = entries.join("\n");
    text.push('\n');
    tokio::fs::write(&list, text)
        .await
        .map_err(|error| Failed(format!("writing the restore's list of entries: {error}")))?;
    let list_arg = format!("--use-list={}", list.display());
    let dbname_arg = format!("--dbname={restoring}");
    let path_arg = order.path.display().to_string();
    let env = settings
        .connection
        .environment(&loader.role, Some(&loader.password), restoring);
    let ran = run_limited(
        &settings.tools.join("pg_restore"),
        &[
            "--data-only",
            "--single-transaction",
            "--exit-on-error",
            "--no-password",
            &list_arg,
            &dbname_arg,
            &path_arg,
        ],
        &env,
        LOAD_TIMEOUT,
        64 * 1024,
    )
    .await;
    let _ = tokio::fs::remove_file(&list).await;
    let output = ran.map_err(|error| Failed(format!("pg_restore did not run: {error}")))?;
    if output.success {
        Ok(())
    } else {
        Err(Failed(format!(
            "loading the copy's rows failed: {}",
            crate::backups::last_lines(&output)
        )))
    }
}

fn random_password() -> Result<SecretString, String> {
    let mut bytes = [0_u8; 24];
    getrandom::fill(&mut bytes).map_err(|_| "no randomness for the restore's role".to_owned())?;
    Ok(SecretString::from(hex::encode(bytes)))
}

fn truncated(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    const CONTENTS: &str = "\
;
; Archive created at 2026-11-18 03:00:00 UTC
;     dbname: tangible
;
216; 1259 16386 TABLE public _sqlx_migrations tangible
230; 1255 16400 FUNCTION public touch_updated_at() tangible
3360; 0 16386 TABLE DATA public _sqlx_migrations tangible
3361; 0 16390 TABLE DATA public artifacts tangible
3362; 0 16391 TABLE DATA public users tangible
3363; 0 16392 TABLE DATA public extra_table tangible
3364; 0 0 SEQUENCE SET public burn_events_id_seq tangible
3365; 0 0 SEQUENCE SET public other_seq tangible
3366; 0 16393 TABLE DATA other artifacts tangible
3367; 0 16394 TABLE DATA public \"Odd\" tangible
3368; 2620 16395 TRIGGER public artifacts artifacts_touch tangible
3369; 0 0 BLOB - 16396 tangible
";

    #[test]
    fn only_rows_for_this_versions_tables_and_sequences_are_loaded() {
        let tables = vec!["artifacts".to_owned(), "users".to_owned()];
        let sequences = vec!["burn_events_id_seq".to_owned()];
        assert_eq!(
            row_entries(CONTENTS, &tables, &sequences),
            [
                "3361; 0 16390 TABLE DATA public artifacts tangible",
                "3362; 0 16391 TABLE DATA public users tangible",
                "3364; 0 0 SEQUENCE SET public burn_events_id_seq tangible",
            ]
        );
    }

    #[test]
    fn an_entry_is_read_only_in_its_one_form() {
        assert_eq!(
            entry_of("3361; 0 16390 TABLE DATA public artifacts tangible"),
            Some((DataKind::Table, "artifacts"))
        );
        for line in [
            "x; 0 16390 TABLE DATA public artifacts tangible",
            "3361; a 16390 TABLE DATA public artifacts tangible",
            "3361; 0 16390 TABLE DATA public artifacts",
            "3361; 0 16390 TABLE DATA public artifacts tangible extra",
            "3361; 0 16390 TABLE public artifacts tangible",
            ";3361; 0 16390 TABLE DATA public artifacts tangible",
        ] {
            assert_eq!(entry_of(line), None, "{line}");
        }
    }

    #[test]
    fn a_copys_migrations_are_read_from_its_copy_block() {
        let text = "\
--
-- PostgreSQL database dump
--
SET statement_timeout = 0;

COPY public._sqlx_migrations (version, description, installed_on, success, checksum, execution_time) FROM stdin;
20260805000001\tinitial schema\t2026-08-05 12:00:00+00\tt\t\\\\x0aff\t1234
20260805000002\tsecond\t2026-08-06 12:00:00+00\tf\t\\\\x00\t99
\\.

";
        assert_eq!(
            recorded_migrations(text).unwrap(),
            [
                RecordedMigration {
                    version: 20_260_805_000_001,
                    success: true,
                    checksum: vec![0x0a, 0xff],
                },
                RecordedMigration {
                    version: 20_260_805_000_002,
                    success: false,
                    checksum: vec![0x00],
                },
            ]
        );
        assert!(recorded_migrations("SET x = 1;\n").is_err());
        let unended = "COPY public._sqlx_migrations (version, success, checksum) FROM stdin;\n1\tt\t\\\\x00\n";
        assert!(
            recorded_migrations(unended)
                .unwrap_err()
                .contains("ends early")
        );
        let odd = "COPY public._sqlx_migrations (version, success, checksum) FROM stdin;\n1\tyes\t\\\\x00\n\\.\n";
        assert!(recorded_migrations(odd).is_err());
    }

    #[test]
    fn work_under_way_is_named() {
        assert_eq!(
            under_way(&[
                ("burn", "writing".to_owned(), 1),
                ("import", "hashing".to_owned(), 2)
            ]),
            "work is under way that a restore would cut off: a burn writing, 2 imports \
             hashing; restore once it is done"
        );
    }

    #[test]
    fn maintenance_keeps_its_start_through_the_steps() {
        let maintenance = Maintenance::default();
        assert!(maintenance.current().is_none());
        maintenance.step(Step::Stopping);
        let started = maintenance.current().unwrap().started_at;
        maintenance.step(Step::Rows);
        let now = maintenance.current().unwrap();
        assert_eq!(now.started_at, started);
        assert_eq!((now.step_number, now.steps), (4, STEPS));
        maintenance.end();
        assert!(maintenance.current().is_none());
    }
}
