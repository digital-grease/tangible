// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Reading a CD with redumper, the drive's read offset corrected (ADR 0015).
//!
//! Only an image built from `deploy/decryption/Dockerfile` has redumper; the
//! worker runs it only once [`crate::decryption::detect_redumper`] has found
//! it at its fixed path, and only for a disc the drive calls a CD. It is run
//! with fixed arguments, in the rip's own directory, with a cleared
//! environment and a time limit, and stopped when the rip is withdrawn.
//! redumper starts no processes of its own, so stopping it stops everything
//! a rip started.
//!
//! Four of its commands, in order, and never any other:
//!
//! 1. `drive::detect` works out how a drive redumper does not know lays out
//!    C2 and subchannel data in a sector. Without it, a generic drive's
//!    errors are read from the wrong bytes (found on the bench, 2026-10-22).
//! 2. `dump` reads the disc, correcting the drive's read offset from
//!    redumper's table of drives.
//! 3. `refine`, only when the dump reported sectors that did not read
//!    cleanly, reads those again, up to [`RETRIES`] times. `dump` itself
//!    retries nothing, whatever it is told.
//! 4. `split` finds the disc's write offset and writes the CUE sheet and its
//!    track files, which are what Redump and AccurateRip hash. It refuses a
//!    dump with errors left in it.
//!
//! Never `disc`, which runs `dvdkey` on a DVD, nor `dvdkey`, `dvdisokey`,
//! `flash::*`, `eject` or `tools::*`: the argument lists here are the control
//! (ADR 0015).
//!
//! What it prints is read for a few known lines and otherwise only kept, a
//! few lines of it, to say why it failed. Of what it writes, only the CUE
//! sheet and the files it names are sent; the raw dump (`.scram`, `.subcode`,
//! `.state`, `.toc`, `.fulltoc`, `.log`) is left to be removed with the rip's
//! directory.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tangible_domain::manifest::{CdReadRecord, ToolRef};
use tangible_domain::rip::{CUE_OUTPUT, MAX_CUE_BYTES, OriginalKind, check_output_set, cue_track};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Command;

use crate::decryption::REDUMPER;
use crate::engine::{BurnEvent, EngineError, EventSink};
use crate::fake::CancelToken;

/// The name redumper is told to give everything it writes.
const IMAGE_NAME: &str = "disc";

/// How many times `refine` reads a sector again. Fixed by the owner's
/// decision, 2026-10-22, not an operator setting.
pub const RETRIES: u32 = 20;

/// The sector layouts `drive::detect` may answer, as `dump` takes them.
const SECTOR_ORDERS: [&str; 4] = ["DATA_C2_SUB", "DATA_SUB_C2", "DATA_SUB", "DATA_C2"];

/// How long detecting the drive's layout may take: one sector read four ways.
const DETECT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How long a dump may take. The bench's small disc took 40 seconds over
/// USB; a full one read slowly, with errors, takes an hour or more.
const DUMP_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);

/// How long retrying may take: every bad sector up to [`RETRIES`] times, on a
/// disc that may be scratched all over.
const REFINE_TIMEOUT: Duration = Duration::from_secs(8 * 60 * 60);

/// How long splitting may take. It reads only files; the bench took 2
/// seconds.
const SPLIT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How often a running command checks whether its rip was withdrawn.
const CANCEL_POLL: Duration = Duration::from_millis(250);

/// Lines of output kept, for saying why a command failed.
const KEPT_LINES: usize = 8;

/// Longest line kept; the rest is cut.
const MAX_KEPT_LINE_CHARS: usize = 200;

/// Longest line read whole. A tool that never ends a line must not grow a
/// buffer without limit.
const MAX_LINE_BYTES: usize = 4096;

/// Warnings from `split` recorded on the rip, at most. A damaged disc can
/// print one for every track.
const MAX_WARNINGS: usize = 20;

/// What a CD read with redumper produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdDump {
    /// The files to send, by name, in the output directory: `disc.cue` and
    /// its tracks.
    pub outputs: Vec<String>,
    /// The offsets applied and found.
    pub cd_read: CdReadRecord,
}

/// What reads a CD with its read offset corrected. redumper in a worker, a
/// stand-in in tests.
#[async_trait]
pub trait CdDumper: Send + Sync {
    /// The tool and its build, for the rip's record and the worker's
    /// capabilities.
    fn tool(&self) -> ToolRef;

    /// Read the CD in `device` into `output_dir`, which must exist and be
    /// empty, and split it into a CUE sheet and its tracks.
    ///
    /// # Errors
    ///
    /// [`EngineError::Cancelled`], [`EngineError::Timeout`], or
    /// [`EngineError::Unsupported`] saying what went wrong. What was written
    /// is left for the caller to remove.
    async fn dump(
        &self,
        device: &Path,
        output_dir: &Path,
        cancel: &CancelToken,
        sink: &dyn EventSink,
    ) -> Result<CdDump, EngineError>;
}

/// redumper, as the opt-in image installs it.
#[derive(Debug, Clone)]
pub struct Redumper {
    program: PathBuf,
    build: String,
}

impl Redumper {
    /// redumper at the one place the opt-in image installs it, of the build
    /// its record names.
    #[must_use]
    pub fn installed(build: impl Into<String>) -> Self {
        Self::at(Path::new("/").join(REDUMPER), build)
    }

