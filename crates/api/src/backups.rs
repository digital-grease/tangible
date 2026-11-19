// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Copies of the database, taken, listed, fetched, added and removed by the
//! server (ADR 0026, docs/plans/backups-ui.md).
//!
//! A copy is a file in the backups volume, which the `backup` Compose
//! service writes too: before every start it takes one named as these are,
//! `tangible-YYYYMMDDTHHMMSSZ.dump`, and both keep the newest
//! `TANGIBLE_BACKUPS_KEPT` of those. A copy uploaded through the server is
//! `uploaded-YYYYMMDDTHHMMSSZ.dump`, and anything else ending `.dump` was put
//! there at the machine; neither is ever pruned. The volume's root is the
//! image user's group with the setgid bit, and every copy is written
//! readable by that group alone.
//!
//! `pg_dump` and `pg_restore` run as every other tool does (AGENTS.md 8):
//! fixed arguments, the password in the child's environment and nowhere
//! else, output bounded, a time limit. A name from a request is only ever
//! matched against the files listed, never joined onto a path.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use time::OffsetDateTime;
use time::macros::format_description;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use utoipa::ToSchema;

/// How a copy's file is written: readable by the image user's group, which
/// the `backup` service shares, and by nobody else.
const COPY_MODE: u32 = 0o640;

/// How long a copy may take.
const DUMP_TIMEOUT: Duration = Duration::from_hours(2);

/// How long reading a copy's table of contents may take.
const LIST_TIMEOUT: Duration = Duration::from_mins(2);

/// How long asking a tool its version may take.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// How much of a tool's output is kept, per stream.
const OUTPUT_LIMIT: usize = 64 * 1024;

/// What every PostgreSQL custom-format archive begins with.
const ARCHIVE_MAGIC: &[u8] = b"PGDMP";

/// How large an uploaded copy may be unless configured: 4 GiB.
pub const DEFAULT_MAX_UPLOAD_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Where the dump tools are in the image: Debian's PostgreSQL 17 client.
pub const DEFAULT_TOOLS_DIR: &str = "/usr/lib/postgresql/17/bin";

pub use tangible_db::restores::ToolConnection;

/// What the server needs to keep copies.
#[derive(Debug, Clone)]
pub struct BackupSettings {
    /// The backups volume.
    pub dir: PathBuf,
    /// How many of the copies Tangible names are kept, newest first
    /// (`TANGIBLE_BACKUPS_KEPT`). 0 takes none.
    pub kept: u32,
    /// The directory holding `pg_dump` and `pg_restore`.
    pub tools: PathBuf,
    /// How they reach the database.
    pub connection: ToolConnection,
    /// The largest copy an upload may be.
    pub max_upload_bytes: u64,
}

/// What can go wrong with copies.
#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    /// `TANGIBLE_BACKUPS_KEPT` is 0.
    #[error("copies are turned off (TANGIBLE_BACKUPS_KEPT is 0)")]
    Off,
    /// A copy is being taken or added, or a restore is under way.
    #[error("{0} is under way")]
    Busy(&'static str),
    /// No copy of that name.
    #[error("no copy is named that")]
    NotFound,
    /// The tools are missing, or older than the database.
    #[error("{0}")]
    ToolsUnavailable(String),
    /// The backups volume cannot be written.
    #[error("the backups volume cannot be written: {0}")]
    NotWritable(String),
    /// A file could not be read or written.
    #[error("{what}: {source}")]
    Io {
        /// What was being done.
        what: &'static str,
        /// Why it failed.
        #[source]
        source: std::io::Error,
    },
    /// A tool ran and failed.
    #[error("{tool} failed: {detail}")]
    Tool {
        /// Which.
        tool: &'static str,
        /// Its last lines.
        detail: String,
    },
    /// An upload that is not a PostgreSQL archive.
    #[error("{0}")]
    NotACopy(String),
    /// An upload larger than allowed.
    #[error("the copy is larger than the {0} bytes this server accepts")]
    TooLarge(u64),
    /// An upload whose connection ended before it was complete.
    #[error("the upload ended before it was complete")]
    Interrupted,
    /// The database could not be asked.
    #[error("the database could not be asked")]
    Database(#[from] tangible_db::DbError),
}

fn io(what: &'static str) -> impl FnOnce(std::io::Error) -> BackupError {
    move |source| BackupError::Io { what, source }
}

/// Who took or added a copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CopyKind {
    /// Taken by Tangible, at a start, on the schedule, or asked for: counted
    /// and pruned.
    Taken,
    /// Uploaded through the server: never pruned.
    Uploaded,
    /// Put in the volume at the machine: never pruned.
    Added,
}

/// One copy in the backups volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct CopyEntry {
    /// The file's name, which names it in every route.
    pub name: String,
    /// Who took or added it.
    pub kind: CopyKind,
    /// When it was taken, from its name; for a copy added at the machine,
    /// when its file was last written.
    #[serde(with = "time::serde::rfc3339")]
    pub taken_at: OffsetDateTime,
    /// Its size in bytes.
    pub size_bytes: u64,
}

/// What is under way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Operation {
    /// A copy being taken.
    Taking {
        /// Since when.
        #[serde(with = "time::serde::rfc3339")]
        started_at: OffsetDateTime,
        /// By whom: a username, or `schedule`.
        by: String,
    },
    /// A copy being uploaded.
    Uploading {
        /// Since when.
        #[serde(with = "time::serde::rfc3339")]
        started_at: OffsetDateTime,
        /// By whom.
        by: String,
    },
    /// A copy being restored.
    Restoring {
        /// Since when.
        #[serde(with = "time::serde::rfc3339")]
        started_at: OffsetDateTime,
        /// By whom.
        by: String,
    },
}

