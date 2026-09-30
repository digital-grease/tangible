// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Running cdrdao.
//!
//! The adapter that turns the table of contents writer, the argument builder
//! and the output parser into a [`BurnEngine`] for discs described as tracks.
//! The process rules are the xorriso engine's, and one is stricter here:
//!
//! - **A write is never interrupted.** Cancellation is honoured before the
//!   write starts and never after.
//! - **Output is bounded, and both pipes are drained at the same time.** cdrdao
//!   prints everything to stderr; a reader waiting on stdout first while stderr
//!   fills would stop the process, and a stopped process mid-write is a ruined
//!   disc.
//! - **A write is judged line by line as it happens**, not from the retained
//!   log, because the line that says it finished is the last one and the one a
//!   truncated log would lose.
//!
//! # Asking the tool before trusting the document
//!
//! Preflight writes the table of contents, then runs `cdrdao show-toc` on it
//! and compares cdrdao's reading, track by track, against the plan: modes,
//! pregaps, where every track starts and ends, index points, flags and codes.
//! The writer is tested against golden output, but the golden output is this
//! project's idea of the format. `show-toc` is cdrdao's, and it is the one that
//! decides what lands on the disc. It is also the check that catches a missing
//! or short file and a track under four seconds, which cdrdao prints as
//! objections and then exits 0 from.
//!
//! # What this engine does not claim
//!
//! Verification is not built. Reading a disc of tracks back needs raw sector
//! reads, a per-track report, and a real drive to establish what the reads can
//! be trusted for; audio in particular cannot be compared byte for byte without
//! the drive's read offset. Until that exists [`BurnEngine::verify`] says it
//! cannot, and the runner records the disc as written and unverified. A result
//! that reported a match here would be recorded as a full sector comparison of
//! a digest nobody computed.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tangible_domain::Sha256Digest;
use tangible_domain::cd::TrackFlag;
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Command;

use crate::cdrdao::{command, parse, toc};
use crate::engine::{BurnEngine, BurnEvent, EngineError, EventSink};
use crate::fake::CancelToken;
use crate::plan::{
    BlankReport, BlankRequest, BurnPlan, DriveCapabilities, DriveRef, MediumInfo, PreflightFailure,
    PreflightReport, VerifyReport, WriteMode, WriteReport,
};

/// How long inspecting or probing a drive may take.
///
/// Longer than xorriso's: an empty drive makes cdrdao retry "unit not ready"
/// ten times at three-second intervals before it gives up, and the answer at
/// the end of that is worth waiting for.
const PROBE_TIMEOUT: Duration = Duration::from_secs(90);

/// How long `show-toc` may take. It reads a small file and stats a few more.
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);

/// How much tool output is retained. What is dropped is counted, not kept.
const MAX_LOG_BYTES: usize = 256 * 1024;

/// Longest line kept whole. A tool that never ends a line must not grow a
/// buffer without limit; the rest of such a line is dropped.
const MAX_LINE_BYTES: usize = 4096;

/// Most diagnostics carried on a write report.
const MAX_DIAGNOSTICS: usize = 20;

/// Bytes in the block cdrdao counts capacity in: one raw CD sector.
const CD_BLOCK_BYTES: u32 = 2352;

/// The cdrdao burn engine.
#[derive(Debug, Clone)]
pub struct CdrdaoEngine {
    program: PathBuf,
    version: String,
    cancel: CancelToken,
    work_dir: PathBuf,
}

impl CdrdaoEngine {
    /// An engine that runs `cdrdao` from `PATH` and keeps its tables of
    /// contents in `work_dir`.
    ///
    /// `work_dir` must be absolute: the table of contents is passed to cdrdao
    /// as a bare argument, and the engine refuses to pass one that could be
    /// read as an option. The version is not probed here, so a worker whose
    /// image is broken can still start and say so.
    #[must_use]
    pub fn new(work_dir: impl Into<PathBuf>) -> Self {
        Self::at(command::CDRDAO, work_dir)
    }