    /// redumper at `program`. For tests and hardware tests only: a worker
    /// never takes the path from anywhere.
    #[must_use]
    pub fn at(program: impl Into<PathBuf>, build: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            build: build.into(),
        }
    }
}

#[async_trait]
impl CdDumper for Redumper {
    fn tool(&self) -> ToolRef {
        ToolRef {
            name: "redumper".to_owned(),
            version: self.build.clone(),
        }
    }

    async fn dump(
        &self,
        device: &Path,
        output_dir: &Path,
        cancel: &CancelToken,
        sink: &dyn EventSink,
    ) -> Result<CdDump, EngineError> {
        let step = Step {
            device: absolute(device, "the drive")?,
            dir: absolute(output_dir, "the rip's directory")?,
            work_dir: output_dir,
            cancel,
            sink,
        };
        let order = self.detect(&step).await?;
        let dumped = self.read(&step, order.as_deref()).await?;
        let split = self.split(&step).await?;
        let outputs = outputs_in(output_dir).await?;
        Ok(CdDump {
            outputs,
            cd_read: CdReadRecord {
                read_offset: if dumped.offset_unknown {
                    None
                } else {
                    dumped.read_offset
                },
                write_offset: split.write_offset,
                generic_drive: dumped.generic_drive,
                retried_samples: dumped.errored_samples(),
            },
        })
    }
}

/// What every command of one rip shares.
struct Step<'a> {
    /// The drive, as an argument.
    device: String,
    /// The rip's directory, as an argument.
    dir: String,
    /// The same directory, to run in.
    work_dir: &'a Path,
    cancel: &'a CancelToken,
    sink: &'a dyn EventSink,
}

impl Redumper {
    /// `drive::detect`: the sector layout to tell `dump`, when there is one to
    /// tell. Refuses a disc that is not a CD, and a drive redumper neither
    /// knows nor can work out.
    async fn detect(&self, step: &Step<'_>) -> Result<Option<String>, EngineError> {
        step.sink.emit(BurnEvent::new(
            "ripping",
            "RIP_DETECTING",
            "asking the drive how it returns C2 and subchannel data",
        ));
        let mut seen = Observed::default();
        let run = self
            .run(
                &detect_arguments(&step.device, &step.dir),
                step.work_dir,
                DETECT_TIMEOUT,
                step.cancel,
                &mut |line| seen.observe(line),
            )
            .await?;
        if run.exit_code != Some(0) {
            return Err(failure("could not work out how the drive reads a CD", &run));
        }
        if let Some(profile) = &seen.profile
            && !profile.to_ascii_uppercase().starts_with("CD")
        {
            return Err(EngineError::Unsupported {
                what: format!("redumper sees a {profile} in the drive, not a CD; nothing was read"),
            });
        }
        if seen.order_unknown && seen.drive_unknown {
            return Err(EngineError::Unsupported {
                what: "redumper does not know this drive and could not work out how it returns \
                       C2 and subchannel data, so its read would not be trustworthy; rip this \
                       disc with cdrdao instead"
                    .to_owned(),
            });
        }
        Ok(seen.sector_order)
    }

    /// `dump`, then `refine` when the dump reported samples that did not read
    /// cleanly. Answers what the dump said.
    async fn read(&self, step: &Step<'_>, order: Option<&str>) -> Result<Observed, EngineError> {
        let mut seen = Observed::default();
        let mut progress = DumpProgress::new(step.sink);
        let run = self
            .run(
                &dump_arguments(&step.device, order, &step.dir),
                step.work_dir,
                DUMP_TIMEOUT,
                step.cancel,
                &mut |line| {
                    progress.observe(line);
                    seen.observe(line);
                },
            )
            .await?;
        if run.exit_code != Some(0) {
            return Err(failure("could not read the disc", &run));
        }
        let errored = seen.errored_samples();
        if errored == 0 {
            return Ok(seen);
        }
        step.sink.emit(BurnEvent::new(
            "ripping",
            "RIP_RETRYING",
            format!(
                "{errored} samples did not read cleanly; reading them again, up to {RETRIES} times"
            ),
        ));
        let run = self
            .run(
                &refine_arguments(&step.device, order, &step.dir),
                step.work_dir,
                REFINE_TIMEOUT,
                step.cancel,
                &mut |_| {},
            )
            .await?;
        if run.exit_code != Some(0) {
            return Err(failure("could not read the disc again", &run));
        }
        Ok(seen)
    }

    /// `split`, its warnings recorded as events. Answers what it said.
    async fn split(&self, step: &Step<'_>) -> Result<Observed, EngineError> {
        step.sink.emit(BurnEvent::new(
            "ripping",
            "RIP_SPLITTING",
            "splitting what was read into a CUE sheet and its tracks",
        ));
        let mut seen = Observed::default();
        let run = self
            .run(
                &split_arguments(&step.dir),
                step.work_dir,
                SPLIT_TIMEOUT,
                step.cancel,
                &mut |line| seen.observe(line),
            )
            .await?;
        if run.exit_code != Some(0) {
            let why = if seen.track_errors.is_empty() {
                "could not split what it read into tracks"
            } else {
                "found sectors that would not read, even after retrying"
            };
            return Err(failure(why, &run));
        }
        // Recorded, never fatal: split warns about things that are not
        // wrong ("descramble failed" on a correct data track, on the bench).
        for warning in &seen.warnings {
            step.sink
                .emit(BurnEvent::new("ripping", "RIP_WARNING", warning.clone()));
        }
        Ok(seen)
    }
}