/// How the last copy the server took or added since it started went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Outcome {
    /// When it ended.
    #[serde(with = "time::serde::rfc3339")]
    pub finished_at: OffsetDateTime,
    /// The copy, when there is one.
    pub name: Option<String>,
    /// Why it failed, when it did.
    pub error: Option<String>,
}

/// Whether the dump tools can copy this database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ToolsStatus {
    /// Whether copies can be taken and restored.
    pub ready: bool,
    /// The tools' major version, when they answer.
    pub tools_version: Option<u32>,
    /// The database server's major version, when it answers.
    pub database_version: Option<u32>,
    /// What stands in the way, when something does.
    pub problem: Option<String>,
}

/// The copies, and whether the server can write more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    /// Newest first.
    pub copies: Vec<CopyEntry>,
    /// Whether the server can write into the volume.
    pub writable: bool,
}

/// Copies of the database, shared by every clone of the server's state.
#[derive(Debug, Clone)]
pub struct Backups {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    settings: BackupSettings,
    /// Held while a copy is taken or added (and, later, restored): one at a
    /// time.
    busy: Arc<tokio::sync::Mutex<()>>,
    operation: Mutex<Option<Operation>>,
    last: Mutex<Option<Outcome>>,
    /// The tools' major version, asked once.
    tools_version: tokio::sync::OnceCell<Result<u32, String>>,
    /// Wakes the schedule when it changes.
    schedule_nudge: tokio::sync::Notify,
}

/// A copy, or an upload, in hand: holding it keeps anything else from
/// starting.
pub struct Turn {
    _guard: tokio::sync::OwnedMutexGuard<()>,
    backups: Backups,
}

impl std::fmt::Debug for Turn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Turn").finish_non_exhaustive()
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        if let Ok(mut operation) = self.backups.inner.operation.lock() {
            *operation = None;
        }
    }
}

impl Backups {
    /// Copies kept as `settings` says.
    #[must_use]
    pub fn new(settings: BackupSettings) -> Self {
        Self {
            inner: Arc::new(Inner {
                settings,
                busy: Arc::new(tokio::sync::Mutex::new(())),
                operation: Mutex::new(None),
                last: Mutex::new(None),
                tools_version: tokio::sync::OnceCell::new(),
                schedule_nudge: tokio::sync::Notify::new(),
            }),
        }
    }

    /// How they are kept.
    #[must_use]
    pub fn settings(&self) -> &BackupSettings {
        &self.inner.settings
    }

    /// Tell the schedule it changed.
    pub fn schedule_changed(&self) {
        self.inner.schedule_nudge.notify_one();
    }

    /// What is under way, if anything.
    #[must_use]
    pub fn operation(&self) -> Option<Operation> {
        self.inner
            .operation
            .lock()
            .map_or(None, |operation| operation.clone())
    }

    /// How the last copy taken or added since the server started went.
    #[must_use]
    pub fn last_outcome(&self) -> Option<Outcome> {
        self.inner.last.lock().map_or(None, |last| last.clone())
    }

    fn record(&self, outcome: Outcome) {
        if let Ok(mut last) = self.inner.last.lock() {
            *last = Some(outcome);
        }
    }

    /// Take the turn for `operation`, or say what holds it.
    ///
    /// # Errors
    ///
    /// [`BackupError::Busy`] while something else holds it.
    pub fn begin(&self, operation: Operation) -> Result<Turn, BackupError> {
        let guard = self
            .inner
            .busy
            .clone()
            .try_lock_owned()
            .map_err(|_| BackupError::Busy(self.busy_with()))?;
        if let Ok(mut current) = self.inner.operation.lock() {
            *current = Some(operation);
        }
        Ok(Turn {
            _guard: guard,
            backups: self.clone(),
        })
    }

