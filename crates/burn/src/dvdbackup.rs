// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Decrypting a DVD in the drive, through dvdbackup (ADR 0013).
//!
//! Only an image built from `deploy/decryption/Dockerfile` has dvdbackup and
//! libdvdcss; the worker runs the tool only once [`crate::decryption`] has
//! found both, and only for a rip that asked to decrypt. dvdbackup is run
//! from its fixed path, with a fixed argument list, in its own directory,
//! with a time limit, and stopped when the rip is withdrawn. libdvdread loads
//! libdvdcss itself; nothing here loads a library or names one.
//!
//! Two things are asked of it:
//!
//! - **Authentication**, `dvdbackup -I`. Reading the disc's information makes
//!   libdvdcss authenticate the drive, and an authenticated drive hands over
//!   the scrambled title video to plain block reads until the disc leaves
//!   it. Found on the bench, 2026-10-20. That is what lets a rip keep the
//!   encrypted original as it is on the disc.
//! - **A decrypted copy**, `dvdbackup -M`: every file of the disc's `VIDEO_TS`,
//!   decrypted as it is read.
//!
//! # Keys
//!
//! libdvdcss works keys out from the disc and the drive; none is given to
//! it, kept, or sent anywhere. Its cache is off (`DVDCSS_CACHE=off`) and it
//! is told to be quiet. What the tool prints is never sent as it is: only a
//! few lines of it are kept, for saying why it failed, and every line that
//! could carry key material is dropped first ([`shareable`]). libdvdread's
//! "Get key for ... at 0x..." lines name sector offsets, not keys, and are
//! dropped all the same.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tangible_domain::manifest::DecryptionRecord;
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Command;

use crate::decryption::{CssDecryption, DVDBACKUP};
use crate::engine::{BurnEvent, EngineError, EventSink};
use crate::fake::CancelToken;

/// The name dvdbackup is told to give the disc. It writes the copy to
/// `<output>/disc/VIDEO_TS/`.
const MIRROR_NAME: &str = "disc";

/// How long authenticating may take. libdvdread works out every title's
/// key, which it warns "can take a _long_ time"; the bench disc took
/// seconds, a damaged one may take minutes.
const AUTHENTICATE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// How long a decrypted copy may take. A dual-layer disc over USB copied at
/// about 2 MB/s on the bench, which is an hour; a disc that needs retries is
/// slower still.
const MIRROR_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);

/// How often a copy's progress is measured.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(2);

/// How often a running tool checks whether its rip was withdrawn.
const CANCEL_POLL: Duration = Duration::from_millis(250);

/// Lines of the tool's output kept, for saying why it failed.
const KEPT_LINES: usize = 8;

/// Longest line kept; the rest is cut.
const MAX_KEPT_LINE_CHARS: usize = 200;

/// Longest line read whole. A tool that never ends a line must not grow a
/// buffer without limit.
const MAX_LINE_BYTES: usize = 4096;

/// What decrypts a DVD in the drive. dvdbackup in a worker, a stand-in in
/// tests.
#[async_trait]
pub trait CssCopier: Send + Sync {
    /// What decrypts, for the rip's record: library and tool, never a key.
    fn decryption(&self) -> DecryptionRecord;

    /// Authenticate the drive for the disc in it, so that scrambled sectors
    /// read.
    ///
    /// # Errors
    ///
    /// [`EngineError::Cancelled`], [`EngineError::Timeout`], or
    /// [`EngineError::Unsupported`] saying why it could not.
    async fn authenticate(
        &self,
        device: &Path,
        work_dir: &Path,
        cancel: &CancelToken,
    ) -> Result<(), EngineError>;

    /// Copy the disc's video, decrypted, into `into/VIDEO_TS/`. `into` must
    /// exist and be empty. `expected_bytes`, the disc's size, is what
    /// progress is measured against.
    ///
    /// # Errors
    ///
    /// As [`Self::authenticate`]. What was copied is left for the caller to
    /// remove.
    async fn mirror(
        &self,
        device: &Path,
        into: &Path,
        expected_bytes: Option<u64>,
        cancel: &CancelToken,
        sink: &dyn EventSink,
    ) -> Result<(), EngineError>;
}

/// dvdbackup, as the opt-in image installs it.
#[derive(Debug, Clone)]
pub struct Dvdbackup {
    program: PathBuf,
    css: CssDecryption,
}