/// `drive::detect`: the drive's sector layout, when redumper has to work it
/// out.
fn detect_arguments(device: &str, dir: &str) -> Vec<String> {
    vec![
        "drive::detect".to_owned(),
        format!("--drive={device}"),
        format!("--image-path={dir}"),
        format!("--image-name={IMAGE_NAME}"),
    ]
}

/// `dump`, with the layout detected when there was one to detect.
fn dump_arguments(device: &str, order: Option<&str>, dir: &str) -> Vec<String> {
    let mut arguments = vec!["dump".to_owned(), format!("--drive={device}")];
    if let Some(order) = order {
        arguments.push(format!("--drive-sector-order={order}"));
    }
    arguments.push(format!("--image-path={dir}"));
    arguments.push(format!("--image-name={IMAGE_NAME}"));
    arguments
}

/// `refine`: the sectors the dump could not read, again.
fn refine_arguments(device: &str, order: Option<&str>, dir: &str) -> Vec<String> {
    let mut arguments = vec!["refine".to_owned(), format!("--drive={device}")];
    if let Some(order) = order {
        arguments.push(format!("--drive-sector-order={order}"));
    }
    arguments.push(format!("--retries={RETRIES}"));
    arguments.push(format!("--image-path={dir}"));
    arguments.push(format!("--image-name={IMAGE_NAME}"));
    arguments
}

/// `split`: no drive, only the files the dump wrote.
fn split_arguments(dir: &str) -> Vec<String> {
    vec![
        "split".to_owned(),
        format!("--image-path={dir}"),
        format!("--image-name={IMAGE_NAME}"),
    ]
}

/// A path as an argument's value, refused unless it is absolute and plain.
fn absolute(path: &Path, what: &str) -> Result<String, EngineError> {
    match path.to_str() {
        Some(text) if path.is_absolute() && !text.chars().any(|c| c.is_control() || c == '=') => {
            Ok(text.to_owned())
        }
        _ => Err(EngineError::Unsupported {
            what: format!("{what} is not at an absolute, plain path"),
        }),
    }
}

/// What a command printed that matters, gathered line by line.
// Independent facts, each from its own line; they are read together, not
// one state.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default)]
struct Observed {
    /// The layout `drive::detect` found.
    sector_order: Option<String>,
    /// `drive::detect` tried and could not tell.
    order_unknown: bool,
    /// The drive is not in redumper's database.
    drive_unknown: bool,
    /// The drive's read offset is not in redumper's table, so none was
    /// applied.
    offset_unknown: bool,
    /// The read offset redumper applied, from its account of the drive.
    read_offset: Option<i32>,
    /// redumper does not trust this drive to dump faithfully.
    generic_drive: bool,
    /// The disc, as the drive names it.
    profile: Option<String>,
    /// Samples the dump could not read: SCSI errors.
    scsi_samples: u64,
    /// Samples the dump read with C2 errors.
    c2_samples: u64,
    /// The disc's write offset, from `split`.
    write_offset: Option<i32>,
    /// `split`'s warnings, bounded.
    warnings: Vec<String>,
    /// `split`'s account of tracks with errors left in them.
    track_errors: Vec<String>,
    /// Which block of a summary the next lines belong to.
    in_media_errors: bool,
}

impl Observed {
    /// Samples the dump could not read cleanly, either way.
    fn errored_samples(&self) -> u64 {
        self.scsi_samples.saturating_add(self.c2_samples)
    }

    fn observe(&mut self, line: &str) {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("GENERIC: auto-detected sector order: ") {
            let order = rest.split_whitespace().next().unwrap_or_default();
            // Only a layout dump can be told; anything else is not believed.
            self.sector_order = SECTOR_ORDERS
                .iter()
                .find(|known| **known == order)
                .map(|known| (*known).to_owned());
            self.order_unknown = self.sector_order.is_none();
        } else if line.starts_with("warning: sector order detection failed") {
            self.order_unknown = true;
        } else if line == "warning: drive not found in the database" {
            self.drive_unknown = true;
        } else if line == "warning: drive read offset not found in the database" {
            self.offset_unknown = true;
        } else if line == "warning: using generic drive" {
            self.generic_drive = true;
        } else if let Some(rest) = line.strip_prefix("configuration: ") {
            self.read_offset = number_after(rest, "read offset: ");
        } else if let Some(rest) = line.strip_prefix("profile: ") {
            self.profile = Some(plain(rest));
        } else if line == "media errors:" {
            self.in_media_errors = true;
        } else if self.in_media_errors
            && let Some(rest) = line.strip_prefix("SCSI: ")
        {
            self.scsi_samples = leading_count(rest);
        } else if self.in_media_errors
            && let Some(rest) = line.strip_prefix("C2: ")
        {
            self.c2_samples = leading_count(rest);
            self.in_media_errors = false;
        } else if let Some(rest) = line.strip_prefix("disc write offset: ") {
            self.write_offset = rest.trim().parse().ok();
        } else if line.starts_with("errors detected, track:") {
            if self.track_errors.len() < MAX_WARNINGS {
                self.track_errors.push(plain(line));
            }
        } else if line.starts_with("warning: ") && self.warnings.len() < MAX_WARNINGS {
            self.warnings.push(plain(line));
        }
    }
}