    fn busy_with(&self) -> &'static str {
        match self.operation() {
            Some(Operation::Uploading { .. }) => "an upload",
            Some(Operation::Restoring { .. }) => "a restore",
            Some(Operation::Taking { .. }) | None => "a copy",
        }
    }

    /// The copies in the volume, newest first, and whether the server can
    /// write there.
    ///
    /// # Errors
    ///
    /// [`BackupError::Io`] when the volume cannot be read.
    pub async fn list(&self) -> Result<Listing, BackupError> {
        let dir = &self.inner.settings.dir;
        let mut copies = Vec::new();
        let mut entries = tokio::fs::read_dir(dir)
            .await
            .map_err(io("reading the backups volume"))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(io("reading the backups volume"))?
        {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !is_copy_name(&name) {
                continue;
            }
            // Not followed: a link is not a copy.
            let Ok(metadata) = tokio::fs::symlink_metadata(entry.path()).await else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            copies.push(entry_for(name, &metadata));
        }
        copies.sort_by(|a, b| {
            b.taken_at
                .cmp(&a.taken_at)
                .then_with(|| b.name.cmp(&a.name))
        });
        Ok(Listing {
            copies,
            writable: writable(dir).await,
        })
    }

    /// The copy named `name`, if the volume holds one.
    ///
    /// # Errors
    ///
    /// [`BackupError::NotFound`], or [`BackupError::Io`] when the volume
    /// cannot be read.
    pub async fn find(&self, name: &str) -> Result<CopyEntry, BackupError> {
        if !is_copy_name(name) {
            return Err(BackupError::NotFound);
        }
        self.list()
            .await?
            .copies
            .into_iter()
            .find(|copy| copy.name == name)
            .ok_or(BackupError::NotFound)
    }

    /// The path of a copy [`Self::find`] found.
    #[must_use]
    pub fn path_of(&self, copy: &CopyEntry) -> PathBuf {
        self.inner.settings.dir.join(&copy.name)
    }

    /// Open a copy to read, refusing one replaced by anything but a plain
    /// file since it was listed.
    ///
    /// # Errors
    ///
    /// [`BackupError::NotFound`] or [`BackupError::Io`].
    pub async fn open(&self, name: &str) -> Result<(tokio::fs::File, CopyEntry), BackupError> {
        let copy = self.find(name).await?;
        let path = self.path_of(&copy);
        let listed = tokio::fs::symlink_metadata(&path)
            .await
            .map_err(|_| BackupError::NotFound)?;
        let file = tokio::fs::File::open(&path)
            .await
            .map_err(io("opening the copy"))?;
        let opened = file.metadata().await.map_err(io("opening the copy"))?;
        if !same_file(&listed, &opened) || !opened.is_file() {
            return Err(BackupError::NotFound);
        }
        Ok((file, copy))
    }

    /// Remove a copy.
    ///
    /// # Errors
    ///
    /// [`BackupError::NotFound`], [`BackupError::Busy`] while a copy is
    /// taken or added, or [`BackupError::Io`].
    pub async fn delete(&self, name: &str, by: &str) -> Result<CopyEntry, BackupError> {
        let _turn = self.begin(Operation::Taking {
            started_at: OffsetDateTime::now_utc(),
            by: by.to_owned(),
        })?;
        let copy = self.find(name).await?;
        tokio::fs::remove_file(self.path_of(&copy))
            .await
            .map_err(io("removing the copy"))?;
        sync_dir(&self.inner.settings.dir).await;
        Ok(copy)
    }

    /// Whether the tools can copy `database`: present, and no older than its
    /// server.
    pub async fn tools(&self, database: &tangible_db::Database) -> ToolsStatus {
        let tools = self
            .inner
            .tools_version
            .get_or_init(|| async {
                let program = self.inner.settings.tools.join("pg_dump");
                match run(&program, &["--version"], &[], VERSION_TIMEOUT).await {
                    Ok(output) if output.success => {
                        major_version(&output.stdout).ok_or_else(|| {
                            format!(
                                "pg_dump answered its version unreadably: {}",
                                output.stdout.trim()
                            )
                        })
                    }
                    Ok(output) => Err(format!("pg_dump did not run: {}", last_lines(&output))),
                    Err(error) => Err(format!(
                        "the PostgreSQL client tools are not at {}: {error}",
                        self.inner.settings.tools.display()
                    )),
                }
            })
            .await
            .clone();
        let database_version = tangible_db::backups::server_major_version(database.pool())
            .await
            .ok();
        let (ready, problem) = match (&tools, database_version) {
            (Err(problem), _) => (false, Some(problem.clone())),
            (Ok(_), None) => (
                false,
                Some("the database did not say its version".to_owned()),
            ),
            (Ok(tools), Some(server)) if server > *tools => (
                false,
                Some(format!(
                    "the database is PostgreSQL {server}, newer than this image's tools \
                     ({tools}), which cannot copy it"
                )),
            ),
            (Ok(_), Some(_)) => (true, None),
        };
        ToolsStatus {
            ready,
            tools_version: tools.ok(),
            database_version,
            problem,
        }
    }

    /// Take a copy now, holding `turn`, then prune the oldest of Tangible's
    /// beyond the number kept.
    ///
    /// # Errors
    ///
    /// [`BackupError::Off`], [`BackupError::NotWritable`],
    /// [`BackupError::Tool`] or [`BackupError::Io`].
    pub async fn take(&self, _turn: &Turn) -> Result<CopyEntry, BackupError> {
        let outcome = self.take_now().await;
        self.record(Outcome {
            finished_at: OffsetDateTime::now_utc(),
            name: outcome.as_ref().ok().map(|copy| copy.name.clone()),
            error: outcome.as_ref().err().map(ToString::to_string),
        });
        outcome
    }

    /// Take a copy of the database as it is before a restore replaces it,
    /// holding `turn`: whatever the number kept, and pruning nothing, so the
    /// copy being restored is not the one that goes to make room.
    ///
    /// # Errors
    ///
    /// As [`Self::take`].
    pub async fn take_before_restore(&self, _turn: &Turn) -> Result<CopyEntry, BackupError> {
        self.dump().await
    }

    async fn take_now(&self) -> Result<CopyEntry, BackupError> {
        if self.inner.settings.kept == 0 {
            return Err(BackupError::Off);
        }
        let copy = self.dump().await?;
        self.prune().await;
        Ok(copy)
    }

    async fn dump(&self) -> Result<CopyEntry, BackupError> {
        let settings = &self.inner.settings;
        let (name, stamp) = free_name(&settings.dir, "tangible").await;
        let part = settings.dir.join(format!(".server-{stamp}.part"));
        create_private(&part).await?;
        let file_arg = format!("--file={}", part.display());
        let env = settings.connection.environment(
            &settings.connection.user,
            settings.connection.password.as_ref(),
            &settings.connection.database,
        );
        let ran = run(
            &settings.tools.join("pg_dump"),
            &["--format=custom", "--no-password", &file_arg],
            &env,
            DUMP_TIMEOUT,
        )
        .await;
        let output = match ran {
            Ok(output) => output,
            Err(error) => {
                let _ = tokio::fs::remove_file(&part).await;
                return Err(BackupError::Tool {
                    tool: "pg_dump",
                    detail: error.to_string(),
                });
            }
        };
        if !output.success {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(BackupError::Tool {
                tool: "pg_dump",
                detail: last_lines(&output),
            });
        }
        let path = settings.dir.join(&name);
        finish(&part, &path).await?;
        let metadata = tokio::fs::symlink_metadata(&path)
            .await
            .map_err(io("reading the copy taken"))?;
        Ok(entry_for(name, &metadata))
    }

    /// Remove the oldest of Tangible's copies beyond the number kept. Only
    /// names it takes count, and only they go.
    async fn prune(&self) {
        let Ok(listing) = self.list().await else {
            return;
        };
        let kept = usize::try_from(self.inner.settings.kept).unwrap_or(usize::MAX);
        let mut taken: Vec<&CopyEntry> = listing
            .copies
            .iter()
            .filter(|copy| copy.kind == CopyKind::Taken)
            .collect();
        // By name, as the `backup` service sorts them: the name is the time.
        taken.sort_by(|a, b| b.name.cmp(&a.name));
        for old in taken.into_iter().skip(kept) {
            if let Err(error) =
                tokio::fs::remove_file(self.inner.settings.dir.join(&old.name)).await
            {
                tracing::warn!(copy = %old.name, error = %error, "an old copy could not be removed");
            }
        }
        sync_dir(&self.inner.settings.dir).await;
    }

    /// Keep an uploaded copy, holding `turn`: written beside the others as
    /// `uploaded-YYYYMMDDTHHMMSSZ.dump` once it is whole, at most the size
    /// allowed, and read by `pg_restore` as an archive.
    ///
    /// # Errors
    ///
    /// [`BackupError::TooLarge`], [`BackupError::NotACopy`],
    /// [`BackupError::Interrupted`], [`BackupError::NotWritable`] or
    /// [`BackupError::Io`].
    pub async fn store_upload<S, E>(&self, _turn: &Turn, body: S) -> Result<CopyEntry, BackupError>
    where
        S: futures_util::Stream<Item = Result<axum::body::Bytes, E>> + Send,
    {
        use futures_util::StreamExt as _;
        let settings = &self.inner.settings;
        let part = settings
            .dir
            .join(format!(".server-upload-{}.part", uuid::Uuid::now_v7()));
        let mut file = create_private(&part).await?;
        let mut received = 0_u64;
        let mut head: Vec<u8> = Vec::with_capacity(ARCHIVE_MAGIC.len());
        let mut body = std::pin::pin!(body);
        let written: Result<(), BackupError> = async {
            while let Some(chunk) = body.next().await {
                let chunk = chunk.map_err(|_| BackupError::Interrupted)?;
                let chunk = chunk.as_ref();
                received = received.saturating_add(chunk.len() as u64);
                if received > settings.max_upload_bytes {
                    return Err(BackupError::TooLarge(settings.max_upload_bytes));
                }
                if head.len() < ARCHIVE_MAGIC.len() {
                    let wanted = ARCHIVE_MAGIC.len() - head.len();
                    head.extend_from_slice(&chunk[..wanted.min(chunk.len())]);
                    if head.len() == ARCHIVE_MAGIC.len() && head != ARCHIVE_MAGIC {
                        return Err(not_an_archive());
                    }
                }
                file.write_all(chunk)
                    .await
                    .map_err(io("writing the upload"))?;
            }
            if head != ARCHIVE_MAGIC {
                return Err(not_an_archive());
            }
            file.flush().await.map_err(io("writing the upload"))?;
            file.sync_all().await.map_err(io("writing the upload"))?;
            Ok(())
        }
        .await;
        drop(file);
        if let Err(error) = written {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(error);
        }
        if let Err(error) = self.list_contents(&part).await {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(match error {
                BackupError::Tool { detail, .. } => BackupError::NotACopy(format!(
                    "pg_restore could not read it as a PostgreSQL archive: {detail}"
                )),
                other => other,
            });
        }
        let (name, _) = free_name(&settings.dir, "uploaded").await;
        let path = settings.dir.join(&name);
        finish(&part, &path).await?;
        let metadata = tokio::fs::symlink_metadata(&path)
            .await
            .map_err(io("reading the copy uploaded"))?;
        let copy = entry_for(name, &metadata);
        self.record(Outcome {
            finished_at: OffsetDateTime::now_utc(),
            name: Some(copy.name.clone()),
            error: None,
        });
        Ok(copy)
    }

    /// A copy's table of contents, as `pg_restore --list` prints it. Reads
    /// the file only: nothing connects to a database.
    ///
    /// # Errors
    ///
    /// [`BackupError::Tool`] when `pg_restore` cannot read it.
    pub async fn list_contents(&self, path: &Path) -> Result<String, BackupError> {
        let program = self.inner.settings.tools.join("pg_restore");
        let path_arg = path.display().to_string();
        let output = run(&program, &["--list", &path_arg], &[], LIST_TIMEOUT)
            .await
            .map_err(|error| BackupError::Tool {
                tool: "pg_restore",
                detail: error.to_string(),
            })?;
        if output.success {
            Ok(output.stdout)
        } else {
            Err(BackupError::Tool {
                tool: "pg_restore",
                detail: last_lines(&output),
            })
        }
    }
}

