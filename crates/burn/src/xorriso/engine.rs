// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Running xorriso.
//!
//! The adapter that turns the pure halves (the argument builder and the
//! output parser) into a [`BurnEngine`]. What it adds is process handling,
//! and process handling around a program that can destroy a disc has rules of
//! its own:
//!
//! - **A write is never interrupted.** Cancellation is honoured while probing,
//!   inspecting and verifying, and deliberately ignored once the laser is
//!   running: stopping mid-write guarantees a ruined disc, whereas letting it
//!   finish may not.
//! - **Output is bounded.** A tool that decides to print a line per block must
//!   not exhaust the worker's memory, so what is retained is capped and the
//!   rest is counted rather than kept.
//! - **stdout and stderr are read at the same time.** A process whose pipe
//!   fills because nobody is draining it stops, and a stopped process
//!   mid-write is the thing above. Reading one to its end before starting the
//!   other only works while the second never fills.
//!
//! # Reading the disc back
//!
//! Verification hashes what the drive returns and compares it with the plan's
//! digest; it does not ask the tool whether it thinks the disc is fine. The
//! reading, though, goes through xorriso's `-check_media`, over SCSI, and not
//! through the kernel's block device. The first real drive is why: after a
//! burn the kernel still believed the device held a blank disc's two
//! kilobytes, because writing over SCSI raises no media change, and reading
//! the block device returned exactly that much of a correct disc.
//!
//! `-check_media` writes what it reads into a regular file at each block's own
//! position, so the disc is read in chunks into one scratch file, emptied
//! between chunks. Written at an offset into an emptied file, a chunk leaves a
//! hole before it rather than data, so the scratch file never holds more than
//! a chunk however large the disc.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tangible_domain::Sha256Digest;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader};
use tokio::process::Command;

use crate::engine::{BurnEngine, BurnEvent, EngineError, EventSink};
use crate::fake::CancelToken;
use crate::plan::{
    BlankReport, BlankRequest, BurnPlan, DriveCapabilities, DriveRef, MediumInfo, PreflightFailure,
    PreflightReport, VerifyReport, WriteMode, WriteReport,
};
use crate::xorriso::{command, parse};

/// How long a probe or an inspection may take.
///
/// Short: these talk to a drive that is either there or is not. A drive that
/// takes a minute to answer a capability question is a drive an operator
/// should hear about rather than wait for.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// How much of the disc one `-check_media` run reads.
///
/// The scratch file holds at most this much. Larger chunks mean fewer runs,
/// each of which acquires the drive afresh; a CD is three, a single-layer
/// Blu-ray about a hundred.
const VERIFY_CHUNK_BLOCKS: u64 = 256 * 1024 * 1024 / BLOCK_BYTES;

/// Bytes in a data block, which is what `-check_media` counts in.
const BLOCK_BYTES: u64 = 2048;

/// How long one chunk of read-back may take.
///
/// Generous: a quarter of a gigabyte at single speed from a disc that needs
/// retries is several minutes. Unlike a write, a read that is stopped ruins
/// nothing, so it can have a limit at all.
const VERIFY_CHUNK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How much tool output is retained.
///
/// Bounded because a tool that decides to print a line per block must not
/// exhaust a worker's memory. What is dropped is counted, so a truncated log
/// says it is truncated rather than merely being short.
const MAX_LOG_BYTES: usize = 256 * 1024;

/// The xorriso burn engine.
#[derive(Debug, Clone)]
pub struct XorrisoEngine {
    program: PathBuf,
    version: String,
    cancel: CancelToken,
    scratch_dir: PathBuf,
    chunk_blocks: u64,
}

impl XorrisoEngine {
    /// An engine that runs `xorriso` from `PATH`.
    ///
    /// The version is not probed here: constructing an engine must not require
    /// a working tool, or a worker with a misconfigured image could not start
    /// and say so.
    #[must_use]
    pub fn new() -> Self {
        Self::at(command::XORRISO)
    }

