// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The chdman engine.
//!
//! chdman, from MAME, turns a disc image into a CHD. Creating one is not
//! enough to trust it: chdman 0.251 and 0.276 (Debian 12's and 13's) read a
//! cdrdao table of contents whose track starts with an in-file pregap
//! (`START`) as a gap with nothing stored, and the CHD they write holds
//! that track's data shifted by the pregap, at the right length. Nothing but
//! a byte comparison shows it.
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
//! A CHD imported as an original is extracted the other way
//! (docs/plans/chd-images.md): verified by chdman, extracted, and checked
//! against the CHD's own data SHA-1, which covers every frame or sector:
//!
//! - a CUE/BIN is made into a CHD again, whose data SHA-1 must be the
//!   original's;
//! - an ISO's own SHA-1 must be the DVD CHD's data SHA-1.
//!
//! Either is `structurally_equivalent`: the data is the same, the container
//! is not.
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

use crate::derive::{DerivationEngine, DeriveError, DeriveReport, DeriveRequest, bin_beside};
use crate::{chd, cue, toc};

/// Where the image installs chdman.
pub const CHDMAN: &str = "/usr/bin/chdman";

/// The first chdman release with `createdvd`.
const FIRST_DVD_MINOR: u32 = 262;

/// The longest line, in bytes before its line ending, of a sheet chdman is
/// given. chdman reads a sheet a line at a time with `fgets` into 511 bytes,
/// so a longer line goes on as a second one: a long comment could carry a
/// FILE line that Tangible, reading the line whole, never saw (security
/// review, 2026-11-12). Well under, with room for any line ending; a FILE
/// line naming a 255-byte file fits.
const MAX_SHEET_LINE_BYTES: usize = 400;