impl Dvdbackup {
    /// dvdbackup at the one place the opt-in image installs it, described by
    /// the capability the worker found.
    #[must_use]
    pub fn installed(css: CssDecryption) -> Self {
        Self::at(Path::new("/").join(DVDBACKUP), css)
    }

    /// dvdbackup at `program`. For tests and hardware tests only: a worker
    /// never takes the path from anywhere.
    #[must_use]
    pub fn at(program: impl Into<PathBuf>, css: CssDecryption) -> Self {
        Self {
            program: program.into(),
            css,
        }
    }
}

#[async_trait]
impl CssCopier for Dvdbackup {
    fn decryption(&self) -> DecryptionRecord {
        DecryptionRecord {
            system: "css".to_owned(),
            library: self.css.library.clone(),
            library_version: self.css.version.clone(),
            tool: self.css.tool.clone(),
            tool_version: self.css.tool_version.clone(),
        }
    }

    async fn authenticate(
        &self,
        device: &Path,
        work_dir: &Path,
        cancel: &CancelToken,
    ) -> Result<(), EngineError> {
        let arguments = info_arguments(device)?;
        let run = self
            .run(&arguments, work_dir, AUTHENTICATE_TIMEOUT, cancel, None)
            .await?;
        if run.exit_code != Some(0) {
            return Err(EngineError::Unsupported {
                what: failure("could not read the disc's DVD-Video information", &run),
            });
        }
        Ok(())
    }

    async fn mirror(
        &self,
        device: &Path,
        into: &Path,
        expected_bytes: Option<u64>,
        cancel: &CancelToken,
        sink: &dyn EventSink,
    ) -> Result<(), EngineError> {
        let arguments = mirror_arguments(device, into)?;
        let written = into.join(MIRROR_NAME).join("VIDEO_TS");
        let progress = expected_bytes
            .filter(|bytes| *bytes > 0)
            .map(|total| Progress {
                directory: written.clone(),
                total,
                sink,
            });
        let run = self
            .run(&arguments, into, MIRROR_TIMEOUT, cancel, progress)
            .await?;
        if run.exit_code != Some(0) {
            return Err(EngineError::Unsupported {
                what: failure("could not copy the disc's video", &run),
            });
        }
        let io = |operation: &'static str| move |source| EngineError::Io { operation, source };
        tokio::fs::rename(&written, into.join("VIDEO_TS"))
            .await
            .map_err(io("placing the decrypted copy"))?;
        tokio::fs::remove_dir_all(into.join(MIRROR_NAME))
            .await
            .map_err(io("tidying after the decrypted copy"))?;
        Ok(())
    }
}

/// `dvdbackup -I -i <device>`: the disc's information, which authenticates.
fn info_arguments(device: &Path) -> Result<Vec<String>, EngineError> {
    Ok(vec![
        "-I".to_owned(),
        "-i".to_owned(),
        absolute(device, "the drive")?,
    ])
}

/// `dvdbackup -M -n disc -i <device> -o <into>`: every `VIDEO_TS` file,
/// decrypted.
fn mirror_arguments(device: &Path, into: &Path) -> Result<Vec<String>, EngineError> {
    Ok(vec![
        "-M".to_owned(),
        "-n".to_owned(),
        MIRROR_NAME.to_owned(),
        "-i".to_owned(),
        absolute(device, "the drive")?,
        "-o".to_owned(),
        absolute(into, "the copy's directory")?,
    ])
}

/// A path as an argument, refused unless it is absolute and plain, so that it
/// can never be read as an option.
fn absolute(path: &Path, what: &str) -> Result<String, EngineError> {
    match path.to_str() {
        Some(text) if path.is_absolute() && !text.contains('\n') => Ok(text.to_owned()),
        _ => Err(EngineError::Unsupported {
            what: format!("{what} is not at an absolute, plain path"),
        }),
    }
}

/// What a run of the tool left behind.
#[derive(Debug)]
struct Run {
    exit_code: Option<i32>,
    /// The last few lines that may be shown, from both pipes.
    tail: Vec<String>,
}

/// A copy's progress: bytes in the directory it is written to, against the
/// disc's size.
struct Progress<'a> {
    directory: PathBuf,
    total: u64,
    sink: &'a dyn EventSink,
}

