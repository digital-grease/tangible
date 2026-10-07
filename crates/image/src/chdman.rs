// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The chdman engine.
//!
//! chdman, from MAME, turns a disc image into a CHD. Creating one is not
//! enough to trust it: chdman 0.251 reads a cdrdao table of contents whose
//! track starts with an in-file pregap (`START`) as a gap with nothing
//! stored, and the CHD it writes holds that track's data shifted by the
//! pregap, at the right length. Nothing but a byte comparison shows it.
//!
//! So every CHD is made, verified by chdman against its own checksums, then
//! extracted again and compared with the parent, track by track, through
//! this crate's own CUE and TOC layouts rather than chdman's reading of them:
//!
//! - an ISO that comes back byte-identical is `bit_exact_repack`;
//! - a CD layout whose every track comes back with the same mode, start,
//!   length, index points and bytes is `structurally_equivalent`: the track
//!   data survives, the descriptor's text and the file split do not;
//! - anything else is refused, and no derivative is recorded.
//!
//! Arguments are fixed lists built from validated options. No text from an
//! image or a request reaches the command line except the paths this engine
//! chose.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tangible_domain::cd::SampleByteOrder;
use tangible_domain::{LogicalPath, LossCharacter, Transformation};
use tokio::io::{AsyncRead, AsyncReadExt as _};

use crate::derive::{DerivationEngine, DeriveError, DeriveReport, DeriveRequest};
use crate::{cue, toc};

/// Where the image installs chdman.
pub const CHDMAN: &str = "/usr/bin/chdman";

/// The first chdman release with `createdvd`.
const FIRST_DVD_MINOR: u32 = 262;

/// Output kept from each pipe: chdman redraws a progress line with carriage
/// returns, which on a large image is megabytes nobody reads.
const OUTPUT_LIMIT: usize = 64 * 1024;

/// Longest a single chdman run may take. A Blu-ray image compresses for a
/// long time on a slow machine; a run that exceeds this is stuck.
const RUN_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);

/// Bytes compared at a time.
const COMPARE_CHUNK: usize = 1 << 20;

/// chdman, at a known path and version.
#[derive(Debug, Clone)]
pub struct Chdman {
    program: PathBuf,
    version: String,
    minor: u32,
}

impl Chdman {
    /// Run `program` once to learn its version.
    ///
    /// # Errors
    ///
    /// [`DeriveError::Unavailable`] when it cannot be run or does not say
    /// what version it is.
    pub async fn probe(program: impl Into<PathBuf>) -> Result<Self, DeriveError> {
        let program = program.into();
        let unavailable = |detail: String| DeriveError::Unavailable {
            tool: "chdman",
            detail,
        };
        // chdman with no arguments prints its banner and usage, and exits
        // non-zero; the banner is what is wanted.
        let output = run(
            &program,
            &[],
            &std::env::temp_dir(),
            Duration::from_secs(30),
        )
        .await
        .map_err(|e| unavailable(e.to_string()))?;
        let text = format!("{}\n{}", output.stdout, output.stderr);
        let version = parse_version(&text)
            .ok_or_else(|| unavailable("it did not report a version".to_owned()))?;
        let minor = version
            .split('.')
            .nth(1)
            .and_then(|m| m.parse().ok())
            .unwrap_or(0);
        Ok(Self {
            program,
            version,
            minor,
        })
    }

    async fn chdman(
        &self,
        args: &[&str],
        cwd: &Path,
        step: &'static str,
    ) -> Result<RunOutput, DeriveError> {
        let output = run(&self.program, args, cwd, RUN_TIMEOUT)
            .await
            .map_err(|e| DeriveError::Io {
                operation: step,
                source: e,
            })?;
        if output.success {
            Ok(output)
        } else {
            Err(DeriveError::Failed {
                code: "CHDMAN_FAILED",
                detail: format!("chdman {step} failed: {}", last_lines(&output)),
                retryable: false,
            })
        }
    }
}