    /// An engine that runs a specific binary.
    ///
    /// For a worker whose image pins a path, and for tests.
    #[must_use]
    pub fn at(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            version: "unknown".to_owned(),
            cancel: CancelToken::new(),
            scratch_dir: std::env::temp_dir(),
            chunk_blocks: VERIFY_CHUNK_BLOCKS,
        }
    }

    /// Read back in chunks of this many blocks instead of a quarter of a
    /// gigabyte.
    ///
    /// For tests, which need to cross chunk boundaries without writing a
    /// quarter of a gigabyte first. A chunk is at least one block.
    #[must_use]
    pub fn with_read_back_chunk(mut self, blocks: u64) -> Self {
        self.chunk_blocks = blocks.max(1);
        self
    }

    /// Where verification keeps its scratch file.
    ///
    /// It holds up to a quarter of a gigabyte at a time. The system temporary
    /// directory unless set; a worker points it at its own state volume.
    #[must_use]
    pub fn with_scratch_dir(mut self, scratch_dir: impl Into<PathBuf>) -> Self {
        self.scratch_dir = scratch_dir.into();
        self
    }

    /// Attach a cancellation token.
    #[must_use]
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Ask the tool what version it is and remember the answer.
    ///
    /// Called at startup by a worker reporting capabilities. Failing here
    /// means the binary is missing or is not xorriso, which is worth an
    /// operator's attention rather than a burn attempt.
    ///
    /// # Errors
    ///
    /// [`EngineError::Unsupported`] if the binary does not identify itself as
    /// xorriso, or [`EngineError::Io`] if it cannot be run at all.
    pub async fn probe_version(&mut self) -> Result<parse::Version, EngineError> {
        let output = self.run(&command::version(), PROBE_TIMEOUT).await?;
        let version = parse::version(&output.text).ok_or_else(|| EngineError::Unsupported {
            what: "the configured binary does not identify itself as xorriso".to_owned(),
        })?;
        self.version.clone_from(&version.version);
        Ok(version)
    }

    /// Run the tool and collect what it said.
    async fn run(&self, arguments: &[String], timeout: Duration) -> Result<Run, EngineError> {
        let mut child = Command::new(&self.program)
            .args(arguments)
            // Explicit, and somewhere harmless: a tool that writes a temporary
            // file relative to its working directory must not scatter it
            // through the worker's state.
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| EngineError::Io {
                operation: "starting xorriso",
                source,
            })?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // Both pipes drained together. A process whose pipe fills because
        // nobody is reading it stops, and a stopped process holding a drive is
        // worse than a slow one.
        let finished = tokio::time::timeout(timeout, async {
            let (stdout, stderr) = tokio::join!(read_all(stdout), read_all(stderr));
            (stdout, stderr, child.wait().await)
        })
        .await;

        let Ok((stdout, stderr, status)) = finished else {
            // A probe that will not answer is killed. This is never reached
            // for a write, which has no timeout for exactly the reason a
            // probe does.
            let _ = child.kill().await;
            return Err(EngineError::Timeout {
                operation: "xorriso",
                seconds: timeout.as_secs(),
            });
        };
        let status = status.map_err(|source| EngineError::Io {
            operation: "waiting for xorriso",
            source,
        })?;

        let mut text = String::new();
        push_bounded(&mut text, &stdout);
        push_bounded(&mut text, &stderr);
        let run = Run {
            text,
            exit_code: status.code(),
        };
        // Logged rather than acted on. A probe can exit non-zero and still
        // have answered (`-devices` on a machine with no drives does exactly
        // that), so the exit code is context for a human reading logs, and
        // the parsed output is what the caller decides from.
        tracing::debug!(
            program = %self.program.display(),
            exit_code = ?run.exit_code,
            "xorriso finished"
        );
        Ok(run)
    }

    fn check_cancelled(&self) -> Result<(), EngineError> {
        if self.cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        Ok(())
    }
}

impl Default for XorrisoEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// What one run of the tool produced.
#[derive(Debug)]
struct Run {
    text: String,
    exit_code: Option<i32>,
}