impl Dvdbackup {
    async fn run(
        &self,
        arguments: &[String],
        work_dir: &Path,
        timeout: Duration,
        cancel: &CancelToken,
        progress: Option<Progress<'_>>,
    ) -> Result<Run, EngineError> {
        if cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        let mut child = Command::new(&self.program)
            .args(arguments)
            .current_dir(work_dir)
            // Nothing inherited reaches libdvdcss: its method, cache and
            // verbosity are set here and nowhere else.
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("DVDCSS_CACHE", "off")
            .env("DVDCSS_VERBOSE", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| EngineError::Io {
                operation: "starting dvdbackup",
                source,
            })?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let finished = async {
            // Together, never one after the other: a pipe nobody reads fills,
            // and the tool writing to it stops.
            let (mut out, err) = tokio::join!(kept_lines(stdout), kept_lines(stderr));
            out.extend(err);
            let start = out.len().saturating_sub(KEPT_LINES);
            (out.split_off(start), child.wait().await)
        };
        let watched = async {
            let mut last_percent = None;
            let mut since_measured = Duration::ZERO;
            loop {
                tokio::time::sleep(CANCEL_POLL).await;
                if cancel.is_cancelled() {
                    return;
                }
                since_measured += CANCEL_POLL;
                if let Some(progress) = &progress
                    && since_measured >= PROGRESS_INTERVAL
                {
                    since_measured = Duration::ZERO;
                    report_progress(progress, &mut last_percent).await;
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
                operation: "dvdbackup",
                seconds: timeout.as_secs(),
            }),
            Ok(None) => Err(EngineError::Cancelled),
            Ok(Some((tail, status))) => {
                let status = status.map_err(|source| EngineError::Io {
                    operation: "waiting for dvdbackup",
                    source,
                })?;
                tracing::debug!(exit_code = ?status.code(), "dvdbackup finished");
                Ok(Run {
                    exit_code: status.code(),
                    tail,
                })
            }
        }
    }
}

/// Emit a copy's progress, once for each whole percent. Never 100 before
/// the tool has finished: the disc's size is a little more than its video.
async fn report_progress(progress: &Progress<'_>, last_percent: &mut Option<u32>) {
    let written = bytes_in(&progress.directory).await;
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let fraction = ((written as f64 / progress.total as f64) as f32).min(0.99);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let percent = (fraction * 100.0) as u32;
    if *last_percent != Some(percent) {
        *last_percent = Some(percent);
        progress.sink.emit(BurnEvent::progress(
            "ripping",
            "RIP_COPY_PROGRESS",
            fraction,
            format!("{percent}% of the decrypted copy made"),
        ));
    }
}

/// Bytes in the regular files directly in `directory`; zero when it does
/// not exist yet.
async fn bytes_in(directory: &Path) -> u64 {
    let Ok(mut entries) = tokio::fs::read_dir(directory).await else {
        return 0;
    };
    let mut total = 0;
    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Ok(metadata) = entry.metadata().await
            && metadata.is_file()
        {
            total += metadata.len();
        }
    }
    total
}

/// Read a pipe to its end, keeping only its last [`KEPT_LINES`] shareable
/// lines.
async fn kept_lines<R: AsyncRead + Unpin>(pipe: Option<R>) -> Vec<String> {
    let mut kept = std::collections::VecDeque::with_capacity(KEPT_LINES + 1);
    let Some(mut pipe) = pipe else {
        return Vec::new();
    };
    let mut keep = |line: &[u8]| {
        let line = String::from_utf8_lossy(line);
        if let Some(line) = shareable(&line) {
            kept.push_back(line);
            if kept.len() > KEPT_LINES {
                kept.pop_front();
            }
        }
    };
    let mut pending = Vec::new();
    let mut buffer = vec![0_u8; 8192];
    loop {
        match pipe.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                for &byte in &buffer[..read] {
                    if byte == b'\n' || byte == b'\r' {
                        keep(&pending);
                        pending.clear();
                    } else if pending.len() < MAX_LINE_BYTES {
                        pending.push(byte);
                    }
                }
            }
        }
    }
    keep(&pending);
    kept.into()
}