// --- the schedule (docs/plans/backups-ui.md, D5) -------------------------------------

/// The longest the schedule sleeps before it looks again, so a clock set
/// forward or a missed wake costs at most this.
const SCHEDULE_RECHECK: Duration = Duration::from_mins(15);

/// The next time at `minute_of_day` past midnight UTC after `now`: today's,
/// unless it has passed, then tomorrow's.
#[must_use]
pub fn next_due(now: OffsetDateTime, minute_of_day: u16) -> OffsetDateTime {
    let minute = minute_of_day.min(24 * 60 - 1);
    let at = time::Time::from_hms(
        u8::try_from(minute / 60).unwrap_or(0),
        u8::try_from(minute % 60).unwrap_or(0),
        0,
    )
    .unwrap_or(time::Time::MIDNIGHT);
    let today = now.to_offset(time::UtcOffset::UTC).replace_time(at);
    if today > now {
        today
    } else {
        today + time::Duration::days(1)
    }
}

/// Takes a copy each day at the time the schedule says.
#[derive(Debug, Clone)]
pub struct BackupScheduler {
    backups: Backups,
    database: tangible_db::Database,
}

impl BackupScheduler {
    /// The schedule for `backups`, read from `database`.
    #[must_use]
    pub fn new(backups: Backups, database: tangible_db::Database) -> Self {
        Self { backups, database }
    }