/// Read a pipe to its end. Unbounded here and bounded by the caller, which is
/// fine for probes: their output is a screenful.
async fn read_all<R: tokio::io::AsyncRead + Unpin>(pipe: Option<R>) -> String {
    let mut text = String::new();
    if let Some(mut pipe) = pipe {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes).await;
        text = String::from_utf8_lossy(&bytes).into_owned();
    }
    text
}

/// Read a pipe line by line as it arrives, reporting each line and keeping a
/// bounded copy.
async fn follow_lines<R: tokio::io::AsyncRead + Unpin>(
    pipe: Option<R>,
    sink: &dyn EventSink,
) -> String {
    let mut text = String::new();
    let mut last_mb = None;
    if let Some(pipe) = pipe {
        let mut lines = BufReader::new(pipe).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            emit(sink, &line, &mut last_mb);
            push_bounded(&mut text, &line);
            push_bounded(&mut text, "\n");
        }
    }
    text
}

/// Append to a bounded log, saying so when something is dropped.
fn push_bounded(log: &mut String, addition: &str) {
    if log.len() >= MAX_LOG_BYTES {
        return;
    }
    let room = MAX_LOG_BYTES - log.len();
    if addition.len() <= room {
        log.push_str(addition);
        return;
    }

    // On a character boundary: a log cut mid-codepoint is not text any more.
    let mut end = room;
    while end > 0 && !addition.is_char_boundary(end) {
        end -= 1;
    }
    log.push_str(&addition[..end]);
    log.push_str("\n[output truncated]\n");
}

/// Turn a medium report into the shape the rest of the system speaks.
fn medium_info(report: &parse::MediumReport) -> MediumInfo {
    let block_size = 2048;
    MediumInfo {
        profile: report
            .profile
            .clone()
            .unwrap_or_else(|| "unknown".to_owned()),
        blank: report.blank,
        // A medium that can be written to at all. xorriso reports what is
        // writable; nothing is inferred from the profile name, because a
        // profile says what a disc is and not what this drive can do with it.
        writable: report.writable_blocks.is_some_and(|blocks| blocks > 0),
        rewritable: report
            .profile
            .as_deref()
            .is_some_and(|profile| profile.contains("RW") || profile.contains("RE")),
        capacity_blocks: report
            .writable_blocks
            .unwrap_or(0)
            .saturating_add(report.readable_blocks.unwrap_or(0)),
        free_blocks: report.writable_blocks.unwrap_or(0),
        block_size,
        // The disc's maker, which is what a physical copy records. The drive's
        // identity was recorded here until the first real drive showed it.
        manufacturer_id: report.media_manufacturer.clone(),
        sessions: report.sessions.unwrap_or(0),
        erasable: report
            .profile
            .as_deref()
            .is_some_and(|profile| profile.contains("RW") || profile.contains("RE")),
        status_warnings: Vec::new(),
    }
}