/// The signed number after `label` in `text`, up to the next comma or
/// bracket: `+6` in `GENERIC (read offset: +6, C2 shift: 0, ...)`.
fn number_after(text: &str, label: &str) -> Option<i32> {
    let start = text.find(label)? + label.len();
    let rest = &text[start..];
    let end = rest.find([',', ')']).unwrap_or(rest.len());
    rest[..end].trim().parse().ok()
}

/// The count at the start of `12 samples`.
fn leading_count(text: &str) -> u64 {
    text.split_whitespace()
        .next()
        .and_then(|count| count.parse().ok())
        .unwrap_or(0)
}

/// A line as it may be kept: cut short and stripped of control characters.
fn plain(line: &str) -> String {
    line.trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_KEPT_LINE_CHARS)
        .collect()
}

/// A dump's progress lines, as events, once for each whole percent.
///
/// redumper redraws `[ 42%] LBA: 123456/330000, errors: ...` in place. The
/// percent is passed on as it is, but never 100 before the rip has finished:
/// splitting is still to come.
struct DumpProgress<'a> {
    sink: &'a dyn EventSink,
    last_percent: Option<u32>,
}

impl<'a> DumpProgress<'a> {
    fn new(sink: &'a dyn EventSink) -> Self {
        Self {
            sink,
            last_percent: None,
        }
    }

    fn observe(&mut self, line: &str) {
        let Some(percent) = progress_percent(line) else {
            return;
        };
        if self.last_percent == Some(percent) {
            return;
        }
        self.last_percent = Some(percent);
        #[allow(clippy::cast_precision_loss)]
        let fraction = (percent as f32 / 100.0).min(0.99);
        self.sink.emit(BurnEvent::progress(
            "ripping",
            "RIP_PROGRESS",
            fraction,
            format!("{percent}% read"),
        ));
    }
}

/// The percent in a progress line, `[ 42%] LBA: ...`, if it is one.
fn progress_percent(line: &str) -> Option<u32> {
    let open = line.find('[')?;
    let rest = &line[open + 1..];
    let close = rest.find("%]")?;
    if !rest[close..].contains("LBA:") {
        return None;
    }
    rest[..close]
        .trim()
        .parse()
        .ok()
        .filter(|percent| *percent <= 100)
}

/// The files `split` wrote that are sent: the CUE sheet and the files it
/// names, which must be the track files beside it, all of them and no more.
async fn outputs_in(dir: &Path) -> Result<Vec<String>, EngineError> {
    let unexpected = |what: String| EngineError::Unsupported {
        what: format!("redumper's split {what}"),
    };
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .map_err(|source| EngineError::Io {
            operation: "listing what redumper wrote",
            source,
        })?;
    let mut tracks = Vec::new();
    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(source) => {
                return Err(EngineError::Io {
                    operation: "listing what redumper wrote",
                    source,
                });
            }
        };
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let is_file = entry.file_type().await.is_ok_and(|kind| kind.is_file());
        if is_file && (name == "disc.bin" || cue_track(&name).is_some()) {
            tracks.push(name);
        }
    }
    tracks.sort_unstable();

    let sheet_path = dir.join(CUE_OUTPUT);
    let sheet = read_bounded(&sheet_path)
        .await
        .map_err(|error| unexpected(format!("left no CUE sheet that could be read: {error}")))?;
    let mut named = referenced_files(&sheet);
    named.sort_unstable();
    named.dedup();
    if named != tracks {
        return Err(unexpected(format!(
            "wrote a CUE sheet naming {} beside the track files {}",
            list(&named),
            list(&tracks)
        )));
    }

    let mut outputs = vec![CUE_OUTPUT.to_owned()];
    outputs.extend(tracks);
    match check_output_set(&outputs, false, None) {
        Ok(sets) if sets.original == OriginalKind::CueBin => Ok(outputs),
        Ok(_) => Err(unexpected(
            "wrote something other than a CUE sheet".to_owned(),
        )),
        Err(error) => Err(unexpected(format!(
            "wrote files the server would refuse: {error}"
        ))),
    }
}