#[async_trait]
impl DerivationEngine for Chdman {
    fn tool_name(&self) -> &'static str {
        "chdman"
    }

    fn tool_version(&self) -> String {
        self.version.clone()
    }

    fn check_supported(&self, transformation: Transformation) -> Result<(), String> {
        match transformation {
            Transformation::ChdCreateCd => Ok(()),
            Transformation::ChdCreateDvd if self.minor >= FIRST_DVD_MINOR => Ok(()),
            Transformation::ChdCreateDvd => Err(format!(
                "chdman {} cannot make DVD CHDs; createdvd arrived in 0.{FIRST_DVD_MINOR}",
                self.version
            )),
        }
    }

    async fn derive(&self, request: &DeriveRequest<'_>) -> Result<DeriveReport, DeriveError> {
        self.check_supported(request.transformation)
            .map_err(|detail| DeriveError::Failed {
                code: "DERIVATION_UNSUPPORTED",
                detail,
                retryable: false,
            })?;
        let input = request.input_dir.join(request.input.as_str());
        let input_text = path_arg(&input)?;
        let output_text = path_arg(request.output)?;
        let codecs = request
            .options
            .compression
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let hunk = request.options.hunk_bytes.to_string();
        let create = match request.transformation {
            Transformation::ChdCreateCd => "createcd",
            Transformation::ChdCreateDvd => "createdvd",
        };
        let result = self
            .make_and_check(request, create, &input_text, &output_text, &codecs, &hunk)
            .await;
        if result.is_err() {
            // Nothing that failed a check may be left where a caller would
            // find it.
            let _ = tokio::fs::remove_file(request.output).await;
        }
        result
    }
}

impl Chdman {
    async fn make_and_check(
        &self,
        request: &DeriveRequest<'_>,
        create: &str,
        input: &str,
        output: &str,
        codecs: &str,
        hunk: &str,
    ) -> Result<DeriveReport, DeriveError> {
        let cwd = request.input_dir;
        self.chdman(
            &[create, "-i", input, "-o", output, "-c", codecs, "-hs", hunk],
            cwd,
            "create",
        )
        .await?;
        self.chdman(&["verify", "-i", output], cwd, "verify")
            .await?;

        let check_dir = request.output.with_extension("check");
        let _ = tokio::fs::remove_dir_all(&check_dir).await;
        tokio::fs::create_dir_all(&check_dir)
            .await
            .map_err(|source| DeriveError::Io {
                operation: "preparing the round-trip check",
                source,
            })?;
        let outcome = self.round_trip(request, output, &check_dir).await;
        let _ = tokio::fs::remove_dir_all(&check_dir).await;
        outcome
    }

    /// Extract the CHD again and compare it with the parent.
    async fn round_trip(
        &self,
        request: &DeriveRequest<'_>,
        output: &str,
        check_dir: &Path,
    ) -> Result<DeriveReport, DeriveError> {
        let back_cue = check_dir.join("back.cue");
        let back_bin = check_dir.join("back.bin");
        let not_equivalent = |detail: String| DeriveError::Failed {
            code: "DERIVATION_NOT_EQUIVALENT",
            detail,
            retryable: false,
        };
        match request.transformation {
            Transformation::ChdCreateDvd => {
                let back = check_dir.join("back.iso");
                self.chdman(
                    &["extractdvd", "-i", output, "-o", &path_arg(&back)?],
                    check_dir,
                    "extract",
                )
                .await?;
                let original = request.input_dir.join(request.input.as_str());
                compare_files(&original, &back)
                    .await
                    .map_err(|e| io("comparing the round trip", e))?
                    .map_err(not_equivalent)?;
                Ok(DeriveReport {
                    loss_character: LossCharacter::BitExactRepack,
                    notes: vec!["extracted again and identical to the ISO".to_owned()],
                })
            }
            Transformation::ChdCreateCd => {
                self.chdman(
                    &[
                        "extractcd",
                        "-i",
                        output,
                        "-o",
                        &path_arg(&back_cue)?,
                        "-ob",
                        &path_arg(&back_bin)?,
                    ],
                    check_dir,
                    "extract",
                )
                .await?;
                let is_iso = request
                    .input
                    .as_str()
                    .rsplit_once('.')
                    .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("iso"));
                if is_iso {
                    let original = request.input_dir.join(request.input.as_str());
                    compare_files(&original, &back_bin)
                        .await
                        .map_err(|e| io("comparing the round trip", e))?
                        .map_err(not_equivalent)?;
                    return Ok(DeriveReport {
                        loss_character: LossCharacter::BitExactRepack,
                        notes: vec!["extracted again and identical to the ISO".to_owned()],
                    });
                }
                let parent = disc_layout(request.input_dir, request.input)
                    .await
                    .map_err(|detail| DeriveError::Failed {
                        code: "DERIVATION_PARENT_UNREADABLE",
                        detail,
                        retryable: false,
                    })?;
                let back_name =
                    LogicalPath::parse("back.cue").map_err(|e| DeriveError::Failed {
                        code: "DERIVATION_INTERNAL",
                        detail: e.to_string(),
                        retryable: false,
                    })?;
                let back = disc_layout(check_dir, &back_name).await.map_err(|detail| {
                    not_equivalent(format!("chdman's extracted sheet: {detail}"))
                })?;
                compare_layouts(&parent, &back)
                    .await
                    .map_err(|e| io("comparing the round trip", e))?
                    .map_err(not_equivalent)?;
                Ok(DeriveReport {
                    loss_character: LossCharacter::StructurallyEquivalent,
                    notes: vec![format!(
                        "extracted again: all {} tracks identical in mode, position, index points and bytes",
                        parent.len()
                    )],
                })
            }
        }
    }
}