    /// An engine that runs a specific binary. For a pinned image, and tests.
    #[must_use]
    pub fn at(program: impl Into<PathBuf>, work_dir: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            version: "unknown".to_owned(),
            cancel: CancelToken::new(),
            work_dir: work_dir.into(),
        }
    }

    /// Attach a cancellation token.
    #[must_use]
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Ask the tool what version it is and remember the answer.
    ///
    /// # Errors
    ///
    /// [`EngineError::Unsupported`] if the binary does not identify itself as
    /// cdrdao, or [`EngineError::Io`] if it cannot be run.
    pub async fn probe_version(&mut self) -> Result<String, EngineError> {
        let run = self.run(&command::version(), CHECK_TIMEOUT).await?;
        let version = parse::version(&run.text()).ok_or_else(not_cdrdao)?;
        self.version.clone_from(&version);
        Ok(version)
    }

    /// Write the plan's table of contents and ask cdrdao whether it reads it
    /// as the plan means it.
    ///
    /// Needs no drive, which is why it is separate from preflight and public:
    /// every answer it gives is the same whatever disc is in the tray, and it
    /// is testable against the real tool on a machine without one.
    ///
    /// # Errors
    ///
    /// [`EngineError`] if the tool cannot be run at all. A table of contents
    /// that cannot be written or that cdrdao objects to is a failure in the
    /// returned list, not an error.
    pub async fn check_table_of_contents(
        &self,
        plan: &BurnPlan,
    ) -> Result<Vec<PreflightFailure>, EngineError> {
        let document = match toc::write_toc(plan) {
            Ok(document) => document,
            Err(error) => {
                return Ok(vec![PreflightFailure::TableOfContentsRefused {
                    cause: error.to_string(),
                }]);
            }
        };
        let path = self.store_toc(plan, &document).await?;
        let arguments = command::show_toc(&path).map_err(|error| unusable(&error))?;
        let run = self.run(&arguments, CHECK_TIMEOUT).await?;
        let text = run.text();
        if parse::version(&text).is_none() {
            return Err(not_cdrdao());
        }

        let listing = parse::toc_listing(&text);
        let mut failures: Vec<PreflightFailure> = listing
            .problems
            .iter()
            .map(|problem| PreflightFailure::TableOfContentsRefused {
                cause: format!("cdrdao: {}", problem.text),
            })
            .collect();
        if failures.is_empty() && listing.tracks.is_empty() {
            failures.push(PreflightFailure::TableOfContentsRefused {
                cause: format!(
                    "cdrdao read no tracks from the table of contents (exit status {:?})",
                    run.exit_code
                ),
            });
        }
        if failures.is_empty() {
            failures.extend(
                disagreements(plan, &listing)
                    .into_iter()
                    .map(|detail| PreflightFailure::TableOfContentsDisagrees { detail }),
            );
        }
        Ok(failures)
    }

    /// Where a plan's table of contents lives.
    ///
    /// One per attempt, named for it, and left in place afterwards: it is a
    /// small, exact record of what the drive was told to write.
    fn toc_path(&self, plan: &BurnPlan) -> PathBuf {
        self.work_dir.join(format!("{}.toc", plan.attempt_id))
    }

    /// Write a table of contents atomically.
    async fn store_toc(&self, plan: &BurnPlan, document: &str) -> Result<PathBuf, EngineError> {
        let path = self.toc_path(plan);
        if !path.is_absolute() {
            return Err(EngineError::Unsupported {
                what: format!(
                    "the engine's working directory {} is not absolute",
                    self.work_dir.display()
                ),
            });
        }
        let io = |operation: &'static str| {
            move |source: std::io::Error| EngineError::Io { operation, source }
        };

        tokio::fs::create_dir_all(&self.work_dir)
            .await
            .map_err(io("creating the table of contents directory"))?;
        let staging = path.with_extension("toc.partial");
        tokio::fs::write(&staging, document)
            .await
            .map_err(io("writing the table of contents"))?;
        tokio::fs::rename(&staging, &path)
            .await
            .map_err(io("placing the table of contents"))?;
        Ok(path)
    }

    /// Run the tool to completion and collect what it said.
    async fn run(&self, arguments: &[String], timeout: Duration) -> Result<Run, EngineError> {
        let mut child = Command::new(&self.program)
            .args(arguments)
            .current_dir(self.working_directory())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| EngineError::Io {
                operation: "starting cdrdao",
                source,
            })?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let finished = tokio::time::timeout(timeout, async {
            // Together, never one after the other: a pipe nobody is reading
            // fills, and the process writing to it stops.
            let (stdout, stderr) = tokio::join!(read_bounded(stdout), read_bounded(stderr));
            (stdout, stderr, child.wait().await)
        })
        .await;

        let Ok((stdout, stderr, status)) = finished else {
            // Never reached by a write, which is not run through here.
            let _ = child.kill().await;
            return Err(EngineError::Timeout {
                operation: "cdrdao",
                seconds: timeout.as_secs(),
            });
        };
        let status = status.map_err(|source| EngineError::Io {
            operation: "waiting for cdrdao",
            source,
        })?;

        let run = Run {
            stdout,
            stderr,
            exit_code: status.code(),
        };
        tracing::debug!(
            program = %self.program.display(),
            exit_code = ?run.exit_code,
            "cdrdao finished"
        );
        Ok(run)
    }

    /// Where the tool runs. Explicit, and somewhere this engine owns, so a
    /// temporary file cdrdao writes relative to its working directory does
    /// not land in the worker's state.
    fn working_directory(&self) -> PathBuf {
        if self.work_dir.is_dir() {
            self.work_dir.clone()
        } else {
            std::env::temp_dir()
        }
    }

    fn check_cancelled(&self) -> Result<(), EngineError> {
        if self.cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        Ok(())
    }
}

/// What one run of the tool produced.
#[derive(Debug)]
struct Run {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
}

impl Run {
    /// Both streams, stderr first. cdrdao's banner and every flagged message
    /// are on stderr, and the reports `show-toc` and `disk-info` print are on
    /// stdout; the parsers read each part from wherever it is.
    fn text(&self) -> String {
        format!("{}\n{}", self.stderr, self.stdout)
    }
}

fn not_cdrdao() -> EngineError {
    EngineError::Unsupported {
        what: "the configured binary does not identify itself as cdrdao".to_owned(),
    }
}

fn unusable(error: &command::CommandError) -> EngineError {
    EngineError::Unsupported {
        what: error.to_string(),
    }
}

/// Read a pipe to its end, keeping at most [`MAX_LOG_BYTES`] of it.
///
/// Keeps reading after the cap, discarding, so the process is never left
/// blocked on a full pipe.
async fn read_bounded<R: AsyncRead + Unpin>(pipe: Option<R>) -> String {
    let Some(mut pipe) = pipe else {
        return String::new();
    };
    let mut kept = Vec::new();
    let mut dropped = 0_usize;
    // On the heap: two of these on the stack of a joined future make it large
    // enough to matter.
    let mut buffer = vec![0_u8; 8192];
    loop {
        match pipe.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let room = MAX_LOG_BYTES.saturating_sub(kept.len());
                let keep = read.min(room);
                kept.extend_from_slice(&buffer[..keep]);
                dropped += read - keep;
            }
        }
    }
    let mut text = String::from_utf8_lossy(&kept).into_owned();
    if dropped > 0 {
        let _ = write!(text, "\n[output truncated: {dropped} bytes dropped]\n");
    }
    text
}