    /// Run until `shutdown` resolves.
    pub async fn run<S>(self, shutdown: S)
    where
        S: Future<Output = ()> + Send,
    {
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            let schedule = match tangible_db::backups::schedule(self.database.pool()).await {
                Ok(schedule) => schedule,
                Err(error) => {
                    tracing::warn!(error = ?error, "the copy schedule could not be read");
                    tangible_db::backups::Schedule {
                        enabled: false,
                        ..tangible_db::backups::Schedule::default()
                    }
                }
            };
            let now = OffsetDateTime::now_utc();
            let due = schedule
                .enabled
                .then(|| next_due(now, schedule.minute_of_day));
            let wait = due.map_or(SCHEDULE_RECHECK, |due| {
                Duration::try_from(due - now)
                    .unwrap_or(Duration::ZERO)
                    .min(SCHEDULE_RECHECK)
            });
            tokio::select! {
                () = &mut shutdown => return,
                () = self.backups.inner.schedule_nudge.notified() => continue,
                () = tokio::time::sleep(wait) => {}
            }
            if due.is_some_and(|due| OffsetDateTime::now_utc() >= due) {
                self.take_one().await;
            }
        }
    }

    /// The day's copy, unless copies are off or something else is under
    /// way, which is said and not made up later: the next start takes one.
    async fn take_one(&self) {
        if self.backups.settings().kept == 0 {
            tracing::info!("no copy taken on the schedule: TANGIBLE_BACKUPS_KEPT is 0");
            return;
        }
        let tools = self.backups.tools(&self.database).await;
        let taken = if tools.ready {
            match self.backups.begin(Operation::Taking {
                started_at: OffsetDateTime::now_utc(),
                by: "schedule".to_owned(),
            }) {
                Ok(turn) => self.backups.take(&turn).await,
                Err(error) => {
                    tracing::info!(error = %error, "no copy taken on the schedule");
                    return;
                }
            }
        } else {
            let problem = tools
                .problem
                .unwrap_or_else(|| "the dump tools cannot copy this database".to_owned());
            self.backups.record(Outcome {
                finished_at: OffsetDateTime::now_utc(),
                name: None,
                error: Some(problem.clone()),
            });
            Err(BackupError::ToolsUnavailable(problem))
        };
        let (name, succeeded, metadata) = match &taken {
            Ok(copy) => {
                tracing::info!(copy = %copy.name, bytes = copy.size_bytes, "a copy was taken on the schedule");
                (
                    copy.name.as_str(),
                    true,
                    serde_json::json!({ "size_bytes": copy.size_bytes }),
                )
            }
            Err(error) => {
                tracing::warn!(error = %error, "the copy the schedule takes failed");
                (
                    "-",
                    false,
                    serde_json::json!({ "error": error.to_string() }),
                )
            }
        };
        if let Err(error) = tangible_db::backups::audit(
            self.database.pool(),
            tangible_db::backups::Actor::Schedule,
            "backup.taken",
            name,
            succeeded,
            metadata,
        )
        .await
        {
            tracing::error!(error = ?error, "a copy's audit record could not be written");
        }
    }
}

fn not_an_archive() -> BackupError {
    BackupError::NotACopy(
        "it is not a PostgreSQL custom-format archive (pg_dump --format=custom), which every \
         copy Tangible takes is"
            .to_owned(),
    )
}