/// What chdman is given instead of a CUE sheet: Tangible's own copy, whose
/// FILE lines name the staged files (see [`sheet_for_chdman`]).
const CHDMAN_SHEET_STEM: &str = "tangible-chdman";

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

    /// The version it reported.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Check a CHD against the SHA-1s written in it when it was made: every
    /// hunk decompressed and hashed. It only reads the file.
    ///
    /// # Errors
    ///
    /// [`DeriveError::Failed`] (`CHDMAN_FAILED`) when the CHD does not
    /// verify or chdman cannot read it, with the end of what it said: not
    /// retryable when chdman exited so, retryable when a signal ended it.
    /// [`DeriveError::Io`] when it could not be run or took too long.
    pub async fn verify(&self, chd: &Path) -> Result<(), DeriveError> {
        let input = path_arg(chd)?;
        self.chdman(&["verify", "-i", &input], &std::env::temp_dir(), "verify")
            .await
            .map(|_| ())
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
                // Ended by a signal (the system short of memory, a
                // shutdown), it said nothing about the file.
                retryable: !output.exited,
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
            // createcd, extractcd and extractraw are in every chdman that
            // writes version 5.
            Transformation::ChdCreateCd
            | Transformation::CueBinFromChd
            | Transformation::IsoFromChd => Ok(()),
            Transformation::ChdCreateDvd if self.minor >= FIRST_DVD_MINOR => Ok(()),
            Transformation::ChdCreateDvd => Err(format!(
                "chdman {} cannot make DVD CHDs; createdvd arrived in 0.{FIRST_DVD_MINOR}",
                self.version
            )),
            Transformation::CueBinFromCcd
            | Transformation::CueBinFromMds
            | Transformation::IsoFromMds => Err(
                "chdman does not make CUE/BIN or ISO images of these; Tangible makes them itself"
                    .to_owned(),
            ),
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
        let result = match request.transformation {
            Transformation::ChdCreateCd if !has_extension(request.input, "iso") => {
                // chdman opens the files a sheet names, as it reads them. It
                // is given one whose every such file is staged here, or none.
                let sheet = sheet_for_chdman(request.input_dir, request.input).await?;
                let (codecs, hunk) = chd_options(request)?;
                self.make_and_check(
                    request,
                    "createcd",
                    &path_arg(&sheet)?,
                    &output_text,
                    &codecs,
                    &hunk,
                )
                .await
            }
            Transformation::ChdCreateCd | Transformation::ChdCreateDvd => {
                let (codecs, hunk) = chd_options(request)?;
                let create = if request.transformation == Transformation::ChdCreateCd {
                    "createcd"
                } else {
                    "createdvd"
                };
                self.make_and_check(request, create, &input_text, &output_text, &codecs, &hunk)
                    .await
            }
            Transformation::CueBinFromChd => {
                self.extract_cd(request, &input, &input_text, &output_text)
                    .await
            }
            Transformation::IsoFromChd => {
                self.extract_dvd(request, &input, &input_text, &output_text)
                    .await
            }
            // Refused by check_supported above.
            Transformation::CueBinFromCcd
            | Transformation::CueBinFromMds
            | Transformation::IsoFromMds => Err(DeriveError::Failed {
                code: "DERIVATION_UNSUPPORTED",
                detail: "chdman does not make CUE/BIN or ISO images of these".to_owned(),
                retryable: false,
            }),
        };
        if result.is_err() {
            // Nothing that failed a check may be left where a caller would
            // find it.
            let _ = tokio::fs::remove_file(request.output).await;
            if request.transformation == Transformation::CueBinFromChd {
                let _ = tokio::fs::remove_file(bin_beside(request.output)).await;
            }
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

    /// Extract a CD CHD into the CUE/BIN at `output`, and check it by making
    /// it into a CHD again: that CHD's data SHA-1 must be the original's.
    async fn extract_cd(
        &self,
        request: &DeriveRequest<'_>,
        input: &Path,
        input_text: &str,
        output: &str,
    ) -> Result<DeriveReport, DeriveError> {
        let cwd = request.input_dir;
        let bin = path_arg(&bin_beside(request.output))?;
        self.chdman(&["verify", "-i", input_text], cwd, "verify")
            .await?;
        self.chdman(
            &["extractcd", "-i", input_text, "-o", output, "-ob", &bin],
            cwd,
            "extract",
        )
        .await?;

        let check_dir = request.output.with_extension("check");
        let _ = tokio::fs::remove_dir_all(&check_dir).await;
        tokio::fs::create_dir_all(&check_dir)
            .await
            .map_err(|source| io("preparing the round-trip check", source))?;
        let back = check_dir.join("back.chd");
        let outcome = async {
            self.chdman(
                &["createcd", "-i", output, "-o", &path_arg(&back)?],
                &check_dir,
                "make again",
            )
            .await?;
            same_data(input, &back).await
        }
        .await;
        let _ = tokio::fs::remove_dir_all(&check_dir).await;
        outcome?;
        Ok(DeriveReport {
            loss_character: LossCharacter::StructurallyEquivalent,
            notes: vec![
                "chdman verified the CHD against its own SHA-1s".to_owned(),
                "made into a CHD again, it has the original's data SHA-1: every frame is the same"
                    .to_owned(),
            ],
        })
    }

    /// Extract a DVD CHD's data, which is the ISO, to `output`, and check its
    /// SHA-1 is the CHD's data SHA-1.
    async fn extract_dvd(
        &self,
        request: &DeriveRequest<'_>,
        input: &Path,
        input_text: &str,
        output: &str,
    ) -> Result<DeriveReport, DeriveError> {
        let cwd = request.input_dir;
        self.chdman(&["verify", "-i", input_text], cwd, "verify")
            .await?;
        // extractraw rather than extractdvd, which chdman before 0.262
        // lacks: a DVD CHD's data is the ISO's bytes, whatever made it.
        self.chdman(
            &["extractraw", "-i", input_text, "-o", output],
            cwd,
            "extract",
        )
        .await?;
        let header = read_header(input).await?;
        let digest = sha1_of(request.output)
            .await
            .map_err(|source| io("hashing the ISO", source))?;
        if digest != header.raw_sha1 {
            return Err(DeriveError::Failed {
                code: "DERIVATION_NOT_EQUIVALENT",
                detail: format!(
                    "the extracted ISO's SHA-1 is {}, and the CHD's data SHA-1 {}",
                    chd::hex(&digest),
                    chd::hex(&header.raw_sha1)
                ),
                retryable: false,
            });
        }
        Ok(DeriveReport {
            loss_character: LossCharacter::StructurallyEquivalent,
            notes: vec![
                "chdman verified the CHD against its own SHA-1s".to_owned(),
                "its SHA-1 is the CHD's data SHA-1: every sector is the same".to_owned(),
            ],
        })
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
            Transformation::CueBinFromCcd
            | Transformation::CueBinFromMds
            | Transformation::IsoFromMds
            | Transformation::CueBinFromChd
            | Transformation::IsoFromChd => Err(DeriveError::Failed {
                code: "DERIVATION_UNSUPPORTED",
                detail: "only a CHD that was made is extracted again".to_owned(),
                retryable: false,
            }),
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
                if has_extension(request.input, "iso") {
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

/// A CHD's header and metadata, read from the file: only those, never the
/// hunks, however large the CHD.
async fn read_chd(path: &Path) -> Result<chd::ChdDescription, DeriveError> {
    let owned = path.to_path_buf();
    let read = tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&owned)?;
        let length = file.metadata()?.len();
        Ok::<_, std::io::Error>(chd::read(&mut std::io::BufReader::new(file), length))
    })
    .await
    .map_err(|join| {
        io(
            "reading a CHD header",
            std::io::Error::other(join.to_string()),
        )
    })?
    .map_err(|source| io("reading a CHD header", source))?;
    read.map_err(|e| DeriveError::Failed {
        code: "DERIVATION_NOT_EQUIVALENT",
        detail: format!("{}: {e}", path.display()),
        retryable: false,
    })
}

/// A CHD's header.
async fn read_header(path: &Path) -> Result<chd::ChdHeader, DeriveError> {
    read_chd(path).await.map(|description| description.header)
}

/// Whether two CHDs hold the same disc: the same data, by their data SHA-1s
/// and sizes, and the same tracks in their metadata (type, subcode, frames,
/// pregap and whether it is in the data, postgap), which the data SHA-1
/// does not cover.
async fn same_data(original: &Path, again: &Path) -> Result<(), DeriveError> {
    let (original, again) = (read_chd(original).await?, read_chd(again).await?);
    let refused = |detail: String| DeriveError::Failed {
        code: "DERIVATION_NOT_EQUIVALENT",
        detail,
        retryable: false,
    };
    if original.header.raw_sha1 != again.header.raw_sha1
        || original.header.logical_bytes != again.header.logical_bytes
    {
        return Err(refused(format!(
            "made into a CHD again, the CUE/BIN's data SHA-1 is {} and the original's {}",
            chd::hex(&again.header.raw_sha1),
            chd::hex(&original.header.raw_sha1)
        )));
    }
    if original.tracks != again.tracks {
        return Err(refused(
            "made into a CHD again, the CUE/BIN's tracks are not the original's".to_owned(),
        ));
    }
    Ok(())
}

/// A file's SHA-1, read a chunk at a time.
async fn sha1_of(path: &Path) -> std::io::Result<[u8; 20]> {
    use sha1::Digest as _;
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = sha1::Sha1::new();
    let mut buffer = vec![0_u8; COMPARE_CHUNK];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
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

/// The compression and hunk size a CHD is made with, as chdman takes them.
fn chd_options(request: &DeriveRequest<'_>) -> Result<(String, String), DeriveError> {
    let options = request.options.ok_or_else(|| DeriveError::Failed {
        code: "DERIVATION_INTERNAL",
        detail: "a CHD is made with its options, and none were given".to_owned(),
        retryable: false,
    })?;
    let codecs = options
        .compression
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    Ok((codecs, options.hunk_bytes.to_string()))
}

/// Whether `path` ends `.<extension>`, in any case.
fn has_extension(path: &LogicalPath, extension: &str) -> bool {
    path.as_str()
        .rsplit_once('.')
        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case(extension))
}

fn sheet_refused(detail: String) -> DeriveError {
    DeriveError::Failed {
        code: "DERIVATION_SHEET_REFUSED",
        detail,
        retryable: false,
    }
}

/// The sheet chdman is to read for the CUE or TOC `input` staged in `dir`.
///
/// chdman opens every file its sheet names, relative to the sheet, as it
/// reads the names, and that is not always as Tangible does: Tangible reduces
/// `../../x` to `x` to find a staged file, and chdman follows it out of the
/// staging area. A derivation's error then told an operator about the file
/// it reached (security review, 2026-11-12). So:
///
/// - a CUE sheet is rewritten, every FILE naming the staged file Tangible
///   resolved it to, and the copy written beside the staged files, under a
///   name no staged file has; the staged original is left as it is;
/// - a TOC is given as it is, there being no rewriter for it;
///
/// and either way the sheet chdman will read is read again as chdman reads
/// it ([`chdman_named_files`]): every file it names must be one staged here,
/// and no line may be long enough for chdman to split. Otherwise nothing is
/// run. The round trip still compares the CHD with the original sheet.
async fn sheet_for_chdman(dir: &Path, input: &LogicalPath) -> Result<PathBuf, DeriveError> {
    let bytes = tokio::fs::read(dir.join(input.as_str()))
        .await
        .map_err(|e| io("reading the sheet", e))?;
    let staged = staged_files(dir)
        .await
        .map_err(|e| io("listing the staged files", e))?;
    if has_extension(input, "toc") {
        let base = input
            .as_str()
            .rsplit_once('/')
            .map(|(parent, _)| parent.to_owned());
        check_chdman_names(&bytes, true, base.as_deref(), &staged)?;
        return Ok(dir.join(input.as_str()));
    }

    let sheet = cue::parse(&bytes).map_err(|e| sheet_refused(e.to_string()))?;
    let names: Vec<String> = sheet.files.iter().map(|file| file.name.clone()).collect();
    let resolution = cue::resolve_names(&names, &staged);
    let mut renames = std::collections::BTreeMap::new();
    for file in &resolution.files {
        let resolved = file.resolved.as_ref().ok_or_else(|| {
            sheet_refused(format!(
                "{:?} names a file that is not staged",
                file.declared
            ))
        })?;
        renames.insert(file.declared.clone(), resolved.as_str().to_owned());
    }
    let rewritten = cue::rewrite_file_names(&bytes, |name| renames.get(name).cloned())
        .map_err(|e| sheet_refused(e.to_string()))?;
    check_chdman_names(&rewritten, false, None, &staged)?;

    // One more name than there are staged files is sure to hold a free one.
    let taken = |name: &str| staged.iter().any(|path| path.as_str() == name);
    let name = std::iter::once(format!("{CHDMAN_SHEET_STEM}.cue"))
        .chain((1..=staged.len()).map(|n| format!("{CHDMAN_SHEET_STEM}-{n}.cue")))
        .find(|name| !taken(name))
        .unwrap_or_default();
    let path = dir.join(name);
    tokio::fs::write(&path, &rewritten)
        .await
        .map_err(|e| io("writing the sheet chdman reads", e))?;
    Ok(path)
}

/// Refuse a sheet that chdman would read a file from that is not staged.
/// `base` is the sheet's folder among the staged files, `None` at the top.
fn check_chdman_names(
    sheet: &[u8],
    toc: bool,
    base: Option<&str>,
    staged: &[LogicalPath],
) -> Result<(), DeriveError> {
    for (line, name) in chdman_named_files(sheet, toc)? {
        let full = match base {
            Some(base) => format!("{base}/{name}"),
            None => name.clone(),
        };
        let is_staged = LogicalPath::parse(&full).is_ok_and(|path| staged.contains(&path));
        if !is_staged {
            return Err(sheet_refused(format!(
                "line {line} of the sheet names {name:?}, which is not a file staged for this \
                 derivation"
            )));
        }
    }
    Ok(())
}

/// Every file chdman would open for `sheet`, with its line number, read as
/// chdman reads it: a line at a time, the first two tokens of each, a token
/// running to unquoted white space (C's `isspace`) with either kind of quote
/// toggling and dropped, and nothing past a NUL. A CUE's command is `FILE`, a
/// TOC's `FILE`, `DATAFILE` or `AUDIOFILE`, compared exactly, as chdman does.
///
/// # Errors
///
/// A line longer than [`MAX_SHEET_LINE_BYTES`], which chdman might read as
/// two, or a name that is not UTF-8.
fn chdman_named_files(sheet: &[u8], toc: bool) -> Result<Vec<(usize, String)>, DeriveError> {
    let mut named = Vec::new();
    for (index, line) in sheet.split(|byte| *byte == b'\n').enumerate() {
        let number = index + 1;
        if line.len() > MAX_SHEET_LINE_BYTES {
            return Err(sheet_refused(format!(
                "line {number} of the sheet is longer than {MAX_SHEET_LINE_BYTES} bytes, \
                 which chdman may read as two lines"
            )));
        }
        let (command, at) = chdman_token(line, 0);
        let names_a_file = if toc {
            matches!(command.as_slice(), b"FILE" | b"DATAFILE" | b"AUDIOFILE")
        } else {
            command == b"FILE"
        };
        if !names_a_file {
            continue;
        }
        let (name, _) = chdman_token(line, at);
        let name = String::from_utf8(name).map_err(|_| {
            sheet_refused(format!(
                "line {number} of the sheet names a file not in UTF-8"
            ))
        })?;
        named.push((number, name));
    }
    Ok(named)
}

/// One token of a line as chdman's `cdrom_file::tokenize` takes it, from
/// `at`; and where it stopped.
fn chdman_token(line: &[u8], mut at: usize) -> (Vec<u8>, usize) {
    let space = |byte: u8| matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r');
    while at < line.len() && space(line[at]) {
        at += 1;
    }
    let (mut single, mut double) = (false, false);
    let mut token = Vec::new();
    while at < line.len() && line[at] != 0 {
        let byte = line[at];
        if !single && byte == b'"' {
            double = !double;
        } else if !double && byte == b'\'' {
            single = !single;
        } else if !single && !double && space(byte) {
            break;
        } else {
            token.push(byte);
        }
        at += 1;
    }
    (token, at)
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
            for pair in buf_b[..want].as_chunks_mut::<2>().0 {
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
    /// Whether it exited, rather than being ended by a signal.
    exited: bool,
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
            exited: status.code().is_some(),
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

    fn named(sheet: &[u8], toc: bool) -> Vec<String> {
        chdman_named_files(sheet, toc)
            .unwrap()
            .into_iter()
            .map(|(_, name)| name)
            .collect()
    }

    #[test]
    fn a_sheet_is_read_for_its_files_as_chdman_reads_it() {
        // Either quote, and none; white space as C's isspace; the command
        // compared exactly; nothing past a NUL.
        assert_eq!(
            named(
                b"FILE \"a b.bin\" BINARY\n  FILE c.bin MOTOROLA\r\n\t'FILE' '../d.bin' BINARY\n",
                false
            ),
            ["a b.bin", "c.bin", "../d.bin"]
        );
        assert!(named(b"file \"x.bin\" BINARY\nREM FILE \"y.bin\"\n", false).is_empty());
        assert_eq!(
            named(b"FILE \"ok.bin\0../x.bin\" BINARY\n", false),
            ["ok.bin"]
        );
        // A TOC's three ways to name one; a CUE has only FILE.
        let toc = b"CD_ROM\nTRACK AUDIO\nAUDIOFILE \"a.bin\" 0\nTRACK MODE1\nDATAFILE \"d.bin\"\nFILE \"f.bin\" 0\n";
        assert_eq!(named(toc, true), ["a.bin", "d.bin", "f.bin"]);
        assert_eq!(named(toc, false), ["f.bin"]);
    }

    #[test]
    fn a_line_chdman_might_read_as_two_is_refused() {
        // Found by the 2026-11-12 security review: chdman reads 510 bytes of
        // a line at a time, so the rest of a long comment is a line of its
        // own, and can be a FILE line.
        let mut sheet = b"FILE \"disc.bin\" BINARY\nREM ".to_vec();
        sheet.extend(std::iter::repeat_n(b'x', 506));
        sheet.extend_from_slice(b"FILE \"../../etc/passwd\" BINARY\n");
        let error = chdman_named_files(&sheet, false).unwrap_err();
        assert!(error.to_string().contains("line 2"), "{error}");
        let ok = [b"REM ".as_slice(), &[b'x'; MAX_SHEET_LINE_BYTES - 4]].concat();
        assert!(chdman_named_files(&ok, false).is_ok());
    }

    fn staged(dir: &Path, files: &[(&str, &[u8])]) {
        for (name, bytes) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
    }

    #[tokio::test]
    async fn chdman_gets_a_cue_naming_only_the_staged_files() {
        // Tangible reads `../../x/disc.bin` as the staged `disc.bin`; chdman
        // would have followed it out. It is given a sheet that names the
        // staged file, and the staged original is left as it was.
        let dir = tempfile::TempDir::new().unwrap();
        let original = b"REM from elsewhere\nFILE \"../../x/disc.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n";
        staged(
            dir.path(),
            &[("Game/disc.cue", original), ("Game/disc.bin", &[0; 2352])],
        );
        let input = LogicalPath::parse("Game/disc.cue").unwrap();

        let sheet = sheet_for_chdman(dir.path(), &input).await.unwrap();
        assert_eq!(sheet, dir.path().join("tangible-chdman.cue"));
        let given = std::fs::read(&sheet).unwrap();
        assert_eq!(
            given,
            b"REM from elsewhere\nFILE \"Game/disc.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n"
        );
        assert_eq!(
            std::fs::read(dir.path().join("Game/disc.cue")).unwrap(),
            original
        );

        // A staged file of that name is not overwritten.
        let dir = tempfile::TempDir::new().unwrap();
        staged(
            dir.path(),
            &[
                ("disc.cue", b"FILE \"tangible-chdman.cue\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n"),
                ("tangible-chdman.cue", &[0; 2352]),
            ],
        );
        let sheet = sheet_for_chdman(dir.path(), &LogicalPath::parse("disc.cue").unwrap())
            .await
            .unwrap();
        assert_eq!(sheet, dir.path().join("tangible-chdman-1.cue"));
    }

    #[tokio::test]
    async fn a_sheet_chdman_would_read_outside_the_staged_files_is_refused() {
        let refused = |result: Result<PathBuf, DeriveError>, why: &str| match result {
            Err(DeriveError::Failed {
                code: "DERIVATION_SHEET_REFUSED",
                detail,
                ..
            }) => detail.contains(why),
            _ => false,
        };
        let bin: &[u8] = &[0; 2352];

        // A FILE line in single quotes, which Tangible's reading does not
        // take for one and chdman does.
        let dir = tempfile::TempDir::new().unwrap();
        staged(
            dir.path(),
            &[
                ("disc.cue", b"FILE \"disc.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n'FILE' '../outside.bin' BINARY\n"),
                ("disc.bin", bin),
            ],
        );
        let input = LogicalPath::parse("disc.cue").unwrap();
        assert!(refused(
            sheet_for_chdman(dir.path(), &input).await,
            "line 4 of the sheet names \"../outside.bin\""
        ));

        // A comment long enough that chdman reads its end as a line.
        let mut cue =
            b"FILE \"disc.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\nREM "
                .to_vec();
        cue.extend(std::iter::repeat_n(b'x', 506));
        cue.extend_from_slice(b"FILE \"/etc/passwd\" BINARY\n");
        std::fs::write(dir.path().join("disc.cue"), &cue).unwrap();
        assert!(refused(
            sheet_for_chdman(dir.path(), &input).await,
            "line 4 of the sheet is longer"
        ));

        // A TOC, which is given as it is, must name only what is staged
        // beside it.
        let dir = tempfile::TempDir::new().unwrap();
        staged(
            dir.path(),
            &[
                (
                    "Game/disc.toc",
                    b"CD_ROM\nTRACK MODE1_RAW\nDATAFILE \"../outside.bin\"\n",
                ),
                ("outside.bin", bin),
            ],
        );
        let toc = LogicalPath::parse("Game/disc.toc").unwrap();
        assert!(refused(
            sheet_for_chdman(dir.path(), &toc).await,
            "names \"../outside.bin\""
        ));
        std::fs::write(
            dir.path().join("Game/disc.toc"),
            b"CD_ROM\nTRACK MODE1_RAW\nDATAFILE \"disc.bin\"\n",
        )
        .unwrap();
        staged(dir.path(), &[("Game/disc.bin", bin)]);
        assert_eq!(
            sheet_for_chdman(dir.path(), &toc).await.unwrap(),
            dir.path().join("Game/disc.toc")
        );
    }

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
            exited: true,
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

    /// A stand-in chdman: the banner with no arguments, otherwise `then`.
    #[cfg(unix)]
    async fn stand_in(dir: &Path, name: &str, then: &str) -> Chdman {
        use std::os::unix::fs::PermissionsExt as _;
        let script = dir.join(name);
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nif [ $# -eq 0 ]; then\n  echo 'chdman - MAME Compressed Hunks of Data \
                 (CHD) manager 0.251 (unknown)'\n  exit 1\nfi\n{then}\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A file just written can be busy to exec for a moment while another
        // test's child, forked meanwhile, still holds it.
        for _ in 0..20 {
            if let Ok(chdman) = Chdman::probe(&script).await {
                return chdman;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the stand-in did not run");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_verify_chdman_answered_is_final_and_one_a_signal_ended_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let chd = dir.path().join("x.chd");
        std::fs::write(&chd, b"not a chd").unwrap();

        let refuses = stand_in(
            dir.path(),
            "refuses",
            "echo 'Error: corrupt data' >&2; exit 1",
        )
        .await;
        let error = refuses.verify(&chd).await.expect_err("refused");
        assert_eq!(error.code(), "CHDMAN_FAILED");
        assert!(!error.is_retryable(), "{error}");
        assert!(error.to_string().contains("corrupt data"), "{error}");

        let killed = stand_in(dir.path(), "killed", "kill -KILL $$").await;
        let error = killed.verify(&chd).await.expect_err("ended");
        assert_eq!(error.code(), "CHDMAN_FAILED");
        assert!(
            error.is_retryable(),
            "a signal says nothing of the file: {error}"
        );
    }
}