/// Splits a byte stream into lines on `\r` as well as `\n`, as it arrives.
#[derive(Debug, Default)]
struct LineSplitter {
    pending: Vec<u8>,
}

impl LineSplitter {
    /// Take in some bytes, handing each completed line to `on_line`.
    fn push(&mut self, bytes: &[u8], on_line: &mut impl FnMut(&str)) {
        for &byte in bytes {
            if byte == b'\r' || byte == b'\n' {
                self.flush(on_line);
            } else if self.pending.len() < MAX_LINE_BYTES {
                self.pending.push(byte);
            }
        }
    }

    /// Hand over whatever is left as a final line.
    fn flush(&mut self, on_line: &mut impl FnMut(&str)) {
        if !self.pending.is_empty() {
            on_line(&String::from_utf8_lossy(&self.pending));
            self.pending.clear();
        }
    }
}

/// Follow a write's stderr as it happens: judge each line, report it, keep a
/// bounded copy.
async fn follow_write<R: AsyncRead + Unpin>(
    pipe: Option<R>,
    sink: &dyn EventSink,
) -> (parse::WriteOutcome, String) {
    let mut outcome = parse::WriteOutcome::default();
    let mut log = String::new();
    let Some(mut pipe) = pipe else {
        return (outcome, log);
    };

    let mut splitter = LineSplitter::default();
    let mut last_mb = None;
    let mut on_line = |line: &str| {
        if let Some(event) = outcome.observe(line) {
            emit(sink, &event, &mut last_mb);
        }
        push_bounded(&mut log, line);
        push_bounded(&mut log, "\n");
    };

    // On the heap: two of these on the stack of a joined future make it large
    // enough to matter.
    let mut buffer = vec![0_u8; 8192];
    loop {
        match pipe.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => splitter.push(&buffer[..read], &mut on_line),
        }
    }
    splitter.flush(&mut on_line);
    (outcome, log)
}

/// Append to a bounded log, saying so once when something is dropped.
fn push_bounded(log: &mut String, addition: &str) {
    const MARK: &str = "\n[output truncated]\n";
    if log.ends_with(MARK) {
        return;
    }
    if log.len() + addition.len() <= MAX_LOG_BYTES {
        log.push_str(addition);
    } else {
        log.push_str(MARK);
    }
}

/// Turn a write event into something the operator sees.
fn emit(sink: &dyn EventSink, event: &parse::WriteEvent, last_mb: &mut Option<u64>) {
    match event {
        parse::WriteEvent::Progress {
            written_mb,
            total_mb,
        } => {
            // cdrdao repeats a reading until the next megabyte; one event per
            // megabyte is plenty.
            if *last_mb == Some(*written_mb) {
                return;
            }
            *last_mb = Some(*written_mb);
            #[allow(clippy::cast_precision_loss)]
            let fraction = if *total_mb == 0 {
                0.0
            } else {
                *written_mb as f32 / *total_mb as f32
            };
            sink.emit(BurnEvent::progress(
                "writing",
                "WRITE_PROGRESS",
                fraction,
                format!("{written_mb} of {total_mb} MB"),
            ));
        }
        parse::WriteEvent::TrackStarted { track } => sink.emit(BurnEvent::new(
            "writing",
            "TRACK_STARTED",
            format!("writing track {track}"),
        )),
        parse::WriteEvent::DataWritten { blocks } => sink.emit(BurnEvent::new(
            "writing",
            "WRITE_LEADOUT",
            // Said because the next minute looks like nothing happening.
            format!(
                "all {blocks} blocks sent; the drive is writing the lead-out and closing the disc"
            ),
        )),
        parse::WriteEvent::Completed => sink.emit(BurnEvent::new(
            "writing",
            "WRITE_COMPLETED",
            "the engine reported the write finished",
        )),
        parse::WriteEvent::Problem(message) => sink.emit(BurnEvent::new(
            "writing",
            "ENGINE_MESSAGE",
            format!("{}: {}", severity_word(message.severity), message.text),
        )),
    }
}

const fn severity_word(severity: parse::Severity) -> &'static str {
    match severity {
        parse::Severity::Warning => "WARNING",
        parse::Severity::Error => "ERROR",
        parse::Severity::InternalError => "INTERNAL ERROR",
        parse::Severity::Fatal => "FATAL ERROR",
    }
}

/// Turn a disk report into the shape the rest of the system speaks.
fn medium_info(info: &parse::DiskInfo) -> MediumInfo {
    let rewritable = info.rewritable == Some(true);
    let blank = info.empty == Some(true);
    let appendable = info.appendable == Some(true);
    let free_blocks = if blank {
        info.capacity_blocks.unwrap_or(0)
    } else if appendable {
        info.remaining_blocks.unwrap_or(0)
    } else {
        0
    };

    MediumInfo {
        // cdrdao does not report an MMC profile, only whether the disc is
        // rewritable, and it only talks to CD drives. A drive that would not
        // say is "unknown", which a track layout's preflight refuses rather
        // than assuming a CD.
        profile: match info.rewritable {
            Some(true) => "CD-RW",
            Some(false) => "CD-R",
            None => "unknown",
        }
        .to_owned(),
        blank,
        writable: free_blocks > 0,
        rewritable,
        capacity_blocks: info.capacity_blocks.unwrap_or(0),
        free_blocks,
        block_size: CD_BLOCK_BYTES,
        manufacturer_id: info.manufacturer.clone(),
        sessions: info.sessions.unwrap_or(0),
        erasable: rewritable,
        status_warnings: Vec::new(),
    }
}