/// A line of the tool's output as it may be shown, or `None` when it must
/// not be, or says nothing.
///
/// Dropped: anything that mentions a key, anything with a long run of hex
/// digits or a `0x` number (how libdvdcss and libdvdread print keys and
/// offsets alike), and the copy's progress lines, which say nothing about a
/// failure. What is left is cut short and stripped of control characters.
#[must_use]
pub fn shareable(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let lower = trimmed.to_ascii_lowercase();
    if trimmed.is_empty()
        || lower.contains("key")
        || lower.contains("0x")
        || lower.starts_with("copying ")
        || lower.contains("elapsed time")
        || longest_hex_run(trimmed) >= 10
    {
        return None;
    }
    Some(
        trimmed
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(MAX_KEPT_LINE_CHARS)
            .collect(),
    )
}

/// The longest run of hex digits in `text`, ignoring the separators keys are
/// sometimes printed with.
fn longest_hex_run(text: &str) -> usize {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        if c.is_ascii_hexdigit() {
            run += 1;
            longest = longest.max(run);
        } else if !matches!(c, ':' | ' ' | '-') {
            run = 0;
        }
    }
    longest
}

/// Why the tool failed, in its own words where they may be shown.
fn failure(what: &str, run: &Run) -> String {
    let code = run
        .exit_code
        .map_or_else(|| "was stopped".to_owned(), |code| format!("exited {code}"));
    if run.tail.is_empty() {
        format!("dvdbackup {what}; it {code}")
    } else {
        format!("dvdbackup {what}; it {code}: {}", run.tail.join(" / "))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::engine::CollectingSink;

    fn css() -> CssDecryption {
        CssDecryption {
            library: "libdvdcss".to_owned(),
            version: "1.6.0".to_owned(),
            tool: "dvdbackup".to_owned(),
            tool_version: "0.4.2-4.1".to_owned(),
        }
    }

    /// A stand-in for dvdbackup running `body`, which sees the arguments as
    /// `$@` and records them, and its environment, beside itself.
    ///
    /// Made executable by `install`, in a process of its own: a script this
    /// process held open for writing could be inherited by a tool another
    /// test is starting at that moment, and then fail to run ("text file
    /// busy").
    #[cfg(unix)]
    fn stub(dir: &Path, body: &str) -> Dvdbackup {
        let source = dir.join("dvdbackup.sh");
        let script = dir.join("dvdbackup");
        std::fs::write(
            &source,
            format!(
                "#!/bin/sh\n\
                 here=$(dirname \"$0\")\n\
                 echo \"$@\" > \"$here/arguments\"\n\
                 env > \"$here/environment\"\n\
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
        Dvdbackup::at(script, css())
    }

    #[test]
    fn the_record_names_the_library_and_tool_and_nothing_else() {
        let record = Dvdbackup::installed(css()).decryption();
        assert_eq!(
            record,
            DecryptionRecord {
                system: "css".to_owned(),
                library: "libdvdcss".to_owned(),
                library_version: "1.6.0".to_owned(),
                tool: "dvdbackup".to_owned(),
                tool_version: "0.4.2-4.1".to_owned(),
            }
        );
        assert_eq!(
            Dvdbackup::installed(css()).program,
            Path::new("/usr/bin/dvdbackup")
        );
    }

    #[test]
    fn arguments_are_fixed_and_paths_must_be_absolute() {
        assert_eq!(
            info_arguments(Path::new("/dev/disc-block")).unwrap(),
            ["-I", "-i", "/dev/disc-block"]
        );
        assert_eq!(
            mirror_arguments(Path::new("/dev/disc-block"), Path::new("/state/rips/r")).unwrap(),
            [
                "-M",
                "-n",
                "disc",
                "-i",
                "/dev/disc-block",
                "-o",
                "/state/rips/r"
            ]
        );
        for device in ["-o/etc", "disc-block", "", "/dev/x\n-o"] {
            assert!(info_arguments(Path::new(device)).is_err(), "{device:?}");
        }
        assert!(mirror_arguments(Path::new("/dev/sr0"), Path::new("relative")).is_err());
    }

    #[test]
    fn nothing_that_could_be_key_material_is_kept() {
        for line in [
            "libdvdread: Get key for /VIDEO_TS/VTS_01_1.VOB at 0x0000076f",
            "libdvdcss debug: title key found: 4f:a1:9c:02:7e",
            "libdvdcss debug: disc key 0123456789",
            "libdvdcss: DISC KEY",
            "4fa19c027e1234",
            "4f a1 9c 02 7e",
            "libdvdread: Elapsed time 0",
            "Copying Title, part 1/7: 15% done (152/1024 MiB)",
            "   ",
        ] {
            assert_eq!(shareable(line), None, "{line:?}");
        }
    }

    #[test]
    fn plain_failures_are_kept_bounded_and_printable() {
        assert_eq!(
            shareable("  libdvdread: Can't open /dev/sr0 for reading \u{1b}[0m ").as_deref(),
            Some("libdvdread: Can't open /dev/sr0 for reading  [0m")
        );
        assert_eq!(
            shareable("Found 13 VTS's").as_deref(),
            Some("Found 13 VTS's"),
            "a short number is not a key"
        );
        let long = shareable(&"error ".repeat(100)).unwrap();
        assert_eq!(long.chars().count(), MAX_KEPT_LINE_CHARS);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn authentication_runs_the_fixed_command_in_a_clean_environment() {
        let dir = tempfile::tempdir().unwrap();
        let tool = stub(
            dir.path(),
            "echo 'libdvdread: Get key for /VIDEO_TS/VTS_01_1.VOB at 0x1'",
        );
        tool.authenticate(
            Path::new("/dev/disc-block"),
            dir.path(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("arguments")).unwrap(),
            "-I -i /dev/disc-block\n"
        );
        let environment = std::fs::read_to_string(dir.path().join("environment")).unwrap();
        assert!(environment.contains("DVDCSS_CACHE=off"), "{environment}");
        assert!(environment.contains("DVDCSS_VERBOSE=0"), "{environment}");
        assert!(
            !environment.contains("HOME="),
            "nothing inherited: {environment}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_authentication_says_why_without_anything_key_shaped() {
        let dir = tempfile::tempdir().unwrap();
        let tool = stub(
            dir.path(),
            "echo 'libdvdcss debug: title key 4f:a1:9c:02:7e' >&2\n\
             echo \"libdvdread: Can't open /dev/disc-block for reading\" >&2\n\
             exit 1",
        );
        let error = tool
            .authenticate(
                Path::new("/dev/disc-block"),
                dir.path(),
                &CancelToken::new(),
            )
            .await
            .unwrap_err();
        let EngineError::Unsupported { what } = error else {
            panic!("{error:?}")
        };
        assert!(what.contains("exited 1"), "{what}");
        assert!(what.contains("Can't open /dev/disc-block"), "{what}");
        assert!(!what.contains("4f:a1"), "{what}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_copy_lands_in_video_ts_and_reports_progress() {
        let dir = tempfile::tempdir().unwrap();
        let into = dir.path().join("decrypted");
        std::fs::create_dir(&into).unwrap();
        let tool = stub(
            dir.path(),
            "out=$7/disc/VIDEO_TS; mkdir -p \"$out\"\n\
             head -c 4096 /dev/zero > \"$out/VIDEO_TS.IFO\"\n\
             sleep 3\n\
             head -c 4096 /dev/zero > \"$out/VTS_01_1.VOB\"",
        );
        let sink = CollectingSink::new();
        tool.mirror(
            Path::new("/dev/disc-block"),
            &into,
            Some(16384),
            &CancelToken::new(),
            &sink,
        )
        .await
        .unwrap();
        assert!(into.join("VIDEO_TS/VIDEO_TS.IFO").is_file());
        assert!(into.join("VIDEO_TS/VTS_01_1.VOB").is_file());
        assert!(!into.join("disc").exists(), "tidied");
        let progress: Vec<f32> = sink
            .events()
            .iter()
            .filter(|event| event.code == "RIP_COPY_PROGRESS")
            .filter_map(|event| event.progress)
            .collect();
        assert!(!progress.is_empty(), "measured while copying");
        assert!(progress.iter().all(|fraction| *fraction < 1.0));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_withdrawn_copy_stops_the_tool() {
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
            .mirror(
                Path::new("/dev/disc-block"),
                dir.path(),
                None,
                &cancel,
                &CollectingSink::new(),
            )
            .await;
        stopper.await.unwrap();
        assert!(matches!(result, Err(EngineError::Cancelled)), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