fn io(operation: &'static str, source: std::io::Error) -> DeriveError {
    DeriveError::Io { operation, source }
}

/// A path as an argument. Paths here are ones this engine built under a
/// staging directory, so they are UTF-8; refusing otherwise is cheaper than
/// passing an argument chdman might read differently.
fn path_arg(path: &Path) -> Result<String, DeriveError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| DeriveError::Failed {
            code: "DERIVATION_INTERNAL",
            detail: format!("{} is not a UTF-8 path", path.display()),
            retryable: false,
        })
}

/// The version in chdman's banner: `... (CHD) manager 0.251 (unknown)`.
fn parse_version(text: &str) -> Option<String> {
    let line = text
        .lines()
        .find(|l| l.contains("chdman") && l.contains("manager"))?;
    let after = line.split("manager").nth(1)?.trim();
    let version = after.split_whitespace().next()?;
    let valid = version.split('.').count() >= 2
        && version
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    valid.then(|| version.to_owned())
}

/// One track as stored: where its bytes are and what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackBytes {
    /// Track number.
    pub number: u32,
    /// Mode as declared.
    pub mode: String,
    /// The file holding it.
    pub file: PathBuf,
    /// Where its first present sector starts in that file.
    pub offset: u64,
    /// Bytes of it present in the file.
    pub length: u64,
    /// Image-relative LBA of its first present sector.
    pub start_lba: u64,
    /// Total gap before INDEX 01.
    pub pregap_sectors: u64,
    /// Index points, relative to the first present sector.
    pub indexes: Vec<(u32, u64)>,
    /// How an audio track's file stores its samples: a CUE `BINARY` file is
    /// little-endian, a cdrdao TOC's data file big-endian.
    pub sample_byte_order: Option<SampleByteOrder>,
}

/// A parsed descriptor.
enum Descriptor {
    Toc(toc::TocDocument),
    Cue(cue::CueSheet),
}

/// Lay out the disc a CUE sheet or cdrdao TOC in `dir` describes.
///
/// # Errors
///
/// Why it cannot be laid out, in words.
pub async fn disc_layout(dir: &Path, descriptor: &LogicalPath) -> Result<Vec<TrackBytes>, String> {
    let bytes = tokio::fs::read(dir.join(descriptor.as_str()))
        .await
        .map_err(|e| format!("reading {}: {e}", descriptor.as_str()))?;
    let staged = staged_files(dir)
        .await
        .map_err(|e| format!("listing the files: {e}"))?;
    let is_toc = descriptor
        .as_str()
        .rsplit_once('.')
        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("toc"));
    let parsed = if is_toc {
        Descriptor::Toc(toc::parse(&bytes).map_err(|e| e.to_string())?)
    } else {
        Descriptor::Cue(cue::parse(&bytes).map_err(|e| e.to_string())?)
    };
    let names = match &parsed {
        Descriptor::Toc(document) => document.files.clone(),
        Descriptor::Cue(sheet) => sheet.files.iter().map(|f| f.name.clone()).collect(),
    };
    let resolution = cue::resolve_names(&names, &staged);
    let mut files = Vec::new();
    let mut sizes = Vec::new();
    for file in &resolution.files {
        let resolved = file
            .resolved
            .as_ref()
            .ok_or_else(|| format!("{} names a file that is not there", file.declared))?;
        let path = dir.join(resolved.as_str());
        let size = tokio::fs::metadata(&path)
            .await
            .map_err(|e| format!("reading {}: {e}", resolved.as_str()))?
            .len();
        files.push(path);
        sizes.push(size);
    }
    let layout = match &parsed {
        Descriptor::Toc(document) => toc::layout(document, &sizes)
            .map(|l| l.layout)
            .map_err(|e| e.to_string())?,
        Descriptor::Cue(sheet) => cue::layout(sheet, &sizes).map_err(|e| e.to_string())?,
    };
    layout
        .tracks
        .iter()
        .map(|track| {
            let sector = u64::from(
                tangible_domain::cd::sector_bytes(&track.mode)
                    .ok_or_else(|| format!("track {} has no known sector size", track.number))?,
            );
            Ok(TrackBytes {
                number: track.number,
                mode: track.mode.to_ascii_uppercase(),
                file: files
                    .get(track.file)
                    .cloned()
                    .ok_or_else(|| format!("track {} names no file", track.number))?,
                offset: track.file_offset_bytes,
                length: track.sector_count * sector,
                start_lba: track.start_lba,
                pregap_sectors: track.pregap_sectors,
                indexes: track.indexes.clone(),
                sample_byte_order: track.sample_byte_order,
            })
        })
        .collect()
}