/// One track as cdrdao should list it if it read the plan's table of contents
/// the way the plan means it.
#[derive(Debug, PartialEq, Eq)]
struct ExpectedTrack {
    mode: &'static str,
    isrc: Option<String>,
    copy_permitted: bool,
    pre_emphasis: bool,
    four_channel: bool,
    pregap: u64,
    start: u64,
    end: u64,
    indexes: Vec<u64>,
}

/// Where cdrdao's reading of a table of contents differs from the plan.
///
/// Positions are counted from the start of the program area, as cdrdao lists
/// them: track one begins at zero because cdrdao supplies its lead-in pregap
/// itself. A track begins where the last ended; its generated gap comes
/// first, then its file's sectors, and INDEX 01 is its whole pregap in.
fn disagreements(plan: &BurnPlan, listing: &parse::TocListing) -> Vec<String> {
    let mut found = Vec::new();

    let disc_type = toc::disc_type(&plan.tracks);
    if listing.disc_type.as_deref() != Some(disc_type) {
        found.push(format!(
            "the disc type is {:?}, where the plan makes it {disc_type}",
            listing.disc_type
        ));
    }
    if listing.catalog != plan.catalog {
        found.push(format!(
            "the catalogue number is {:?}, where the plan has {:?}",
            listing.catalog, plan.catalog
        ));
    }
    if listing.tracks.len() != plan.tracks.len() {
        found.push(format!(
            "{} tracks were read, where the plan has {}",
            listing.tracks.len(),
            plan.tracks.len()
        ));
        return found;
    }

    let mut position = 0_u64;
    for (track, listed) in plan.tracks.iter().zip(&listing.tracks) {
        let Ok(mode) = toc::track_mode(track) else {
            found.push(format!("track {} has no cdrdao mode", track.number));
            continue;
        };
        let audio = track.is_audio();
        let start = position.saturating_add(track.pregap_sectors);
        let end = position
            .saturating_add(track.generated_pregap())
            .saturating_add(track.sector_count);
        let index_one = track
            .indexes
            .iter()
            .find(|index| index.number == 1)
            .map_or(0, |index| index.relative_lba);

        let expected = ExpectedTrack {
            mode,
            isrc: track
                .isrc
                .as_ref()
                .filter(|_| audio)
                .map(|isrc| isrc.to_ascii_uppercase()),
            copy_permitted: track.flags.contains(&TrackFlag::DigitalCopyPermitted),
            pre_emphasis: audio && track.flags.contains(&TrackFlag::PreEmphasis),
            four_channel: audio && track.flags.contains(&TrackFlag::FourChannel),
            pregap: track.pregap_sectors,
            start,
            end,
            indexes: track
                .indexes
                .iter()
                .filter(|index| index.number > 1)
                .map(|index| start + index.relative_lba.saturating_sub(index_one))
                .collect(),
        };
        let actual = ExpectedTrack {
            mode: expected.mode,
            isrc: listed.isrc.clone(),
            copy_permitted: listed.copy_permitted,
            pre_emphasis: listed.pre_emphasis,
            four_channel: listed.four_channel,
            pregap: listed.pregap,
            start: listed.start,
            end: listed.end,
            indexes: listed.indexes.iter().map(|(_, at)| *at).collect(),
        };

        if listed.mode != mode {
            found.push(format!(
                "track {} is read as {}, where the plan makes it {mode}",
                track.number, listed.mode
            ));
        }
        if actual != expected {
            found.push(format!(
                "track {} is read as {actual:?}, where the plan means {expected:?}",
                track.number
            ));
        }
        position = end;
    }

    found
}