/// A CUE sheet's text, refused past [`MAX_CUE_BYTES`].
async fn read_bounded(path: &Path) -> Result<String, String> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.take(MAX_CUE_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_CUE_BYTES {
        return Err(format!("it is larger than {MAX_CUE_BYTES} bytes"));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The names in a CUE sheet's `FILE "..."` lines.
fn referenced_files(sheet: &str) -> Vec<String> {
    sheet
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("FILE "))
        .filter_map(|rest| {
            let rest = rest.trim_start().strip_prefix('"')?;
            rest.find('"').map(|end| rest[..end].to_owned())
        })
        .collect()
}

/// Names for a message, bounded.
fn list(names: &[String]) -> String {
    if names.is_empty() {
        return "nothing".to_owned();
    }
    let mut shown: Vec<String> = names.iter().take(4).map(|name| plain(name)).collect();
    if names.len() > shown.len() {
        shown.push(format!("and {} more", names.len() - 4));
    }
    shown.join(", ")
}

/// What a run of the tool left behind.
#[derive(Debug)]
struct Run {
    exit_code: Option<i32>,
    /// The last few lines worth showing: its errors and warnings.
    tail: Vec<String>,
}

impl Redumper {
    /// Run one command to its end, handing every line it prints to
    /// `on_line`.
    async fn run(
        &self,
        arguments: &[String],
        work_dir: &Path,
        timeout: Duration,
        cancel: &CancelToken,
        on_line: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Run, EngineError> {
        if cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        let mut child = Command::new(&self.program)
            .args(arguments)
            .current_dir(work_dir)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| EngineError::Io {
                operation: "starting redumper",
                source,
            })?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let finished = async {
            // Together, never one after the other: a pipe nobody reads fills,
            // and the tool writing to it stops.
            let mut tail = VecDeque::with_capacity(KEPT_LINES + 1);
            let mut keep = |line: &str| {
                let trimmed = line.trim();
                if trimmed.starts_with("error") || trimmed.starts_with("warning") {
                    tail.push_back(plain(trimmed));
                    if tail.len() > KEPT_LINES {
                        tail.pop_front();
                    }
                }
            };
            let mut errors = Vec::new();
            let mut out_line = |line: &str| {
                on_line(line);
                keep(line);
            };
            let mut err_line = |line: &str| {
                let line = plain(line);
                if !line.is_empty() && errors.len() < KEPT_LINES {
                    errors.push(line);
                }
            };
            let ((), ()) = tokio::join!(
                read_lines(stdout, &mut out_line),
                read_lines(stderr, &mut err_line)
            );
            let mut tail: Vec<String> = tail.into();
            tail.extend(errors);
            let start = tail.len().saturating_sub(KEPT_LINES);
            (tail.split_off(start), child.wait().await)
        };
        let watched = async {
            loop {
                tokio::time::sleep(CANCEL_POLL).await;
                if cancel.is_cancelled() {
                    return;
                }
            }
        };
        let outcome = tokio::time::timeout(timeout, async {
            tokio::select! {
                done = finished => Some(done),
                () = watched => None,
            }
        })
        .await;
        // Leaving here drops the child, which kills it if it is still running.
        match outcome {
            Err(_) => Err(EngineError::Timeout {
                operation: "redumper",
                seconds: timeout.as_secs(),
            }),
            Ok(None) => Err(EngineError::Cancelled),
            Ok(Some((tail, status))) => {
                let status = status.map_err(|source| EngineError::Io {
                    operation: "waiting for redumper",
                    source,
                })?;
                tracing::debug!(
                    command = %arguments.first().map_or("", String::as_str),
                    exit_code = ?status.code(),
                    "redumper finished"
                );
                Ok(Run {
                    exit_code: status.code(),
                    tail,
                })
            }
        }
    }
}

/// Read a pipe to its end, handing each line to `on_line`. Lines end at a
/// newline or a carriage return, which is how redumper redraws its progress.
async fn read_lines<R: AsyncRead + Unpin>(pipe: Option<R>, on_line: &mut (dyn FnMut(&str) + Send)) {
    let Some(mut pipe) = pipe else {
        return;
    };
    let mut pending = Vec::new();
    let mut buffer = vec![0_u8; 8192];
    let mut emit = |pending: &mut Vec<u8>| {
        if !pending.is_empty() {
            on_line(&String::from_utf8_lossy(pending));
            pending.clear();
        }
    };
    loop {
        match pipe.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                for &byte in &buffer[..read] {
                    if byte == b'\n' || byte == b'\r' {
                        emit(&mut pending);
                    } else if pending.len() < MAX_LINE_BYTES {
                        pending.push(byte);
                    }
                }
            }
        }
    }
    emit(&mut pending);
}