/// Whether `name` could be a copy: a plain name ending `.dump`, of letters,
/// digits, `-`, `_` and `.`, not hidden, at most 200 bytes.
#[must_use]
pub fn is_copy_name(name: &str) -> bool {
    name.len() <= 200
        && name.len() > ".dump".len()
        // Exactly `.dump`, as every copy Tangible names: the case is not
        // folded, so `x.DUMP` is not one.
        && name.as_bytes().ends_with(b".dump")
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// The time in a name Tangible gives, and whose: `tangible-` or `uploaded-`
/// then `YYYYMMDDTHHMMSSZ.dump`.
fn stamped(name: &str) -> Option<(CopyKind, OffsetDateTime)> {
    let (kind, rest) = match name.strip_prefix("tangible-") {
        Some(rest) => (CopyKind::Taken, rest),
        None => (CopyKind::Uploaded, name.strip_prefix("uploaded-")?),
    };
    let stamp = rest.strip_suffix(".dump")?;
    if stamp.len() != 16 {
        return None;
    }
    let format = format_description!("[year][month][day]T[hour][minute][second]Z");
    let at = time::PrimitiveDateTime::parse(stamp, &format).ok()?;
    Some((kind, at.assume_utc()))
}

fn stamp_of(at: OffsetDateTime) -> String {
    let format = format_description!("[year][month][day]T[hour][minute][second]Z");
    at.format(&format)
        .unwrap_or_else(|_| "00000000T000000Z".to_owned())
}

fn entry_for(name: String, metadata: &std::fs::Metadata) -> CopyEntry {
    let (kind, taken_at) = stamped(&name).unwrap_or_else(|| {
        let written = metadata
            .modified()
            .ok()
            .and_then(crate::gc::file_time)
            .unwrap_or(OffsetDateTime::UNIX_EPOCH);
        (CopyKind::Added, written)
    });
    CopyEntry {
        name,
        kind,
        taken_at,
        size_bytes: metadata.len(),
    }
}

/// A name for a new copy no file has: the time now, or a second later.
async fn free_name(dir: &Path, prefix: &str) -> (String, String) {
    loop {
        let stamp = stamp_of(OffsetDateTime::now_utc());
        let name = format!("{prefix}-{stamp}.dump");
        if tokio::fs::symlink_metadata(dir.join(&name)).await.is_err() {
            return (name, stamp);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Make `path` empty, readable by the group alone, before anything is
/// written to it.
async fn create_private(path: &Path) -> Result<tokio::fs::File, BackupError> {
    tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(COPY_MODE)
        .open(path)
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem => {
                BackupError::NotWritable(not_writable_detail())
            }
            _ => BackupError::Io {
                what: "making the copy's file",
                source: error,
            },
        })
}

fn not_writable_detail() -> String {
    "start the stack once with this release's compose.yaml (`docker compose up -d`), \
     whose backup service lets the server write there"
        .to_owned()
}

/// A finished partial file, flushed and in place under its name.
async fn finish(part: &Path, path: &Path) -> Result<(), BackupError> {
    // pg_dump opens the file again, so it is made private once more in
    // case anything widened it.
    tokio::fs::set_permissions(part, std::fs::Permissions::from_mode(COPY_MODE))
        .await
        .map_err(io("finishing the copy"))?;
    let file = tokio::fs::File::open(part)
        .await
        .map_err(io("finishing the copy"))?;
    file.sync_all().await.map_err(io("finishing the copy"))?;
    drop(file);
    tokio::fs::rename(part, path)
        .await
        .map_err(io("finishing the copy"))?;
    if let Some(dir) = path.parent() {
        sync_dir(dir).await;
    }
    Ok(())
}

async fn sync_dir(dir: &Path) {
    if let Ok(dir) = tokio::fs::File::open(dir).await {
        let _ = dir.sync_all().await;
    }
}

/// Whether a file can be made in `dir`.
async fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".server-probe-{}", uuid::Uuid::now_v7()));
    match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(COPY_MODE)
        .open(&probe)
        .await
    {
        Ok(_) => {
            let _ = tokio::fs::remove_file(&probe).await;
            true
        }
        Err(_) => false,
    }
}

fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// The major version in a tool's `--version`: 17 for
/// "pg_dump (PostgreSQL) 17.12 (Debian 17.12-1)".
fn major_version(text: &str) -> Option<u32> {
    let after = text.split_once(')')?.1.trim_start();
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

// --- running the tools ------------------------------------------------------------

/// What a run printed, bounded.
#[derive(Debug, Clone)]
pub(crate) struct RunOutput {
    pub(crate) success: bool,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    /// Whether stdout was longer than kept.
    pub(crate) truncated: bool,
}

/// The last lines a run printed, for an error.
pub(crate) fn last_lines(output: &RunOutput) -> String {
    let text = if output.stderr.trim().is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    let lines: Vec<&str> = text
        .split(['\n', '\r'])
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let shown = lines[lines.len().saturating_sub(3)..].join(" / ");
    if shown.is_empty() {
        "it said nothing".to_owned()
    } else {
        shown
    }
}

/// Run a program with fixed arguments and only the environment given, both
/// pipes read at once and bounded, killed with its process group past the
/// time limit or when dropped.
pub(crate) async fn run(
    program: &Path,
    args: &[&str],
    env: &[(&'static str, String)],
    timeout: Duration,
) -> std::io::Result<RunOutput> {
    run_limited(program, args, env, timeout, OUTPUT_LIMIT).await
}

/// [`run`], keeping up to `stdout_limit` bytes of stdout: the first, with
/// [`RunOutput::truncated`] saying when there were more.
pub(crate) async fn run_limited(
    program: &Path,
    args: &[&str],
    env: &[(&'static str, String)],
    timeout: Duration,
    stdout_limit: usize,
) -> std::io::Result<RunOutput> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(name, value)| (*name, value.as_str())))
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command.spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let reading = async { tokio::join!(read_head(stdout, stdout_limit), read_bounded(stderr)) };
    let finished = tokio::time::timeout(timeout, async {
        let (out, err) = reading.await;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((status, out, err))
    })
    .await;
    if let Ok(result) = finished {
        let (status, (stdout, truncated), stderr) = result?;
        Ok(RunOutput {
            success: status.success(),
            stdout,
            stderr,
            truncated,
        })
    } else {
        let _ = child.kill().await;
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "{} took longer than {} minutes",
                program.display(),
                timeout.as_secs() / 60
            ),
        ))
    }
}