#[async_trait]
impl BurnEngine for XorrisoEngine {
    fn name(&self) -> &'static str {
        "xorriso"
    }

    fn version(&self) -> String {
        self.version.clone()
    }

    /// Always, including against a file target: two runs writing one target
    /// ruin it whether or not the target spins.
    fn uses_hardware(&self) -> bool {
        true
    }

    /// Data images only.
    ///
    /// xorriso writes a prepared image to a disc. It does not write a table of
    /// contents with per-track modes, audio tracks and exact pregaps, which is
    /// what cdrdao is for and why there are two engines rather than one.
    fn supports_mode(&self, mode: WriteMode) -> bool {
        matches!(mode, WriteMode::DataDiscAtOnce | WriteMode::DataTrackAtOnce)
    }

    async fn probe_drive(&self, drive: &DriveRef) -> Result<DriveCapabilities, EngineError> {
        self.check_cancelled()?;
        let run = self.run(&command::devices(), PROBE_TIMEOUT).await?;
        let devices = parse::devices(&run.text);

        // What the drive says it is, recorded as evidence rather than as
        // permission. Profiles are left empty until a medium is inspected:
        // claiming a drive writes BD-R because its model number suggests so
        // is exactly the guess preflight exists to avoid.
        let mut capabilities = DriveCapabilities::default();
        if let Some(found) = devices
            .iter()
            .find(|device| device.path == drive.device_alias)
        {
            let identity = [found.vendor.clone(), found.model.clone()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            capabilities
                .engine_evidence
                .insert("xorriso".to_owned(), identity);
        } else {
            capabilities.engine_evidence.insert(
                "xorriso".to_owned(),
                format!(
                    "{} was not among the drives xorriso listed",
                    drive.device_alias
                ),
            );
        }
        Ok(capabilities)
    }

    async fn inspect_medium(&self, drive: &DriveRef) -> Result<MediumInfo, EngineError> {
        self.check_cancelled()?;
        let run = self.run(&command::inspect(drive), PROBE_TIMEOUT).await?;

        // Checked before anything is concluded about a disc. A drive xorriso
        // refused or could not acquire says nothing about what is in it, and
        // reading that as an empty tray is how the first end-to-end run's
        // worker sat waiting for a disc that was already there: the problem
        // was the address, and only an operator can fix an address.
        if let Some(reason) = parse::drive_refused(&run.text) {
            tracing::error!(alias = %drive.device_alias, %reason, "xorriso refused the drive");
            return Err(EngineError::DriveUnavailable {
                alias: drive.device_alias.clone(),
            });
        }

        let report = parse::medium(&run.text);
        if !report.present() {
            return Err(EngineError::NoMedium {
                alias: drive.device_alias.clone(),
            });
        }
        Ok(medium_info(&report))
    }

    async fn preflight(&self, plan: &BurnPlan) -> Result<PreflightReport, EngineError> {
        self.check_cancelled()?;

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
        let mut warnings = Vec::new();

        if !medium.writable {
            failures.push(PreflightFailure::MediumNotWritable {
                profile: medium.profile.clone(),
            });
        }
        if medium.sessions > 0 && !medium.erasable {
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
        // Sectors for a track layout, bytes for a block image. The plan knows
        // which it is; an engine counting bytes would call a full CD oversized
        // by a sixth because its raw sectors are larger than the blocks the
        // profile reports.
        failures.extend(plan.capacity_failures(&medium));

        // The check that makes "hash verified before write" a fact rather than
        // an intention. Bytes staged an hour ago may not be the bytes on disk
        // now, and the whole point of a burn is that the disc holds what the
        // library holds.
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

        if command::write(plan).is_none() {
            failures.push(PreflightFailure::DriveCannotWriteProfile {
                profile: "this engine writes one prepared image; a track layout needs cdrdao"
                    .to_owned(),
            });
        }
        if medium.sessions > 0 && medium.erasable {
            warnings.push("the medium holds data and will be erased before writing".to_owned());
        }

        Ok(PreflightReport {
            failures,
            warnings,
            medium: Some(medium),
        })
    }

    async fn write(
        &self,
        plan: &BurnPlan,
        sink: &dyn EventSink,
    ) -> Result<WriteReport, EngineError> {
        // Checked here and never again. Past this point the laser may be
        // running, and stopping guarantees a ruined disc where finishing may
        // not.
        self.check_cancelled()?;

        let arguments = command::write(plan).ok_or_else(|| EngineError::Unsupported {
            what: "this engine writes a single prepared image".to_owned(),
        })?;

        let mut child = Command::new(&self.program)
            .args(&arguments)
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Deliberately not kill_on_drop: a dropped future must not stop a
            // write. The worker's recovery record is what accounts for a
            // process that outlives its caller.
            .spawn()
            .map_err(|source| EngineError::Io {
                operation: "starting xorriso",
                source,
            })?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // Both read line by line as they come, and at the same time. A write
        // is minutes long; a progress bar that only moves at the end is not
        // one, and a pipe left unread until the other closes can fill and
        // stop the write. In cdrecord mode everything xorriso says, progress,
        // completion and every severity line, is on stderr, so reading the two
        // together changes nothing about the order the outcome is judged in.
        let (stderr_text, stdout_text) =
            tokio::join!(follow_lines(stderr, sink), follow_lines(stdout, sink));
        let mut text = stderr_text;
        push_bounded(&mut text, &stdout_text);

        let status = child.wait().await.map_err(|source| EngineError::Io {
            operation: "waiting for xorriso",
            source,
        })?;

        let outcome = parse::write_outcome(&text);
        let exit_code = status.code();

        // Both facts, combined here and nowhere else. The tool's own account
        // of the write and its exit status can disagree (the fixtures
        // include a run that wrote the disc and then crashed on shutdown), so
        // success requires the tool to have said it completed *and* nothing
        // to have been reported at a severity that stops a burn.
        let engine_reported_success = outcome.reported_complete && outcome.failures.is_empty();

        let mut diagnostics: Vec<String> = outcome.failures.clone();
        diagnostics.extend(outcome.late_problems.iter().cloned());
        if let Some(code) = exit_code
            && code != 0
        {
            diagnostics.push(format!("xorriso exited with status {code}"));
        }
        if engine_reported_success && exit_code.is_some_and(|code| code != 0) {
            // Recorded rather than resolved. The tool wrote the data and then
            // had trouble finishing; whether the disc is good is a question
            // for the read-back, which is the only thing that can answer it.
            diagnostics.push(
                "the write was reported complete but the tool exited non-zero; the disc must be \
                 read back before it is believed"
                    .to_owned(),
            );
        }

        Ok(WriteReport {
            engine_reported_success,
            bytes_written: if engine_reported_success {
                plan.total_bytes
            } else {
                // What the last progress line said, in bytes. An unfinished
                // write reporting the full length would be a lie about how
                // much of the medium was consumed.
                outcome
                    .last_progress
                    .map_or(0, |(written, _)| written.saturating_mul(1_000_000))
            },
            engine: self.name().to_owned(),
            engine_version: self.version(),
            finalized: plan.finalize && engine_reported_success,
            diagnostics,
        })
    }

    async fn verify(
        &self,
        plan: &BurnPlan,
        sink: &dyn EventSink,
    ) -> Result<VerifyReport, EngineError> {
        self.check_cancelled()?;

        let expected = plan
            .inputs
            .first()
            .ok_or_else(|| EngineError::Unsupported {
                what: "nothing to verify against".to_owned(),
            })?
            .sha256;

        sink.emit(BurnEvent::new(
            "verifying",
            "VERIFY_STARTED",
            "reading the medium back",
        ));

        let scratch = self
            .scratch_dir
            .join(format!("{}.readback", plan.attempt_id));
        let result = self.read_back(plan, &scratch, sink).await;
        // Always, including after an error: a quarter of a gigabyte left in a
        // worker's state per failed verification adds up.
        let _ = tokio::fs::remove_file(&scratch).await;
        let read = result?;

        let matched = read.unreadable_at.is_none()
            && read.digest == expected
            && read.bytes == plan.total_bytes;
        sink.emit(BurnEvent::new(
            "verifying",
            if matched {
                "VERIFY_MATCHED"
            } else {
                "VERIFY_MISMATCH"
            },
            match read.unreadable_at {
                Some(offset) => format!(
                    "read back {} bytes; the drive could not read the block at byte {offset}",
                    read.bytes
                ),
                None => format!("read back {} bytes", read.bytes),
            },
        ));

        Ok(VerifyReport {
            matched,
            bytes_compared: read.bytes,
            method: "full_sector_readback".to_owned(),
            first_mismatch_offset: read.unreadable_at,
            // Always populated: a verification result that does not state its
            // limits invites being read as a stronger guarantee than it is.
            limitations: vec![
                "compares the written extent only; padding beyond the image is not read".to_owned(),
                "reads through this drive, so a disc unreadable elsewhere can still match here"
                    .to_owned(),
            ],
            tracks: Vec::new(),
        })
    }

    async fn blank(
        &self,
        request: &BlankRequest,
        _sink: &dyn EventSink,
    ) -> Result<BlankReport, EngineError> {
        // Refused rather than implemented for now, and refused loudly. Erasing
        // is destructive, and an engine that quietly did nothing would be
        // worse than one that says it cannot.
        if !request.confirmed_destructive {
            return Err(EngineError::Unsupported {
                what: "blanking requires explicit confirmation".to_owned(),
            });
        }
        Err(EngineError::Unsupported {
            what: "blanking is not implemented for xorriso yet".to_owned(),
        })
    }

    async fn eject(&self, drive: &DriveRef) -> Result<(), EngineError> {
        let arguments = vec![
            "-abort_on".to_owned(),
            "FATAL".to_owned(),
            "-outdev".to_owned(),
            if drive.device_alias.starts_with("/dev/") {
                drive.device_alias.clone()
            } else {
                format!("stdio:{}", drive.device_alias)
            },
            "-eject".to_owned(),
            "all".to_owned(),
        ];
        self.run(&arguments, PROBE_TIMEOUT).await.map(|_| ())
    }
}

/// What reading a disc back produced.
struct ReadBack {
    /// Digest of every byte read, in order.
    digest: Sha256Digest,
    /// Bytes read.
    bytes: u64,
    /// Byte offset of the first block the drive reported it could not read.
    unreadable_at: Option<u64>,
}

impl XorrisoEngine {
    /// Read the written extent of the medium back through xorriso, a chunk at
    /// a time, hashing as it goes.
    async fn read_back(
        &self,
        plan: &BurnPlan,
        scratch: &Path,
        sink: &dyn EventSink,
    ) -> Result<ReadBack, EngineError> {
        use sha2::Digest as _;

        let length = plan.total_bytes;
        let blocks = length.div_ceil(BLOCK_BYTES);
        let mut hasher = sha2::Sha256::new();
        let mut read_total = 0_u64;
        let mut unreadable_at = None;

        let mut first = 0_u64;
        while first < blocks {
            // Between chunks, never within one: a read that is stopped ruins
            // nothing, so verification is cancellable where a write is not.
            self.check_cancelled()?;
            let last = (first + self.chunk_blocks).min(blocks) - 1;

            // Emptied before each chunk, so what it holds is only this chunk.
            tokio::fs::write(scratch, b"")
                .await
                .map_err(|source| EngineError::Io {
                    operation: "preparing the read-back file",
                    source,
                })?;
            let arguments =
                command::verify(&plan.drive, first, last, scratch).ok_or_else(|| {
                    EngineError::Unsupported {
                        what: format!(
                            "the scratch directory {} is not an absolute path",
                            self.scratch_dir.display()
                        ),
                    }
                })?;
            let run = self.run(&arguments, VERIFY_CHUNK_TIMEOUT).await?;

            // The drive's own account of which blocks it could read. A block it
            // could not read is left as a hole in the file, which would hash
            // as zeroes; the mismatch that produces is real, and this says
            // where it is.
            if unreadable_at.is_none()
                && let Some(region) = parse::media_regions(&run.text)
                    .iter()
                    .filter(|region| region.overlaps(first, last))
                    .find(|region| !region.read_ok())
            {
                unreadable_at = Some(region.lba.max(first) * BLOCK_BYTES);
            }

            let start = first * BLOCK_BYTES;
            let end = ((last + 1) * BLOCK_BYTES).min(length);
            let read = hash_range(scratch, start, end, &mut hasher).await?;
            read_total += read;
            if read < end - start {
                // The drive returned less than was asked for. What is missing
                // was not read, and the digest will say so.
                break;
            }

            #[allow(clippy::cast_precision_loss)]
            let fraction = read_total as f32 / length.max(1) as f32;
            sink.emit(BurnEvent::progress(
                "verifying",
                "VERIFY_PROGRESS",
                fraction,
                format!("read {read_total} of {length} bytes"),
            ));
            first = last + 1;
        }

        Ok(ReadBack {
            digest: Sha256Digest::from_bytes(hasher.finalize().into()),
            bytes: read_total,
            unreadable_at,
        })
    }
}

/// Feed bytes `start` to `end` of a file into a hasher, returning how many
/// there were.
async fn hash_range(
    path: &Path,
    start: u64,
    end: u64,
    hasher: &mut sha2::Sha256,
) -> Result<u64, EngineError> {
    use sha2::Digest as _;
    use tokio::io::AsyncSeekExt as _;

    let io = |operation: &'static str| {
        move |source: std::io::Error| EngineError::Io { operation, source }
    };
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(io("opening the read-back file"))?;
    file.seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(io("seeking in the read-back file"))?;

    let mut buffer = vec![0_u8; 1 << 20];
    let mut total = 0_u64;
    while total < end - start {
        let want = usize::try_from((end - start - total).min(buffer.len() as u64)).unwrap_or(0);
        let read = file
            .read(&mut buffer[..want])
            .await
            .map_err(io("reading the read-back file"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
    }
    Ok(total)
}

/// Emit an engine event for a line of tool output, when it carries one.
///
/// xorriso prints its progress line once a second whether or not anything
/// moved, and a "Thank you for being patient" line while the drive is busy.
/// The first real burn sent an operator "0 of 4 MB" fifteen times and the
/// patience line seven. Progress is reported when the megabyte count changes,
/// and UPDATE lines that are not progress stay in the log and off the page.
fn emit(sink: &dyn EventSink, line: &str, last_mb: &mut Option<u64>) {
    match parse::write_outcome(line).events.first() {
        Some(parse::WriteEvent::Progress {
            written_mb,
            total_mb,
            speed,
        }) => {
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
                match speed {
                    Some(speed) => format!("{written_mb} of {total_mb} MB at {speed}"),
                    None => format!("{written_mb} of {total_mb} MB"),
                },
            ));
        }
        Some(parse::WriteEvent::Completed) => sink.emit(BurnEvent::new(
            "writing",
            "WRITE_COMPLETED",
            "the engine reported the write finished",
        )),
        Some(parse::WriteEvent::Problem { severity, .. }) if severity == "UPDATE" => {}
        Some(parse::WriteEvent::Problem { severity, message }) => sink.emit(BurnEvent::new(
            "writing",
            "ENGINE_MESSAGE",
            format!("{severity}: {message}"),
        )),
        None => {}
    }
}