/// Every regular file under `dir`, as logical paths.
async fn staged_files(dir: &Path) -> std::io::Result<Vec<LogicalPath>> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        let mut entries = tokio::fs::read_dir(&current).await?;
        while let Some(entry) = entries.next_entry().await? {
            let kind = entry.file_type().await?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file()
                && let Ok(relative) = entry.path().strip_prefix(dir)
                && let Some(text) = relative.to_str()
                && let Ok(path) = LogicalPath::parse(text)
            {
                found.push(path);
            }
        }
    }
    Ok(found)
}

/// Compare two discs track by track. `Ok(Err(reason))` when they differ.
///
/// # Errors
///
/// When a file cannot be read.
pub async fn compare_layouts(
    parent: &[TrackBytes],
    back: &[TrackBytes],
) -> std::io::Result<Result<(), String>> {
    if parent.len() != back.len() {
        return Ok(Err(format!(
            "the parent has {} tracks and the CHD {}",
            parent.len(),
            back.len()
        )));
    }
    for (p, b) in parent.iter().zip(back) {
        let differs = |what: &str, a: &dyn std::fmt::Debug, c: &dyn std::fmt::Debug| {
            Err(format!(
                "track {}: {what} is {a:?} in the parent and {c:?} in the CHD",
                p.number
            ))
        };
        if p.number != b.number {
            return Ok(differs("the number", &p.number, &b.number));
        }
        if p.mode != b.mode {
            return Ok(differs("the mode", &p.mode, &b.mode));
        }
        if p.start_lba != b.start_lba {
            return Ok(differs("the start", &p.start_lba, &b.start_lba));
        }
        if p.length != b.length {
            return Ok(differs("the length in bytes", &p.length, &b.length));
        }
        if p.pregap_sectors != b.pregap_sectors {
            return Ok(differs("the pregap", &p.pregap_sectors, &b.pregap_sectors));
        }
        if p.indexes != b.indexes {
            return Ok(differs("the index points", &p.indexes, &b.indexes));
        }
        // The same samples in the other byte order are the same audio: chdman
        // turns a TOC's big-endian samples into the little-endian a CUE
        // BINARY file holds, and that is a faithful copy, not a change.
        let swapped = matches!(
            (p.sample_byte_order, b.sample_byte_order),
            (Some(x), Some(y)) if x != y
        );
        if let Some(at) =
            first_difference(&p.file, p.offset, &b.file, b.offset, p.length, swapped).await?
        {
            return Ok(Err(format!(
                "track {}: the bytes differ from byte {at} of the track",
                p.number
            )));
        }
    }
    Ok(Ok(()))
}

/// Compare two whole files. `Ok(Err(reason))` when they differ.
///
/// # Errors
///
/// When a file cannot be read.
pub async fn compare_files(a: &Path, b: &Path) -> std::io::Result<Result<(), String>> {
    let (len_a, len_b) = (
        tokio::fs::metadata(a).await?.len(),
        tokio::fs::metadata(b).await?.len(),
    );
    if len_a != len_b {
        return Ok(Err(format!(
            "the parent is {len_a} bytes and the extracted image {len_b}"
        )));
    }
    Ok(match first_difference(a, 0, b, 0, len_a, false).await? {
        Some(at) => Err(format!("the bytes differ from byte {at}")),
        None => Ok(()),
    })
}

/// The first offset, from the start of the range, where two ranges differ.
/// With `swapped`, the second range is read as the first with the bytes of
/// every 16-bit sample exchanged.
async fn first_difference(
    a: &Path,
    a_offset: u64,
    b: &Path,
    b_offset: u64,
    length: u64,
    swapped: bool,
) -> std::io::Result<Option<u64>> {
    use tokio::io::AsyncSeekExt as _;
    let mut fa = tokio::fs::File::open(a).await?;
    let mut fb = tokio::fs::File::open(b).await?;
    fa.seek(std::io::SeekFrom::Start(a_offset)).await?;
    fb.seek(std::io::SeekFrom::Start(b_offset)).await?;
    let mut buf_a = vec![0_u8; COMPARE_CHUNK];
    let mut buf_b = vec![0_u8; COMPARE_CHUNK];
    let mut done = 0_u64;
    while done < length {
        let want =
            usize::try_from((length - done).min(COMPARE_CHUNK as u64)).unwrap_or(COMPARE_CHUNK);
        fa.read_exact(&mut buf_a[..want]).await?;
        fb.read_exact(&mut buf_b[..want]).await?;
        if swapped {
            // Chunks are whole sectors' worth of even length, so samples
            // never straddle two reads.
            for pair in buf_b[..want].chunks_exact_mut(2) {
                pair.swap(0, 1);
            }
        }
        if let Some(i) = buf_a[..want]
            .iter()
            .zip(&buf_b[..want])
            .position(|(x, y)| x != y)
        {
            return Ok(Some(done + i as u64));
        }
        done += want as u64;
    }
    Ok(None)
}