/// Why a command failed, in its own words where it gave any.
fn failure(what: &str, run: &Run) -> EngineError {
    let code = run
        .exit_code
        .map_or_else(|| "was stopped".to_owned(), |code| format!("exited {code}"));
    EngineError::Unsupported {
        what: if run.tail.is_empty() {
            format!("redumper {what}; it {code}")
        } else {
            format!("redumper {what}; it {code}: {}", run.tail.join(" / "))
        },
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::engine::CollectingSink;

    #[test]
    fn the_four_commands_are_exactly_these_arguments() {
        // Asserted in full: an argument that appears here without a test
        // saying why is an argument nobody decided to pass.
        assert_eq!(
            detect_arguments("/dev/disc-block", "/state/rips/r"),
            [
                "drive::detect",
                "--drive=/dev/disc-block",
                "--image-path=/state/rips/r",
                "--image-name=disc"
            ]
        );
        assert_eq!(
            dump_arguments("/dev/disc-block", Some("DATA_SUB_C2"), "/state/rips/r"),
            [
                "dump",
                "--drive=/dev/disc-block",
                "--drive-sector-order=DATA_SUB_C2",
                "--image-path=/state/rips/r",
                "--image-name=disc"
            ]
        );
        assert_eq!(
            dump_arguments("/dev/disc-block", None, "/r"),
            [
                "dump",
                "--drive=/dev/disc-block",
                "--image-path=/r",
                "--image-name=disc"
            ]
        );
        assert_eq!(
            refine_arguments("/dev/disc-block", Some("DATA_C2"), "/r"),
            [
                "refine",
                "--drive=/dev/disc-block",
                "--drive-sector-order=DATA_C2",
                "--retries=20",
                "--image-path=/r",
                "--image-name=disc"
            ]
        );
        assert_eq!(
            split_arguments("/r"),
            ["split", "--image-path=/r", "--image-name=disc"]
        );
        // None of the commands Tangible never runs can be produced.
        for arguments in [
            detect_arguments("/d", "/r"),
            dump_arguments("/d", Some("DATA_C2"), "/r"),
            refine_arguments("/d", None, "/r"),
            split_arguments("/r"),
        ] {
            for forbidden in ["disc", "dvdkey", "dvdisokey", "eject", "bdkey"] {
                assert!(!arguments.iter().any(|a| a == forbidden), "{arguments:?}");
            }
            assert!(
                !arguments
                    .iter()
                    .any(|a| a.starts_with("flash::") || a.starts_with("tools::"))
            );
        }
    }

    #[test]
    fn paths_must_be_absolute_and_plain() {
        assert_eq!(
            absolute(Path::new("/dev/sr0"), "the drive").unwrap(),
            "/dev/sr0"
        );
        for path in ["sr0", "", "/dev/x\n--retries=0", "/dev/a=b", "-o"] {
            assert!(absolute(Path::new(path), "the drive").is_err(), "{path:?}");
        }
    }

    /// redumper's own words, from the bench run of 2026-10-22 and its
    /// source at b768.
    const BENCH_DETECT: &str = "redumper (build: b768)

arguments: drive::detect --drive=/dev/sr0 --image-path=/work --image-name=disc

warning: drive read offset not found in the database
warning: drive not found in the database
warning: using generic drive

drive information
  path: /dev/sr0
  inquiry: Slimtype - DVD A  DS8A8SH (revision level: KAA2, vendor specific: <empty>)
  configuration: GENERIC (read offset: +0, C2 shift: 0, pre-gap start: +0, read method: BE, sector order: DATA_C2_SUB)
  profile: CD-R

*** DRIVE::DETECT (time check: 0s)

GENERIC: auto-detected sector order: DATA_SUB_C2
";

    #[test]
    fn detection_finds_the_layout_and_what_redumper_knows_of_the_drive() {
        let mut seen = Observed::default();
        for line in BENCH_DETECT.lines() {
            seen.observe(line);
        }
        assert_eq!(seen.sector_order.as_deref(), Some("DATA_SUB_C2"));
        assert!(!seen.order_unknown);
        assert!(seen.drive_unknown && seen.offset_unknown && seen.generic_drive);
        assert_eq!(seen.profile.as_deref(), Some("CD-R"));
        assert_eq!(seen.read_offset, Some(0));

        let mut seen = Observed::default();
        seen.observe("GENERIC: auto-detected sector order: DATA_C2 (no subcode validation)");
        assert_eq!(seen.sector_order.as_deref(), Some("DATA_C2"));
        let mut seen = Observed::default();
        seen.observe("GENERIC: auto-detected sector order: --retries=0");
        assert_eq!(seen.sector_order, None, "only a layout redumper names");
        assert!(seen.order_unknown);
        let mut seen = Observed::default();
        seen.observe("warning: sector order detection failed, using default");
        assert!(seen.order_unknown);
    }

    #[test]
    fn the_dump_says_its_offset_and_its_errors() {
        let mut seen = Observed::default();
        for line in [
            "warning: using generic drive",
            "  configuration: GENERIC (read offset: +6, C2 shift: 0, pre-gap start: -135, read method: BE, sector order: DATA_SUB_C2)",
            "| [ 99%] LBA:   3607/3608, errors: { SCSIs: 0, C2s: 0, Q: 3 }",
            "media errors: ",
            "  SCSI: 12 samples",
            "  C2: 588 samples",
            "  Q: 3",
        ] {
            seen.observe(line);
        }
        assert_eq!(seen.read_offset, Some(6));
        assert!(seen.generic_drive && !seen.offset_unknown);
        assert_eq!((seen.scsi_samples, seen.c2_samples), (12, 588));
        assert_eq!(
            number_after("X (read offset: -1164, C2", "read offset: "),
            Some(-1164)
        );
        assert_eq!(
            number_after("X (read offset: +6)", "read offset: "),
            Some(6)
        );
    }

    #[test]
    fn split_says_the_write_offset_and_its_warnings() {
        let mut seen = Observed::default();
        for line in [
            "warning: descramble failed (LBA: [42 .. 1207])",
            "disc write offset: -6",
            "errors detected, track: 2, sectors: {SKIP: 1, C2: 0}, samples: {SKIP: 588, C2: 0}",
        ] {
            seen.observe(line);
        }
        assert_eq!(seen.write_offset, Some(-6));
        assert_eq!(
            seen.warnings,
            ["warning: descramble failed (LBA: [42 .. 1207])"]
        );
        assert_eq!(seen.track_errors.len(), 1);
        for _ in 0..100 {
            seen.observe("warning: TOC / QTOC mismatch");
        }
        assert_eq!(seen.warnings.len(), MAX_WARNINGS, "bounded");
    }

    #[test]
    fn progress_lines_are_read_for_their_percent_alone() {
        assert_eq!(
            progress_percent("/ [ 42%] LBA: 123456/330000, errors: { SCSIs: 0, C2s: 0, Q: 0 }"),
            Some(42)
        );
        assert_eq!(
            progress_percent("[100%] LBA:   3608/3608, errors"),
            Some(100)
        );
        for line in [
            "[LBA:    123] C2 error (bits:   12)",
            "GENERIC: storing lead-in (LBA:   -150, sectors: 150)",
            "[ 101%] LBA: 1/1",
            "[ 42%] something else",
        ] {
            assert_eq!(progress_percent(line), None, "{line}");
        }
    }

    #[test]
    fn a_cue_sheets_files_are_read_from_its_file_lines() {
        let sheet = "REM a\nFILE \"disc (Track 1).bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n\
                     FILE \"disc (Track 2).bin\" BINARY\n  TRACK 02 AUDIO\n";
        assert_eq!(
            referenced_files(sheet),
            ["disc (Track 1).bin", "disc (Track 2).bin"]
        );
    }

    /// A stand-in for redumper that writes `files` into the image path when
    /// split, prints `output` for each command, and records its arguments
    /// and environment beside itself.
    ///
    /// Made executable by `install`, in a process of its own, as in the
    /// dvdbackup tests: a script held open for writing could be inherited
    /// by a tool another test is starting ("text file busy").
    #[cfg(unix)]
    fn stub(dir: &Path, body: &str) -> Redumper {
        let source = dir.join("redumper.sh");
        let script = dir.join("redumper");
        std::fs::write(
            &source,
            format!(
                "#!/bin/sh\n\
                 here=$(dirname \"$0\")\n\
                 echo \"$@\" >> \"$here/arguments\"\n\
                 env > \"$here/environment\"\n\
                 for a in \"$@\"; do case \"$a\" in --image-path=*) out=${{a#--image-path=}};; esac; done\n\
                 {body}\n"
            ),
        )
        .unwrap();
        let installed = std::process::Command::new("install")
            .args(["-m", "0755"])
            .arg(&source)
            .arg(&script)
            .status()
            .unwrap();
        assert!(installed.success());
        Redumper::at(script, "b768")
    }

    /// redumper as it behaved on the bench: detects DATA_SUB_C2, dumps
    /// cleanly with offset +6, splits into two tracks with a false warning.
    const BENCH: &str = r#"case "$1" in
  drive::detect) printf 'warning: drive not found in the database\nwarning: using generic drive\n  profile: CD-R\nGENERIC: auto-detected sector order: DATA_SUB_C2\n';;
  dump) printf 'warning: using generic drive\n  configuration: GENERIC (read offset: +6, C2 shift: 0)\n'
        printf '| [ 10%%] LBA: 360/3608, errors\r/ [ 55%%] LBA: 1984/3608, errors\r'
        printf '\nmedia errors: \n  SCSI: 0 samples\n  C2: 0 samples\n  Q: 3\n'
        head -c 1000 /dev/zero > "$out/disc.scram"; echo t > "$out/disc.toc"; echo l > "$out/disc.log";;
  refine) printf 'correction statistics: \n  SCSI: 1176 samples\n';;
  split) printf 'FILE "disc (Track 1).bin" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\nFILE "disc (Track 2).bin" BINARY\n  TRACK 02 AUDIO\n    ISRC 000000000000\n    INDEX 00 00:00:00\n    INDEX 01 00:02:00\n' > "$out/disc.cue"
         head -c 2352 /dev/zero > "$out/disc (Track 1).bin"; head -c 4704 /dev/zero > "$out/disc (Track 2).bin"
         printf 'warning: descramble failed (LBA: [42 .. 1207])\ndisc write offset: -6\n';;
  *) echo "error: not expected: $1"; exit 3;;