#[async_trait]
impl BurnEngine for CdrdaoEngine {
    fn name(&self) -> &'static str {
        "cdrdao"
    }

    fn version(&self) -> String {
        self.version.clone()
    }

    fn uses_hardware(&self) -> bool {
        true
    }

    /// Tracks only. A single prepared image is xorriso's job, and writing one
    /// here as a single data track would work and would be the wrong engine
    /// for it.
    fn supports_mode(&self, mode: WriteMode) -> bool {
        matches!(mode, WriteMode::TocDiscAtOnce)
    }

    async fn probe_drive(&self, drive: &DriveRef) -> Result<DriveCapabilities, EngineError> {
        self.check_cancelled()?;
        let run = self.run(&command::drive_info(drive), PROBE_TIMEOUT).await?;
        let text = run.text();
        if parse::device_unavailable(&text) {
            return Err(EngineError::DriveUnavailable {
                alias: drive.device_alias.clone(),
            });
        }

        // Evidence rather than permission, as with xorriso: profiles stay
        // empty until a disc is inspected.
        let info = parse::drive_info(&text, &drive.device_alias);
        let mut capabilities = DriveCapabilities {
            supports_buffer_underrun_protection: info.burn_proof == Some(true),
            ..DriveCapabilities::default()
        };
        let evidence = if parse::no_disc(&text) {
            "the drive did not answer: unit not ready".to_owned()
        } else {
            [
                info.identity.clone(),
                info.driver.map(|driver| format!("driver {driver}")),
                info.max_write_kbps
                    .map(|speed| format!("writes at up to {speed} kB/s")),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("; ")
        };
        capabilities
            .engine_evidence
            .insert("cdrdao".to_owned(), evidence);
        Ok(capabilities)
    }

    async fn inspect_medium(&self, drive: &DriveRef) -> Result<MediumInfo, EngineError> {
        self.check_cancelled()?;
        let run = self.run(&command::disk_info(drive), PROBE_TIMEOUT).await?;
        let text = run.text();
        if parse::version(&text).is_none() {
            return Err(not_cdrdao());
        }
        if parse::device_unavailable(&text) {
            return Err(EngineError::DriveUnavailable {
                alias: drive.device_alias.clone(),
            });
        }
        let info = parse::disk_info(&text);
        if parse::no_disc(&text) || !info.present() {
            return Err(EngineError::NoMedium {
                alias: drive.device_alias.clone(),
            });
        }
        Ok(medium_info(&info))
    }

    async fn preflight(&self, plan: &BurnPlan) -> Result<PreflightReport, EngineError> {
        self.check_cancelled()?;

        // First, because none of its answers depend on the disc: a table of
        // contents cdrdao would refuse should be said before anyone is asked
        // for media, not after.
        let failures = self.check_table_of_contents(plan).await?;
        if !failures.is_empty() {
            return Ok(PreflightReport {
                failures,
                warnings: Vec::new(),
                medium: None,
            });
        }

        let medium = match self.inspect_medium(&plan.drive).await {
            Ok(medium) => medium,
            Err(EngineError::NoMedium { .. }) => {
                return Ok(PreflightReport {
                    failures: vec![PreflightFailure::NoMedium],
                    warnings: Vec::new(),
                    medium: None,
                });
            }
            Err(error) => return Err(error),
        };

        let mut failures = Vec::new();
        if !medium.writable {
            failures.push(PreflightFailure::MediumNotWritable {
                profile: medium.profile.clone(),
            });
        }
        // Disc-at-once writes a whole disc and cdrdao does not erase first,
        // so anything already on it, appendable or not, is a refusal.
        if !medium.blank {
            failures.push(PreflightFailure::MediumNotBlank {
                sessions: medium.sessions,
            });
        }
        if !plan.accepted_profiles.is_empty() && !plan.accepted_profiles.contains(&medium.profile) {
            failures.push(PreflightFailure::ProfileNotAccepted {
                found: medium.profile.clone(),
                accepted: plan.accepted_profiles.clone(),
            });
        }
        failures.extend(plan.capacity_failures(&medium));

        if failures.is_empty() {
            // Only once the disc is right. The runner repeats preflight while
            // it waits for media, and reading every input on every poll would
            // re-read a whole disc image every few seconds for nothing. The
            // round that can pass is the round that hashes, so the check
            // still happens immediately before the write.
            for input in &plan.inputs {
                match hash_file(&input.staged_path).await {
                    Err(_) => failures.push(PreflightFailure::InputMissing {
                        path: input.staged_path.display().to_string(),
                    }),
                    Ok(actual) if actual != input.sha256 => {
                        failures.push(PreflightFailure::InputDigestMismatch {
                            path: input.staged_path.display().to_string(),
                            expected: input.sha256.to_hex(),
                            actual: actual.to_hex(),
                        });
                    }
                    Ok(_) => {}
                }
            }
        }

        Ok(PreflightReport {
            failures,
            warnings: Vec::new(),
            medium: Some(medium),
        })
    }

    async fn write(
        &self,
        plan: &BurnPlan,
        sink: &dyn EventSink,
    ) -> Result<WriteReport, EngineError> {
        // Checked here and never again.
        self.check_cancelled()?;

        let document = toc::write_toc(plan).map_err(|error| EngineError::Unsupported {
            what: format!("the table of contents cannot be written: {error}"),
        })?;
        let path = self.store_toc(plan, &document).await?;
        let arguments = command::write(plan, &path).map_err(|error| unusable(&error))?;

        let mut child = Command::new(&self.program)
            .args(&arguments)
            .current_dir(self.working_directory())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Deliberately not kill_on_drop: a dropped future must not stop a
            // write. The worker's recovery record accounts for a process that
            // outlives its caller.
            .spawn()
            .map_err(|source| EngineError::Io {
                operation: "starting cdrdao",
                source,
            })?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        // Everything cdrdao says is on stderr and is judged as it arrives.
        // stdout is drained alongside so it can never fill and stop the write.
        let (stdout, (outcome, log)) =
            tokio::join!(read_bounded(stdout), follow_write(stderr, sink));

        let status = child.wait().await.map_err(|source| EngineError::Io {
            operation: "waiting for cdrdao",
            source,
        })?;
        let exit_code = status.code();
        if !stdout.trim().is_empty() {
            tracing::debug!(stdout = %stdout, "cdrdao printed to stdout during a write");
        }
        tracing::debug!(log_bytes = log.len(), exit_code = ?exit_code, "cdrdao write finished");

        // The tool said it finished and nothing failed before it did. The exit
        // status is not part of this, for the reason the fixtures record:
        // cdrdao can say it finished and then exit 1 releasing the drive.
        let engine_reported_success = outcome.reported_complete && outcome.failures.is_empty();

        let mut diagnostics: Vec<String> = outcome.failures.clone();
        diagnostics.extend(outcome.late_problems.iter().cloned());
        if let Some(code) = exit_code
            && code != 0
        {
            diagnostics.push(format!("cdrdao exited with status {code}"));
        }
        if engine_reported_success && exit_code.is_some_and(|code| code != 0) {
            diagnostics.push(
                "the write was reported complete but the tool exited non-zero; the disc must be \
                 read back before it is believed"
                    .to_owned(),
            );
        }
        diagnostics.truncate(MAX_DIAGNOSTICS);

        Ok(WriteReport {
            engine_reported_success,
            bytes_written: if engine_reported_success {
                plan.total_bytes
            } else {
                // cdrdao's megabytes are 2^20 bytes.
                outcome
                    .last_progress
                    .map_or(0, |(written, _)| written.saturating_mul(1 << 20))
            },
            engine: self.name().to_owned(),
            engine_version: self.version(),
            finalized: plan.finalize && engine_reported_success,
            diagnostics,
        })
    }

    async fn verify(
        &self,
        _plan: &BurnPlan,
        _sink: &dyn EventSink,
    ) -> Result<VerifyReport, EngineError> {
        // Said rather than approximated. See the module documentation: a match
        // reported from here would be recorded as a full sector comparison.
        Err(EngineError::Unsupported {
            what: "reading a disc of tracks back is not built yet; the disc is recorded as \
                   written and unverified"
                .to_owned(),
        })
    }

    async fn blank(
        &self,
        request: &BlankRequest,
        _sink: &dyn EventSink,
    ) -> Result<BlankReport, EngineError> {
        // As with xorriso: erasing needs its own job type and policy check,
        // and neither exists yet, so it is refused loudly rather than done.
        if !request.confirmed_destructive {
            return Err(EngineError::DestructiveNotConfirmed);
        }
        Err(EngineError::Unsupported {
            what: "blanking is not implemented for cdrdao yet".to_owned(),
        })
    }

    async fn eject(&self, drive: &DriveRef) -> Result<(), EngineError> {
        let run = self.run(&command::eject(drive), PROBE_TIMEOUT).await?;
        let text = run.text();
        if parse::device_unavailable(&text) {
            return Err(EngineError::DriveUnavailable {
                alias: drive.device_alias.clone(),
            });
        }
        match parse::messages(&text)
            .into_iter()
            .find(|message| message.severity.is_failure())
        {
            Some(message) => Err(EngineError::Unsupported {
                what: format!("the drive would not eject: {}", message.text),
            }),
            None => Ok(()),
        }
    }
}

