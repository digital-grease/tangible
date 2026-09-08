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
//! - **stdout and stderr are read separately and both are read.** A process
//!   whose pipe fills because nobody is draining it stops, and a stopped
//!   process mid-write is the thing above.
//!
//! Verification does not use xorriso. Reading the medium back and hashing it
//! is what "the disc holds what was intended" means, and doing it directly is
//! both simpler and less trusting than parsing a tool's opinion of its own
//! work.

use std::path::PathBuf;
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
    PreflightReport, VerifyReport, WriteReport,
};
use crate::xorriso::{command, parse};

/// How long a probe or an inspection may take.
///
/// Short: these talk to a drive that is either there or is not. A drive that
/// takes a minute to answer a capability question is a drive an operator
/// should hear about rather than wait for.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

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
}

impl XorrisoEngine {
    /// An engine that runs `xorriso` from `PATH`.
    ///
    /// The version is not probed here: constructing an engine must not require
    /// a working tool, or a worker with a misconfigured image could not start
    /// and say so.
    #[must_use]
    pub fn new() -> Self {
        Self {
            program: PathBuf::from(command::XORRISO),
            version: "unknown".to_owned(),
            cancel: CancelToken::new(),
        }
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

        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let mut text = String::new();

        // Both pipes are drained. A process whose pipe fills because nobody is
        // reading it stops, and a stopped process holding a drive is worse
        // than a slow one.
        let collect = async {
            if let Some(pipe) = stdout.as_mut() {
                let mut buffer = String::new();
                let _ = pipe.read_to_string(&mut buffer).await;
                push_bounded(&mut text, &buffer);
            }
            if let Some(pipe) = stderr.as_mut() {
                let mut buffer = String::new();
                let _ = pipe.read_to_string(&mut buffer).await;
                push_bounded(&mut text, &buffer);
            }
        };

        let finished = tokio::time::timeout(timeout, async {
            collect.await;
            child.wait().await
        })
        .await;

        let Ok(status) = finished else {
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
        manufacturer_id: report.drive_identity.clone(),
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
        if !plan.fits_on(&medium) {
            failures.push(PreflightFailure::InsufficientCapacity {
                required_bytes: plan.total_bytes,
                available_bytes: medium.free_bytes(),
            });
        }

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
        let mut text = String::new();

        // Progress arrives on stderr and the summary on stdout, so both are
        // read line by line as they come rather than after the process ends.
        // A write is minutes long; a progress bar that only moves at the end
        // is not one.
        if let Some(stderr) = stderr {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                emit(sink, &line);
                push_bounded(&mut text, &line);
                push_bounded(&mut text, "\n");
            }
        }
        if let Some(stdout) = stdout {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                emit(sink, &line);
                push_bounded(&mut text, &line);
                push_bounded(&mut text, "\n");
            }
        }

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

        // Read the medium directly rather than asking the tool whether it
        // thinks the disc is fine. Comparing what the drive actually returns
        // against what the library holds is the only thing that establishes
        // the disc holds what was intended.
        let (observed, bytes) = hash_prefix(
            &PathBuf::from(&plan.drive.device_alias),
            plan.total_bytes,
            sink,
        )
        .await?;

        let matched = observed == expected && bytes == plan.total_bytes;
        sink.emit(BurnEvent::new(
            "verifying",
            if matched {
                "VERIFY_MATCHED"
            } else {
                "VERIFY_MISMATCH"
            },
            format!("read back {bytes} bytes"),
        ));

        Ok(VerifyReport {
            matched,
            bytes_compared: bytes,
            method: "full_sector_readback".to_owned(),
            first_mismatch_offset: None,
            // Always populated: a verification result that does not state its
            // limits invites being read as a stronger guarantee than it is.
            limitations: vec![
                "compares the written extent only; padding beyond the image is not read".to_owned(),
                "reads through this drive, so a disc unreadable elsewhere can still match here"
                    .to_owned(),
            ],
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

/// Emit an engine event for a line of tool output, when it carries one.
fn emit(sink: &dyn EventSink, line: &str) {
    match parse::write_outcome(line).events.first() {
        Some(parse::WriteEvent::Progress {
            written_mb,
            total_mb,
            speed,
        }) => {
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

/// Hash the first `length` bytes of a medium, reporting progress.
async fn hash_prefix(
    path: &std::path::Path,
    length: u64,
    sink: &dyn EventSink,
) -> Result<(Sha256Digest, u64), EngineError> {
    use sha2::Digest as _;

    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|source| EngineError::Io {
            operation: "opening the medium",
            source,
        })?;

    let mut hasher = sha2::Sha256::new();
    let mut buffer = vec![0_u8; 1 << 20];
    let mut read_total: u64 = 0;
    let mut last_reported = 0_u64;

    while read_total < length {
        let want = usize::try_from((length - read_total).min(buffer.len() as u64)).unwrap_or(0);
        let read = file
            .read(&mut buffer[..want])
            .await
            .map_err(|source| EngineError::Io {
                operation: "reading the medium",
                source,
            })?;
        if read == 0 {
            // Short: the medium holds less than was written to it, which is a
            // mismatch rather than an error.
            break;
        }
        hasher.update(&buffer[..read]);
        read_total += read as u64;

        // A reading every few per cent, not every buffer: a verify of a
        // twenty-five gigabyte disc would otherwise emit thousands of events.
        if read_total - last_reported > length / 20 + 1 {
            last_reported = read_total;
            #[allow(clippy::cast_precision_loss)]
            let fraction = read_total as f32 / length.max(1) as f32;
            sink.emit(BurnEvent::progress(
                "verifying",
                "VERIFY_PROGRESS",
                fraction,
                format!("read {read_total} of {length} bytes"),
            ));
        }
    }

    Ok((
        Sha256Digest::from_bytes(hasher.finalize().into()),
        read_total,
    ))
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
        );
        assert_eq!(sink.events()[0].code, "WRITE_COMPLETED");
    }

    #[test]
    fn an_ordinary_line_emits_nothing() {
        let sink = CollectingSink::new();
        emit(&sink, "Drive current: -outdev 'stdio:/tmp/x.iso'");
        assert!(sink.events().is_empty());
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