esac"#;

    #[cfg(unix)]
    #[tokio::test]
    async fn a_cd_is_detected_dumped_and_split_into_its_tracks() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("rip");
        std::fs::create_dir(&out).unwrap();
        let tool = stub(dir.path(), BENCH);
        let sink = CollectingSink::new();
        let dump = tool
            .dump(
                Path::new("/dev/disc-block"),
                &out,
                &CancelToken::new(),
                &sink,
            )
            .await
            .unwrap();
        assert_eq!(
            dump.outputs,
            ["disc.cue", "disc (Track 1).bin", "disc (Track 2).bin"]
        );
        assert_eq!(
            dump.cd_read,
            CdReadRecord {
                read_offset: Some(6),
                write_offset: Some(-6),
                generic_drive: true,
                retried_samples: 0,
            }
        );
        let arguments = std::fs::read_to_string(dir.path().join("arguments")).unwrap();
        let commands: Vec<&str> = arguments
            .lines()
            .map(|line| line.split(' ').next().unwrap())
            .collect();
        assert_eq!(
            commands,
            ["drive::detect", "dump", "split"],
            "no refine on a clean dump"
        );
        assert!(
            arguments.contains("--drive-sector-order=DATA_SUB_C2"),
            "{arguments}"
        );
        let environment = std::fs::read_to_string(dir.path().join("environment")).unwrap();
        assert!(
            !environment.contains("HOME="),
            "nothing inherited: {environment}"
        );

        let events = sink.events();
        let progress: Vec<f32> = events
            .iter()
            .filter(|event| event.code == "RIP_PROGRESS")
            .filter_map(|event| event.progress)
            .collect();
        assert_eq!(progress, [0.10, 0.55]);
        let warnings: Vec<&str> = events
            .iter()
            .filter(|event| event.code == "RIP_WARNING")
            .map(|event| event.message.as_str())
            .collect();
        assert_eq!(warnings, ["warning: descramble failed (LBA: [42 .. 1207])"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_dump_with_errors_is_read_again_up_to_twenty_times() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("rip");
        std::fs::create_dir(&out).unwrap();
        let tool = stub(
            dir.path(),
            &BENCH.replace("SCSI: 0 samples", "SCSI: 1176 samples"),
        );
        let sink = CollectingSink::new();
        let dump = tool
            .dump(
                Path::new("/dev/disc-block"),
                &out,
                &CancelToken::new(),
                &sink,
            )
            .await
            .unwrap();
        assert_eq!(dump.cd_read.retried_samples, 1176);
        let arguments = std::fs::read_to_string(dir.path().join("arguments")).unwrap();
        let refine = arguments
            .lines()
            .find(|line| line.starts_with("refine "))
            .expect("refined");
        assert!(refine.contains("--retries=20"), "{refine}");
        assert!(
            refine.contains("--drive-sector-order=DATA_SUB_C2"),
            "{refine}"
        );
        assert!(
            sink.events()
                .iter()
                .any(|event| event.code == "RIP_RETRYING"),
            "said so"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_drive_whose_offset_redumper_does_not_know_is_read_uncorrected() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("rip");
        std::fs::create_dir(&out).unwrap();
        let tool = stub(
            dir.path(),
            &BENCH.replace(
                "dump) printf 'warning: using generic drive\\n",
                "dump) printf 'warning: drive read offset not found in the database\\nwarning: using generic drive\\n",
            ),
        );
        let dump = tool
            .dump(
                Path::new("/dev/disc-block"),
                &out,
                &CancelToken::new(),
                &CollectingSink::new(),
            )
            .await
            .unwrap();
        assert_eq!(dump.cd_read.read_offset, None);
    }

    async fn failed(body: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("rip");
        std::fs::create_dir(&out).unwrap();
        let tool = stub(dir.path(), body);
        let error = tool
            .dump(
                Path::new("/dev/disc-block"),
                &out,
                &CancelToken::new(),
                &CollectingSink::new(),
            )
            .await
            .unwrap_err();
        let EngineError::Unsupported { what } = error else {
            panic!("{error:?}")
        };
        what
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unknown_drive_whose_layout_cannot_be_detected_is_not_dumped() {
        let what = failed(&BENCH.replace(
            "GENERIC: auto-detected sector order: DATA_SUB_C2",
            "warning: sector order detection failed, using default",
        ))
        .await;
        assert!(what.contains("cdrdao"), "{what}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_disc_that_is_not_a_cd_is_not_dumped() {
        let what = failed(&BENCH.replace("profile: CD-R", "profile: DVD-ROM")).await;
        assert!(
            what.contains("DVD-ROM") && what.contains("nothing was read"),
            "{what}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn errors_left_after_retrying_fail_the_rip_saying_so() {
        let what = failed(&BENCH.replace(
            "  split) printf",
            "  split) echo 'errors detected, track: 2, sectors: {SKIP: 1, C2: 0}'; echo 'error: data errors detected, unable to continue'; exit 255; printf",
        ))
        .await;
        assert!(
            what.contains("would not read, even after retrying"),
            "{what}"
        );
        assert!(what.contains("data errors detected"), "{what}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_cue_sheet_that_does_not_match_its_tracks_is_not_sent() {
        let what = failed(&BENCH.replace(
            "head -c 4704 /dev/zero > \"$out/disc (Track 2).bin\"",
            "head -c 4704 /dev/zero > \"$out/disc (Track 3).bin\"",
        ))
        .await;
        assert!(what.contains("CUE sheet naming"), "{what}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_withdrawn_dump_stops_the_tool() {
        let dir = tempfile::tempdir().unwrap();
        let tool = stub(dir.path(), "sleep 30");
        let cancel = CancelToken::new();
        let stopper = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                cancel.cancel();
            })
        };
        let started = std::time::Instant::now();
        let result = tool
            .dump(
                Path::new("/dev/disc-block"),
                dir.path(),
                &cancel,
                &CollectingSink::new(),
            )
            .await;
        stopper.await.unwrap();
        assert!(matches!(result, Err(EngineError::Cancelled)), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn the_tool_is_redumper_of_its_build_at_its_fixed_place() {
        let tool = Redumper::installed("b768");
        assert_eq!(tool.program, Path::new("/usr/local/bin/redumper"));
        assert_eq!(
            tool.tool(),
            ToolRef {
                name: "redumper".to_owned(),
                version: "b768".to_owned()
            }
        );
    }
}