/// Hash a whole file.
async fn hash_file(path: &Path) -> Result<Sha256Digest, EngineError> {
    use sha2::Digest as _;

    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|source| EngineError::Io {
            operation: "opening a staged file",
            source,
        })?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = vec![0_u8; 1 << 20];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|source| EngineError::Io {
                operation: "reading a staged file",
                source,
            })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(Sha256Digest::from_bytes(hasher.finalize().into()))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::engine::CollectingSink;
    use crate::plan::{PlannedIndex, PlannedInput, PlannedTrack};
    use tangible_domain::cd::SampleByteOrder;
    use tangible_domain::{BurnAttemptId, DriveId, WorkerId};

    const RAW: u64 = 2352;

    fn fixture(name: &str) -> String {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/tool-output/cdrdao/"
        );
        std::fs::read_to_string(format!("{path}{name}"))
            .unwrap_or_else(|error| panic!("read {name}: {error}"))
    }

    fn track(number: u32, mode: &str, sectors: u64) -> PlannedTrack {
        PlannedTrack {
            number,
            session: 1,
            mode: mode.to_owned(),
            input: 0,
            file_offset_bytes: 0,
            start_lba: 0,
            sector_count: sectors,
            pregap_sectors: 0,
            indexes: vec![PlannedIndex {
                number: 1,
                relative_lba: 0,
            }],
            isrc: None,
            sample_byte_order: Some(SampleByteOrder::LittleEndian),
            flags: Vec::new(),
        }
    }

    fn plan(tracks: Vec<PlannedTrack>) -> BurnPlan {
        BurnPlan {
            attempt_id: BurnAttemptId::generate(),
            drive: DriveRef {
                worker_id: WorkerId::generate(),
                drive_id: DriveId::generate(),
                device_alias: "/dev/disc-block".to_owned(),
            },
            inputs: vec![PlannedInput {
                staged_path: PathBuf::from("/work/disc.bin"),
                sha256: Sha256Digest::from_bytes([0; 32]),
                length_bytes: RAW * 4000,
            }],
            tracks,
            catalog: None,
            mode: WriteMode::TocDiscAtOnce,
            accepted_profiles: vec!["CD-R".to_owned()],
            speed: None,
            finalize: true,
            eject_on_success: true,
            total_bytes: RAW * 4000,
        }
    }

    /// The plan `toc/mixed.toc` was written from: a data track, then audio
    /// after a generated two-second gap.
    fn mixed() -> BurnPlan {
        let mut audio = track(2, "AUDIO", 3000);
        audio.file_offset_bytes = RAW * 1000;
        audio.pregap_sectors = 150;
        plan(vec![track(1, "MODE2/2352", 1000), audio])
    }

    /// The plan `toc/flags.toc` was written from: the gap is in the file, and
    /// the audio track carries every flag and a recording code.
    fn flags() -> BurnPlan {
        let mut data = track(1, "MODE1/2352", 1000);
        data.flags = vec![TrackFlag::DigitalCopyPermitted];
        let mut audio = track(2, "AUDIO", 3000);
        audio.file_offset_bytes = RAW * 1000;
        audio.pregap_sectors = 150;
        audio.indexes = vec![
            PlannedIndex {
                number: 0,
                relative_lba: 0,
            },
            PlannedIndex {
                number: 1,
                relative_lba: 150,
            },
        ];
        audio.isrc = Some("usrc17607839".to_owned());
        audio.flags = vec![
            TrackFlag::DigitalCopyPermitted,
            TrackFlag::PreEmphasis,
            TrackFlag::FourChannel,
        ];
        plan(vec![data, audio])
    }

    // --- comparing cdrdao's reading with the plan -------------------------------

    #[test]
    fn the_plan_behind_a_captured_listing_agrees_with_it() {
        // The comparison run against real cdrdao output, for the plan the
        // table of contents was written from.
        let listing = parse::toc_listing(&fixture("show-toc-mixed.txt"));
        assert_eq!(disagreements(&mixed(), &listing), Vec::<String>::new());
    }

    #[test]
    fn a_gap_in_the_file_and_every_flag_agree_too() {
        let listing = parse::toc_listing(&fixture("show-toc-flags.txt"));
        assert_eq!(disagreements(&flags(), &listing), Vec::<String>::new());
    }

    #[test]
    fn the_writer_produces_what_the_captured_listing_was_read_from() {
        // Closes the loop: the documents in toc/ are the writer's output for
        // these plans, not hand-written approximations of it.
        for (plan, name) in [(mixed(), "toc/mixed.toc"), (flags(), "toc/flags.toc")] {
            assert_eq!(toc::write_toc(&plan).expect("a document"), fixture(name));
        }
    }

    #[test]
    fn a_track_that_ends_somewhere_else_is_a_disagreement() {
        // As if the writer had counted a gap twice, or a length wrong.
        let mut wrong = mixed();
        wrong.tracks[1].sector_count = 2999;
        let listing = parse::toc_listing(&fixture("show-toc-mixed.txt"));

        let found = disagreements(&wrong, &listing);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("track 2"), "{found:?}");
    }

    #[test]
    fn a_flag_cdrdao_did_not_read_is_a_disagreement() {
        let mut wrong = mixed();
        wrong.tracks[1].flags = vec![TrackFlag::PreEmphasis];
        let listing = parse::toc_listing(&fixture("show-toc-mixed.txt"));

        assert!(!disagreements(&wrong, &listing).is_empty());
    }

    #[test]
    fn a_different_number_of_tracks_is_said_once() {
        // Once, rather than as a disagreement for every track after the
        // first: once the counts differ, pairing tracks up means nothing.
        let mut short = mixed();
        short.tracks.pop();
        let listing = parse::toc_listing(&fixture("show-toc-mixed.txt"));

        assert_eq!(
            disagreements(&short, &listing),
            vec!["2 tracks were read, where the plan has 1".to_owned()]
        );
    }

    #[test]
    fn a_catalogue_number_is_compared() {
        let listing = parse::toc_listing(&fixture("show-toc-codes.txt"));
        let mut audio = track(1, "AUDIO", 3000);
        audio.isrc = Some("USRC17607839".to_owned());
        audio.indexes.push(PlannedIndex {
            number: 2,
            relative_lba: 150,
        });
        let mut codes = plan(vec![audio]);
        codes.catalog = Some("1234567890123".to_owned());
        assert_eq!(disagreements(&codes, &listing), Vec::<String>::new());

        codes.catalog = None;
        assert_eq!(disagreements(&codes, &listing).len(), 1);
    }

    // --- the medium ----------------------------------------------------------------

    #[test]
    fn a_blank_cd_r_is_writable_to_its_whole_capacity() {
        let medium = medium_info(&parse::disk_info(&fixture("disk-info-blank-cdr.txt")));
        assert_eq!(medium.profile, "CD-R");
        assert!(medium.blank);
        assert!(medium.writable);
        assert!(!medium.erasable);
        assert_eq!(medium.free_blocks, 359_846);
    }

    #[test]
    fn a_closed_disc_has_nothing_free() {
        let medium = medium_info(&parse::disk_info(&fixture(
            "assembled/disk-info-closed-cdrw.txt",
        )));
        assert_eq!(medium.profile, "CD-RW");
        assert!(!medium.blank);
        assert!(!medium.writable);
        assert!(medium.erasable);
        assert_eq!(medium.sessions, 1);
    }

    #[test]
    fn an_appendable_disc_is_not_blank_even_with_room_left() {
        // cdrdao writes a whole disc at once and does not erase first, so
        // preflight refuses this on blankness whatever the free space says.
        let medium = medium_info(&parse::disk_info(&fixture(
            "assembled/disk-info-appendable-cdr.txt",
        )));
        assert!(!medium.blank);
        assert_eq!(medium.free_blocks, 348_449);
    }

    #[test]
    fn a_drive_that_will_not_say_what_the_disc_is_does_not_get_a_guess() {
        let medium = medium_info(&parse::DiskInfo {
            empty: Some(true),
            capacity_blocks: Some(1000),
            ..parse::DiskInfo::default()
        });
        assert_eq!(medium.profile, "unknown");
    }

    // --- streaming ------------------------------------------------------------------

    #[test]
    fn a_carriage_return_ends_a_line_as_it_arrives() {
        let mut splitter = LineSplitter::default();
        let mut seen = Vec::new();
        let mut on_line = |line: &str| seen.push(line.to_owned());
        splitter.push(b"Wrote 1 of 9 MB.\rWrote 2 of", &mut on_line);
        splitter.push(
            b" 9 MB.\rWriting track 02 (mode AUDIO/AUDIO )...\n",
            &mut on_line,
        );
        splitter.flush(&mut on_line);
        assert_eq!(
            seen,
            vec![
                "Wrote 1 of 9 MB.",
                "Wrote 2 of 9 MB.",
                "Writing track 02 (mode AUDIO/AUDIO )..."
            ]
        );
    }

    #[test]
    fn a_line_that_never_ends_does_not_grow_without_limit() {
        let mut splitter = LineSplitter::default();
        let mut longest = 0;
        let mut on_line = |line: &str| longest = longest.max(line.len());
        splitter.push(&vec![b'x'; MAX_LINE_BYTES * 3], &mut on_line);
        splitter.flush(&mut on_line);
        assert_eq!(longest, MAX_LINE_BYTES);
    }

    #[tokio::test]
    async fn a_write_is_followed_and_reported_as_it_happens() {
        // The first real burn, as the drive printed it.
        let output = fixture("write-success.txt");
        let sink = CollectingSink::new();
        let (outcome, log) = follow_write(Some(output.as_bytes()), &sink).await;

        assert!(outcome.reported_complete);
        assert!(!log.is_empty());
        let codes: Vec<&str> = sink.events().iter().map(|event| event.code).collect();
        assert!(codes.contains(&"TRACK_STARTED"), "{codes:?}");
        assert!(codes.contains(&"WRITE_LEADOUT"), "{codes:?}");
        assert_eq!(codes.last(), Some(&"WRITE_COMPLETED"), "{codes:?}");
        let progress = codes
            .iter()
            .filter(|code| **code == "WRITE_PROGRESS")
            .count();
        assert_eq!(progress, 7, "one per megabyte: {codes:?}");
    }

    #[tokio::test]
    async fn the_outcome_survives_a_log_that_was_truncated() {
        // The completion line comes last, which is the part a bounded log
        // loses. The outcome is judged from every line, so it is still found.
        let mut output = "Wrote 1 of 9 MB (Buffers 100%  95%).\n".repeat(MAX_LOG_BYTES / 20);
        output.push_str("Writing finished successfully.\n");
        let (outcome, log) = follow_write(Some(output.as_bytes()), &CollectingSink::new()).await;

        assert!(log.ends_with("[output truncated]\n"));
        assert!(outcome.reported_complete);
    }

    #[tokio::test]
    async fn a_bounded_read_keeps_draining_after_the_cap() {
        let big = vec![b'x'; MAX_LOG_BYTES * 2];
        let text = read_bounded(Some(big.as_slice())).await;
        assert!(text.len() < MAX_LOG_BYTES + 100);
        assert!(text.contains("bytes dropped"));
    }

    // --- the engine without a binary --------------------------------------------------

    #[tokio::test]
    async fn an_engine_reports_the_name_it_is_and_only_writes_tracks() {
        let engine = CdrdaoEngine::new("/var/lib/tangible-worker/toc");
        assert_eq!(engine.name(), "cdrdao");
        assert_eq!(engine.version(), "unknown");
        assert!(engine.uses_hardware());
        assert!(engine.supports_mode(WriteMode::TocDiscAtOnce));
        assert!(!engine.supports_mode(WriteMode::DataDiscAtOnce));
    }

    #[tokio::test]
    async fn a_missing_binary_is_an_io_error_rather_than_a_panic() {
        let engine = CdrdaoEngine::at("/nonexistent/cdrdao", std::env::temp_dir());
        let error = engine
            .run(&[], Duration::from_secs(5))
            .await
            .expect_err("must fail");
        assert!(matches!(error, EngineError::Io { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn a_plan_the_writer_refuses_never_reaches_the_tool() {
        // The binary does not exist, so reaching it would be an error rather
        // than this refusal.
        let engine = CdrdaoEngine::at("/nonexistent/cdrdao", std::env::temp_dir());
        let mut silent = mixed();
        silent.tracks[1].sample_byte_order = None;

        let failures = engine
            .check_table_of_contents(&silent)
            .await
            .expect("a refusal, not an error");
        assert!(matches!(
            failures.as_slice(),
            [PreflightFailure::TableOfContentsRefused { .. }]
        ));
    }

    #[tokio::test]
    async fn a_relative_working_directory_is_refused() {
        let engine = CdrdaoEngine::at("/nonexistent/cdrdao", "relative/toc");
        let error = engine
            .check_table_of_contents(&mixed())
            .await
            .expect_err("must fail");
        assert!(
            matches!(error, EngineError::Unsupported { .. }),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn verification_says_it_was_not_done() {
        // A match from here would be recorded as a full sector comparison.
        let engine = CdrdaoEngine::new("/var/lib/tangible-worker/toc");
        let error = engine
            .verify(&mixed(), &CollectingSink::new())
            .await
            .expect_err("not built");
        assert!(matches!(error, EngineError::Unsupported { .. }));
    }

    #[tokio::test]
    async fn blanking_is_refused() {
        let engine = CdrdaoEngine::new("/var/lib/tangible-worker/toc");
        let mut request = BlankRequest {
            drive: mixed().drive,
            full: false,
            confirmed_destructive: false,
        };
        assert!(matches!(
            engine.blank(&request, &CollectingSink::new()).await,
            Err(EngineError::DestructiveNotConfirmed)
        ));
        request.confirmed_destructive = true;
        assert!(matches!(
            engine.blank(&request, &CollectingSink::new()).await,
            Err(EngineError::Unsupported { .. })
        ));
    }
}