/// Read a pipe to the end, keeping at most its first `limit` bytes, and
/// whether there were more: a table of contents is read from its start.
async fn read_head<R: AsyncRead + Unpin>(pipe: Option<R>, limit: usize) -> (String, bool) {
    let Some(mut pipe) = pipe else {
        return (String::new(), false);
    };
    let mut kept: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut buf = vec![0_u8; 8192];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = limit.saturating_sub(kept.len());
                if n > room {
                    truncated = true;
                }
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
    (String::from_utf8_lossy(&kept).into_owned(), truncated)
}

/// Read a pipe to the end, keeping at most the last [`OUTPUT_LIMIT`] bytes.
async fn read_bounded<R: AsyncRead + Unpin>(pipe: Option<R>) -> String {
    let Some(mut pipe) = pipe else {
        return String::new();
    };
    let mut kept: Vec<u8> = Vec::new();
    let mut buf = vec![0_u8; 8192];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                kept.extend_from_slice(&buf[..n]);
                if kept.len() > 2 * OUTPUT_LIMIT {
                    kept.drain(..kept.len() - OUTPUT_LIMIT);
                }
            }
        }
    }
    if kept.len() > OUTPUT_LIMIT {
        kept.drain(..kept.len() - OUTPUT_LIMIT);
    }
    String::from_utf8_lossy(&kept).into_owned()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use tangible_domain::SecretString;

    #[test]
    fn a_copy_is_a_plain_name_ending_dump() {
        for name in [
            "tangible-20261117T210000Z.dump",
            "uploaded-20261118T030000Z.dump",
            "from-the-old-machine.dump",
            "a.b_c-d.dump",
        ] {
            assert!(is_copy_name(name), "{name}");
        }
        for name in [
            ".tangible-20261117T210000Z.part",
            ".hidden.dump",
            ".dump",
            "../x.dump",
            "a/b.dump",
            "space here.dump",
            "x.dump.part",
            "x.sql",
            "quote'.dump",
        ] {
            assert!(!is_copy_name(name), "{name}");
        }
        assert!(!is_copy_name(&format!("{}.dump", "a".repeat(200))));
    }

    #[test]
    fn a_name_tangible_gives_says_when_and_whose() {
        let (kind, at) = stamped("tangible-20261117T210503Z.dump").unwrap();
        assert_eq!(kind, CopyKind::Taken);
        assert_eq!(at, time::macros::datetime!(2026-11-17 21:05:03 UTC));
        assert_eq!(
            stamped("uploaded-20261118T030000Z.dump").unwrap().0,
            CopyKind::Uploaded
        );
        for name in [
            "tangible-2026111T210503Z.dump",
            "tangible-20261317T210503Z.dump",
            "tangible-20261117T210503.dump",
            "mine-20261117T210503Z.dump",
            "tangible-20261117T210503Z-2.dump",
        ] {
            assert!(stamped(name).is_none(), "{name}");
        }
        assert_eq!(
            stamp_of(time::macros::datetime!(2026-11-17 21:05:03 UTC)),
            "20261117T210503Z"
        );
    }

    #[test]
    fn the_schedule_is_due_today_until_its_time_has_passed() {
        use time::macros::datetime;
        let three = 3 * 60;
        assert_eq!(
            next_due(datetime!(2026-11-18 02:59:59 UTC), three),
            datetime!(2026-11-18 03:00:00 UTC)
        );
        // At the minute, or just after, it is tomorrow's: a copy taken at
        // 03:00 is not taken twice.
        assert_eq!(
            next_due(datetime!(2026-11-18 03:00:00 UTC), three),
            datetime!(2026-11-19 03:00:00 UTC)
        );
        assert_eq!(
            next_due(datetime!(2026-11-18 03:00:00.5 UTC), three),
            datetime!(2026-11-19 03:00:00 UTC)
        );
        assert_eq!(
            next_due(datetime!(2026-12-31 23:59:30 UTC), 0),
            datetime!(2027-01-01 00:00:00 UTC)
        );
        assert_eq!(
            next_due(datetime!(2026-11-18 12:00:00 UTC), 23 * 60 + 59),
            datetime!(2026-11-18 23:59:00 UTC)
        );
    }

    #[test]
    fn a_tool_says_its_major_version() {
        assert_eq!(major_version("pg_dump (PostgreSQL) 17.12\n"), Some(17));
        assert_eq!(
            major_version("pg_dump (PostgreSQL) 17.12 (Debian 17.12-1.pgdg13+1)\n"),
            Some(17)
        );
        assert_eq!(major_version("pg_restore (PostgreSQL) 18beta1"), Some(18));
        assert_eq!(major_version("something else"), None);
    }

    #[test]
    fn the_password_reaches_the_environment_and_not_debug_output() {
        let connection = ToolConnection {
            host: "postgres".to_owned(),
            port: 5432,
            user: "tangible".to_owned(),
            password: Some(SecretString::from("hunter2-placeholder".to_owned())),
            database: "tangible".to_owned(),
            sslmode: Some("require".to_owned()),
        };
        assert!(!format!("{connection:?}").contains("hunter2-placeholder"));
        let env = connection.environment("tangible", connection.password.as_ref(), "tangible");
        assert!(env.contains(&("PGPASSWORD", "hunter2-placeholder".to_owned())));
        assert!(env.contains(&("PGSSLMODE", "require".to_owned())));
        assert!(env.contains(&("PGHOST", "postgres".to_owned())));
    }

    fn settings(dir: &Path, kept: u32) -> BackupSettings {
        BackupSettings {
            dir: dir.to_owned(),
            kept,
            tools: PathBuf::from("/nonexistent"),
            connection: ToolConnection {
                host: "localhost".to_owned(),
                port: 5432,
                user: "tangible".to_owned(),
                password: None,
                database: "tangible".to_owned(),
                sslmode: None,
            },
            max_upload_bytes: 1024,
        }
    }

    #[tokio::test]
    async fn the_volume_lists_copies_newest_first_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "tangible-20261101T000000Z.dump",
            "tangible-20261103T000000Z.dump",
            "uploaded-20261102T000000Z.dump",
            ".tangible-20261104T000000Z.part",
            "notes.txt",
        ] {
            std::fs::write(dir.path().join(name), b"PGDMP").unwrap();
        }
        std::fs::create_dir(dir.path().join("folder.dump")).unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("tangible-20261101T000000Z.dump"),
            dir.path().join("linked.dump"),
        )
        .unwrap();
        let backups = Backups::new(settings(dir.path(), 10));
        let listing = backups.list().await.unwrap();
        let names: Vec<&str> = listing.copies.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "tangible-20261103T000000Z.dump",
                "uploaded-20261102T000000Z.dump",
                "tangible-20261101T000000Z.dump",
            ]
        );
        assert!(listing.writable);
        assert!(matches!(
            backups.find("linked.dump").await,
            Err(BackupError::NotFound)
        ));
        assert!(matches!(
            backups.find("../etc/passwd.dump").await,
            Err(BackupError::NotFound)
        ));
    }

    #[tokio::test]
    async fn pruning_counts_and_removes_only_the_copies_tangible_names() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "tangible-20261101T000000Z.dump",
            "tangible-20261102T000000Z.dump",
            "tangible-20261103T000000Z.dump",
            "uploaded-20261001T000000Z.dump",
            "mine.dump",
        ] {
            std::fs::write(dir.path().join(name), b"PGDMP").unwrap();
        }
        Backups::new(settings(dir.path(), 2)).prune().await;
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "mine.dump",
                "tangible-20261102T000000Z.dump",
                "tangible-20261103T000000Z.dump",
                "uploaded-20261001T000000Z.dump",
            ]
        );
    }

    #[tokio::test]
    async fn one_thing_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let backups = Backups::new(settings(dir.path(), 2));
        let turn = backups
            .begin(Operation::Uploading {
                started_at: OffsetDateTime::now_utc(),
                by: "admin".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            backups.begin(Operation::Taking {
                started_at: OffsetDateTime::now_utc(),
                by: "admin".to_owned(),
            }),
            Err(BackupError::Busy("an upload"))
        ));
        assert!(backups.operation().is_some());
        drop(turn);
        assert!(backups.operation().is_none());
    }

    #[tokio::test]
    async fn copies_are_off_when_none_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let backups = Backups::new(settings(dir.path(), 0));
        let turn = backups
            .begin(Operation::Taking {
                started_at: OffsetDateTime::now_utc(),
                by: "admin".to_owned(),
            })
            .unwrap();
        assert!(matches!(backups.take(&turn).await, Err(BackupError::Off)));
        assert!(backups.last_outcome().unwrap().error.is_some());
    }

    #[tokio::test]
    async fn an_upload_that_is_not_an_archive_is_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let backups = Backups::new(settings(dir.path(), 2));
        let turn = backups
            .begin(Operation::Uploading {
                started_at: OffsetDateTime::now_utc(),
                by: "admin".to_owned(),
            })
            .unwrap();
        let body = futures_util::stream::iter([Ok::<_, std::io::Error>(
            axum::body::Bytes::from_static(b"-- a plain SQL script\n"),
        )]);
        assert!(matches!(
            backups.store_upload(&turn, body).await,
            Err(BackupError::NotACopy(_))
        ));
        let too_long = futures_util::stream::iter([Ok::<_, std::io::Error>(
            axum::body::Bytes::from(vec![b'P'; 2048]),
        )]);
        assert!(matches!(
            backups.store_upload(&turn, too_long).await,
            Err(BackupError::TooLarge(1024) | BackupError::NotACopy(_))
        ));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_copy_is_made_readable_by_the_group_alone() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join(".server-x.part");
        let file = create_private(&part).await.unwrap();
        drop(file);
        let mode = std::fs::metadata(&part).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & 0o007, 0, "{mode:o}");
        finish(&part, &dir.path().join("tangible-20261101T000000Z.dump"))
            .await
            .unwrap();
        let mode = std::fs::metadata(dir.path().join("tangible-20261101T000000Z.dump"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o640);
    }
}