/// What a run printed, bounded.
#[derive(Debug, Clone)]
struct RunOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

fn last_lines(output: &RunOutput) -> String {
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
    lines[lines.len().saturating_sub(3)..].join(" / ")
}

/// Run a program with fixed arguments, a working directory, both pipes read
/// at once and bounded, and a time limit after which it is killed.
async fn run(
    program: &Path,
    args: &[&str],
    cwd: &Path,
    timeout: Duration,
) -> std::io::Result<RunOutput> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let reading = async { tokio::join!(read_bounded(stdout), read_bounded(stderr)) };
    let finished = tokio::time::timeout(timeout, async {
        let (out, err) = reading.await;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((status, out, err))
    })
    .await;
    if let Ok(result) = finished {
        let (status, stdout, stderr) = result?;
        Ok(RunOutput {
            success: status.success(),
            stdout,
            stderr,
        })
    } else {
        // chdman starts no processes of its own, so killing it is enough; its
        // own process group keeps a terminal's signals off it.
        let _ = child.kill().await;
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "{} took longer than {}s",
                program.display(),
                timeout.as_secs()
            ),
        ))
    }
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

    #[test]
    fn the_version_is_read_from_the_banner() {
        assert_eq!(
            parse_version(
                "chdman - MAME Compressed Hunks of Data (CHD) manager 0.251 (unknown)\nUsage:"
            ),
            Some("0.251".to_owned())
        );
        assert_eq!(
            parse_version("chdman - MAME Compressed Hunks of Data (CHD) manager 0.262 (mame0262)"),
            Some("0.262".to_owned())
        );
        assert_eq!(parse_version("something else entirely"), None);
        assert_eq!(parse_version("chdman ... manager banana"), None);
    }

    #[test]
    fn dvd_chds_need_a_chdman_that_has_createdvd() {
        let old = Chdman {
            program: PathBuf::from(CHDMAN),
            version: "0.251".to_owned(),
            minor: 251,
        };
        assert!(old.check_supported(Transformation::ChdCreateCd).is_ok());
        let refused = old
            .check_supported(Transformation::ChdCreateDvd)
            .unwrap_err();
        assert!(
            refused.contains("0.251") && refused.contains("0.262"),
            "{refused}"
        );
        let new = Chdman {
            minor: 262,
            version: "0.262".to_owned(),
            ..old
        };
        assert!(new.check_supported(Transformation::ChdCreateDvd).is_ok());
    }

    #[test]
    fn the_last_lines_skip_progress_noise() {
        let output = RunOutput {
            success: false,
            stdout: String::new(),
            stderr: "Compressing, 10% complete...\rCompressing, 20% complete...\r\nError: bad input\nFatal error occurred: 1\n".to_owned(),
        };
        assert_eq!(
            last_lines(&output),
            "Compressing, 20% complete... / Error: bad input / Fatal error occurred: 1"
        );
    }

    #[tokio::test]
    async fn output_is_bounded() {
        let big = vec![b'x'; 5 * OUTPUT_LIMIT];
        let text = read_bounded(Some(&big[..])).await;
        assert_eq!(text.len(), OUTPUT_LIMIT);
    }

    #[tokio::test]
    async fn file_and_range_comparison_find_the_first_difference() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let mut bytes = vec![7_u8; COMPARE_CHUNK + 100];
        std::fs::write(&a, &bytes).unwrap();
        std::fs::write(&b, &bytes).unwrap();
        assert_eq!(compare_files(&a, &b).await.unwrap(), Ok(()));
        bytes[COMPARE_CHUNK + 5] = 0;
        std::fs::write(&b, &bytes).unwrap();
        assert_eq!(
            compare_files(&a, &b).await.unwrap(),
            Err(format!("the bytes differ from byte {}", COMPARE_CHUNK + 5))
        );
    }
}