/// Hash a whole file.
async fn hash_file(path: &std::path::Path) -> Result<Sha256Digest, EngineError> {
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

    #[test]
    fn a_bounded_log_says_when_it_dropped_something() {
        let mut log = String::new();
        push_bounded(&mut log, &"x".repeat(MAX_LOG_BYTES * 2));
        assert!(log.len() <= MAX_LOG_BYTES + 32);
        assert!(log.ends_with("[output truncated]\n"));
    }

    #[test]
    fn a_bounded_log_never_cuts_a_character_in_half() {
        // A log cut mid-codepoint is not text any more.
        let mut log = "x".repeat(MAX_LOG_BYTES - 3);
        push_bounded(&mut log, "ééé");
        assert!(log.is_char_boundary(log.len()));
    }

    #[test]
    fn a_written_medium_is_read_as_not_blank_and_writable() {
        let report = parse::medium(
            "Media current: CD-RW\n\
             Media status : is written , is appendable\n\
             Media blocks : 1000 readable , 200000 writable , 201000 overall\n\
             Media summary: 1 session, 1000 data blocks, 2000k data, 400m free\n",
        );
        let medium = medium_info(&report);
        assert_eq!(medium.profile, "CD-RW");
        assert!(!medium.blank);
        assert!(medium.writable);
        // RW media can be erased, which is what lets a used one be reused.
        assert!(medium.erasable);
        assert_eq!(medium.free_blocks, 200_000);
        assert_eq!(medium.sessions, 1);
    }

    #[test]
    fn a_write_once_medium_is_not_reported_as_erasable() {
        let report = parse::medium(
            "Media current: CD-R\n\
             Media status : is blank\n\
             Media blocks : 0 readable , 360000 writable , 360000 overall\n",
        );
        let medium = medium_info(&report);
        assert!(!medium.erasable);
        assert!(medium.blank);
        assert!(medium.writable);
    }

    #[test]
    fn a_medium_with_nothing_writable_is_not_writable() {
        // A pressed disc in a burner. Reporting it as writable would send a
        // burn at something that cannot take one.
        let report = parse::medium(
            "Media current: CD-ROM\n\
             Media status : is written\n\
             Media blocks : 300000 readable , 0 writable , 300000 overall\n",
        );
        assert!(!medium_info(&report).writable);
    }

    #[test]
    fn progress_lines_become_engine_events() {
        let sink = CollectingSink::new();
        emit(
            &sink,
            "xorriso : UPDATE :   92 of  115 MB written (fifo  0%) [buf  50%]  59.7x.",
            &mut None,
        );
        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code, "WRITE_PROGRESS");
        let progress = events[0].progress.expect("a fraction");
        assert!((progress - 0.8).abs() < 0.02, "{progress}");
        assert!(events[0].message.contains("59.7x"));
    }

    #[test]
    fn a_completion_line_becomes_its_own_event() {
        let sink = CollectingSink::new();
        emit(
            &sink,
            "Writing to 'stdio:/tmp/x.iso' completed successfully.",
            &mut None,
        );
        assert_eq!(sink.events()[0].code, "WRITE_COMPLETED");
    }

    #[test]
    fn an_ordinary_line_emits_nothing() {
        let sink = CollectingSink::new();
        emit(
            &sink,
            "Drive current: -outdev 'stdio:/tmp/x.iso'",
            &mut None,
        );
        assert!(sink.events().is_empty());
    }

    #[test]
    fn the_real_burn_reports_each_megabyte_once_and_no_chatter() {
        // The first real xorriso burn, as the drive printed it.
        let output = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/tool-output/xorriso/write-success-cdr.txt"
        ))
        .expect("fixture");
        let sink = CollectingSink::new();
        let mut last_mb = None;
        for line in output.lines() {
            emit(&sink, line, &mut last_mb);
        }
        let events = sink.events();
        let progress: Vec<&str> = events
            .iter()
            .filter(|event| event.code == "WRITE_PROGRESS")
            .map(|event| event.message.as_str())
            .collect();
        assert_eq!(progress.len(), 3, "0, 2 and 4 MB: {progress:?}");
        assert!(
            !events
                .iter()
                .any(|event| event.message.contains("Thank you for being patient")),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| event.message.contains("WRITE command repetition")),
            "a NOTE still reaches the operator: {events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e.code == "WRITE_COMPLETED")
                .count(),
            1
        );
    }

    #[test]
    fn the_disc_maker_is_recorded_rather_than_the_drive() {
        let report = parse::medium(
            "Media current: CD-R\n\
             Media status : is blank\n\
             Media blocks : 0 readable , 359844 writable , 359844 overall\n\
             Drive type   : vendor 'Slimtype' product 'DVD A DS8A8SH' revision 'KS21'\n\
             Media product: 97m26s66f/79m59s71f , CMC Magnetics Corporation\n",
        );
        assert_eq!(
            medium_info(&report).manufacturer_id.as_deref(),
            Some("CMC Magnetics Corporation")
        );
    }

    #[tokio::test]
    async fn an_engine_reports_the_name_it_is() {
        let engine = XorrisoEngine::new();
        assert_eq!(engine.name(), "xorriso");
        // Unknown until probed: constructing an engine must not require a
        // working tool, or a worker with a broken image could not start and
        // say so.
        assert_eq!(engine.version(), "unknown");
    }

    #[tokio::test]
    async fn a_missing_binary_is_an_io_error_rather_than_a_panic() {
        let engine = XorrisoEngine::at("/nonexistent/xorriso");
        let error = engine
            .run(&["-version".to_owned()], Duration::from_secs(5))
            .await
            .expect_err("must fail");
        assert!(matches!(error, EngineError::Io { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn blanking_without_confirmation_is_refused() {
        let engine = XorrisoEngine::new();
        let request = BlankRequest {
            drive: DriveRef {
                worker_id: tangible_domain::WorkerId::generate(),
                drive_id: tangible_domain::DriveId::generate(),
                device_alias: "/dev/disc-block".to_owned(),
            },
            full: false,
            confirmed_destructive: false,
        };
        assert!(
            engine
                .blank(&request, &CollectingSink::new())
                .await
                .is_err()
        );
    }
}
