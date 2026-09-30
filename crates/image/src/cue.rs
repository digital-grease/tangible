// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! CUE sheets: parsing, reference resolution, and track layout.
//!
//! A CUE sheet is a text descriptor that names one or more data files and
//! describes the tracks inside them. It is the most common way a CD arrives,
//! and it is also the most hostile input this project accepts: it is text
//! somebody else wrote, it names files, and the names go on to be opened.
//!
//! Three separate steps, kept separate on purpose:
//!
//! 1. [`parse`] reads the text and reports what it says. It touches no
//!    filesystem and resolves nothing.
//! 2. [`resolve_references`] matches the files the sheet names against the
//!    files already staged. It cannot reach anything that was not staged,
//!    which is a stronger guarantee than checking a path for `..` and hoping.
//! 3. [`layout`] turns timecodes and file sizes into track extents. It is the
//!    only step that claims to know where a track begins, and it refuses
//!    rather than guesses.
//!
//! Warnings are the useful output. Real preservation dumps are odd in ways
//! that matter to somebody deciding whether to trust a disc, and flattening
//! that to a pass/fail verdict throws the interesting part away.

use std::collections::BTreeMap;
use std::fmt;

use tangible_domain::LogicalPath;
use tangible_domain::cd::{SampleByteOrder, TrackFlag};

/// Frames in one second of CD audio, and therefore sectors.
pub use tangible_domain::cd::{FRAMES_PER_SECOND, SECONDS_PER_MINUTE};

/// Largest descriptor this parser will read.
///
/// A CUE sheet for a 99-track disc is a few kilobytes. A megabyte is already
/// three orders of magnitude past anything legitimate, and the limit exists so
/// that a file merely *named* `.cue` cannot be walked line by line.
pub const MAX_CUE_BYTES: usize = 1 << 20;

/// Longest line the parser will consider.
const MAX_LINE_BYTES: usize = 4096;

/// Most lines the parser will read.
const MAX_LINES: usize = 20_000;

/// Most tracks a sheet may declare.
///
/// Red Book allows 99. A sheet claiming more is not describing a CD.
const MAX_TRACKS: usize = 99;

/// Most files a sheet may name.
///
/// One per track is already unusual; this is the same bound with room to be
/// wrong about that.
const MAX_FILES: usize = 100;

/// Most index points in one track. INDEX 00 through INDEX 99.
const MAX_INDEXES: usize = 100;

/// A timecode, in minutes, seconds and frames.
///
/// Stored exactly as written. A sheet with 75 frames in a second is wrong, and
/// this type keeps the wrong value so the warning can quote it rather than
/// reporting a number nobody typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Msf {
    /// Minutes.
    pub minutes: u32,
    /// Seconds.
    pub seconds: u32,
    /// Frames, of which there are 75 to the second.
    pub frames: u32,
}

impl Msf {
    /// The sector this timecode names, counting from zero.
    #[must_use]
    pub fn to_lba(self) -> u64 {
        let minutes = u64::from(self.minutes);
        let seconds = u64::from(self.seconds);
        let frames = u64::from(self.frames);
        minutes
            .saturating_mul(SECONDS_PER_MINUTE)
            .saturating_add(seconds)
            .saturating_mul(FRAMES_PER_SECOND)
            .saturating_add(frames)
    }

    /// Whether the fields are within the ranges a timecode allows.
    #[must_use]
    pub fn is_well_formed(self) -> bool {
        u64::from(self.seconds) < SECONDS_PER_MINUTE && u64::from(self.frames) < FRAMES_PER_SECOND
    }

    /// Read `mm:ss:ff`.
    fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split(':');
        let minutes = parse_number(parts.next()?)?;
        let seconds = parse_number(parts.next()?)?;
        let frames = parse_number(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            minutes,
            seconds,
            frames,
        })
    }
}

impl fmt::Display for Msf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02}:{:02}:{:02}",
            self.minutes, self.seconds, self.frames
        )
    }
}

/// A digit string, rejecting anything else including a sign.
fn parse_number(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// What a track holds and how big its sectors are in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackMode {
    /// The mode exactly as the sheet spelled it, upper-cased.
    ///
    /// Preserved rather than mapped onto an enum, because an unrecognised mode
    /// is information: it is what an operator needs to see to know why the
    /// track could not be laid out.
    text: String,
    /// Bytes per sector in the file, for modes whose size is known.
    sector_bytes: Option<u32>,
    /// Whether the track carries audio rather than data.
    audio: bool,
}

impl TrackMode {
    /// Recognise a mode string.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let text = text.to_ascii_uppercase();
        // The sizes are domain facts rather than parser facts, and they live
        // with the rest of them. Two tables that disagreed about what a
        // MODE2/2352 sector is would be two components describing different
        // discs.
        Self {
            sector_bytes: tangible_domain::cd::sector_bytes(&text),
            audio: tangible_domain::cd::is_audio(&text),
            text,
        }
    }

    /// The mode as the sheet wrote it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Bytes per sector, when the mode is one this parser knows.
    #[must_use]
    pub fn sector_bytes(&self) -> Option<u32> {
        self.sector_bytes
    }

    /// Whether the track is audio.
    #[must_use]
    pub fn is_audio(&self) -> bool {
        self.audio
    }
}

/// One index point in a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CueIndex {
    /// Index number. 0 is the pregap, 1 is the start of the track proper.
    pub number: u32,
    /// Where it is, as a timecode into the file.
    pub position: Msf,
}

/// One track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CueTrack {
    /// Track number as declared.
    pub number: u32,
    /// Session this track belongs to. One unless `REM SESSION` says otherwise.
    pub session: u32,
    /// Mode, e.g. `AUDIO` or `MODE2/2352`.
    pub mode: TrackMode,
    /// Index points, in the order the sheet gave them.
    pub indexes: Vec<CueIndex>,
    /// A gap the burner generates, which is *not* present in the file.
    pub pregap: Option<Msf>,
    /// A gap after the track, likewise generated.
    pub postgap: Option<Msf>,
    /// Flags such as `DCP` or `PRE`, upper-cased.
    pub flags: Vec<String>,
    /// International Standard Recording Code, as written.
    pub isrc: Option<String>,
}

impl CueTrack {
    /// The first index present, which is where the track's bytes begin.
    #[must_use]
    pub fn first_index(&self) -> Option<CueIndex> {
        self.indexes
            .iter()
            .min_by_key(|index| index.number)
            .copied()
    }

    /// INDEX 01, the start of the track proper.
    #[must_use]
    pub fn index_one(&self) -> Option<CueIndex> {
        self.indexes.iter().find(|index| index.number == 1).copied()
    }
}

/// One file named by the sheet, and the tracks inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CueFile {
    /// The name exactly as the sheet wrote it, quotes removed.
    ///
    /// Not a path. It has not been validated, resolved, or looked at by the
    /// filesystem, and it must not be handed to anything that would.
    pub name: String,
    /// The declared storage format, upper-cased, e.g. `BINARY`.
    pub format: String,
    /// Tracks in the order declared.
    pub tracks: Vec<CueTrack>,
}

impl CueFile {
    /// Whether the file holds raw sectors, which is what a layout needs.
    #[must_use]
    pub fn is_raw_sectors(&self) -> bool {
        matches!(self.format.as_str(), "BINARY" | "MOTOROLA")
    }

    /// How the file stores audio samples, when its format says.
    ///
    /// The format word is the only place a sheet records this, and nothing in
    /// the bytes does. `BINARY` is little-endian and `MOTOROLA` big-endian;
    /// anything else is not raw sectors and has no answer here.
    #[must_use]
    pub fn sample_byte_order(&self) -> Option<SampleByteOrder> {
        match self.format.as_str() {
            "BINARY" => Some(SampleByteOrder::LittleEndian),
            "MOTOROLA" => Some(SampleByteOrder::BigEndian),
            _ => None,
        }
    }
}

/// A parsed sheet.
#[derive(Debug, Clone, PartialEq)]
pub struct CueSheet {
    /// Media catalogue number, as written.
    pub catalog: Option<String>,
    /// Files in the order declared.
    pub files: Vec<CueFile>,
    /// Structural observations that do not stop the sheet being read.
    pub warnings: Vec<CueWarning>,
    /// Why the parser concluded what it did.
    pub evidence: Vec<String>,
    /// Confidence from 0 to 1 that this is a CUE sheet. Diagnostic only.
    pub confidence: f32,
}

impl CueSheet {
    /// Every track, in declared order, paired with the file it lives in.
    pub fn tracks(&self) -> impl Iterator<Item = (usize, &CueTrack)> {
        self.files
            .iter()
            .enumerate()
            .flat_map(|(index, file)| file.tracks.iter().map(move |track| (index, track)))
    }

    /// How many tracks the sheet declares.
    #[must_use]
    pub fn track_count(&self) -> usize {
        self.files.iter().map(|file| file.tracks.len()).sum()
    }
}

/// Something odd about a sheet that does not stop it being read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CueWarning {
    /// The text was not valid UTF-8 and was decoded lossily.
    NonUtf8Text,
    /// A command this parser does not know, preserved but not acted on.
    UnknownCommand {
        /// Line number, one-based.
        line: usize,
        /// The command word.
        command: String,
    },
    /// A file name used backslashes, which were read as separators.
    BackslashSeparators {
        /// The name as written.
        name: String,
    },
    /// The first track is not track 1.
    FirstTrackIsNotOne {
        /// What it was.
        found: u32,
    },
    /// Track numbers skip or repeat.
    TrackNumbersNotSequential {
        /// What was expected next.
        expected: u32,
        /// What was found.
        found: u32,
    },
    /// A track has no INDEX 01, so nothing says where it starts.
    MissingIndexOne {
        /// Track number.
        track: u32,
    },
    /// A timecode names more than 59 seconds or 74 frames.
    ImpossibleTimecode {
        /// Track number.
        track: u32,
        /// The timecode as written.
        timecode: String,
    },
    /// Index points are not in increasing order.
    IndexesOutOfOrder {
        /// Track number.
        track: u32,
    },
    /// The same index number appears twice in one track.
    DuplicateIndex {
        /// Track number.
        track: u32,
        /// Index number.
        index: u32,
    },
    /// A mode this parser does not know the sector size for.
    UnknownTrackMode {
        /// Track number.
        track: u32,
        /// The mode as written.
        mode: String,
    },
    /// A file that declares no tracks.
    FileWithoutTracks {
        /// The name as written.
        name: String,
    },
    /// A file stored as something other than raw sectors.
    NotRawSectors {
        /// The name as written.
        name: String,
        /// The declared format.
        format: String,
    },
    /// Both a PREGAP command and an INDEX 00 for one track.
    ///
    /// They mean different things: one gap is generated by the burner, the
    /// other is in the file. A track claiming both is describing two gaps.
    PregapAndIndexZero {
        /// Track number.
        track: u32,
    },
    /// A catalogue number that is not thirteen digits.
    MalformedCatalog {
        /// What was written.
        value: String,
    },
    /// An ISRC that is not one.
    MalformedIsrc {
        /// Track number.
        track: u32,
        /// What was written.
        value: String,
    },
    /// A flag outside the four a track can carry.
    ///
    /// Left out of the layout rather than refused: some sheets write `DATA`
    /// for a data track, which the mode already says, and a disc is not worth
    /// refusing over a word that changes nothing.
    UnknownFlag {
        /// Track number.
        track: u32,
        /// What was written.
        flag: String,
    },
    /// A file's length is not a whole number of sectors.
    ///
    /// The usual cause is a truncated dump, and the last track is short by
    /// exactly what is missing.
    FileNotSectorAligned {
        /// The name as written.
        name: String,
        /// Length in bytes.
        bytes: u64,
        /// Bytes per sector for the track being laid out.
        sector_bytes: u64,
    },
    /// A file was matched to a staged name that differs in case.
    CaseInsensitiveMatch {
        /// The name as written.
        declared: String,
        /// The staged file it was matched to.
        matched: String,
    },
    /// A file was matched by its last path segment rather than in full.
    MatchedByFileName {
        /// The name as written.
        declared: String,
        /// The staged file it was matched to.
        matched: String,
    },
}

impl fmt::Display for CueWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonUtf8Text => write!(f, "the sheet is not valid UTF-8 and was decoded lossily"),
            Self::UnknownCommand { line, command } => {
                write!(f, "line {line}: unknown command {command}")
            }
            Self::BackslashSeparators { name } => {
                write!(f, "{name} uses backslashes, read as path separators")
            }
            Self::FirstTrackIsNotOne { found } => write!(f, "the first track is {found}, not 1"),
            Self::TrackNumbersNotSequential { expected, found } => {
                write!(
                    f,
                    "track {found} follows where track {expected} was expected"
                )
            }
            Self::MissingIndexOne { track } => write!(f, "track {track} has no INDEX 01"),
            Self::ImpossibleTimecode { track, timecode } => {
                write!(f, "track {track} has the impossible timecode {timecode}")
            }
            Self::IndexesOutOfOrder { track } => {
                write!(f, "track {track} lists its indexes out of order")
            }
            Self::DuplicateIndex { track, index } => {
                write!(f, "track {track} declares INDEX {index:02} twice")
            }
            Self::UnknownTrackMode { track, mode } => {
                write!(f, "track {track} has the unrecognised mode {mode}")
            }
            Self::FileWithoutTracks { name } => write!(f, "{name} declares no tracks"),
            Self::NotRawSectors { name, format } => {
                write!(f, "{name} is stored as {format} rather than raw sectors")
            }
            Self::PregapAndIndexZero { track } => {
                write!(f, "track {track} has both a PREGAP and an INDEX 00")
            }
            Self::MalformedCatalog { value } => {
                write!(f, "the catalogue number {value} is not thirteen digits")
            }
            Self::MalformedIsrc { track, value } => {
                write!(f, "track {track} carries {value}, which is not an ISRC")
            }
            Self::UnknownFlag { track, flag } => {
                write!(f, "track {track} carries the unrecognised flag {flag}")
            }
            Self::FileNotSectorAligned {
                name,
                bytes,
                sector_bytes,
            } => write!(
                f,
                "{name} is {bytes} bytes, not a whole number of {sector_bytes} byte sectors"
            ),
            Self::CaseInsensitiveMatch { declared, matched } => {
                write!(
                    f,
                    "{declared} was matched to {matched}, which differs in case"
                )
            }
            Self::MatchedByFileName { declared, matched } => {
                write!(f, "{declared} was matched to {matched} by file name alone")
            }
        }
    }
}

/// Why a sheet could not be read at all.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CueError {
    /// Larger than [`MAX_CUE_BYTES`].
    #[error("the sheet is {bytes} bytes, larger than the {MAX_CUE_BYTES} byte limit")]
    TooLarge {
        /// Size seen.
        bytes: usize,
    },

    /// More lines than [`MAX_LINES`].
    #[error("the sheet has more than {MAX_LINES} lines")]
    TooManyLines,

    /// A line longer than [`MAX_LINE_BYTES`].
    #[error("line {line} is longer than {MAX_LINE_BYTES} bytes")]
    LineTooLong {
        /// Line number, one-based.
        line: usize,
    },

    /// Nothing but blank lines and comments.
    #[error("the sheet declares nothing")]
    Empty,

    /// No FILE command, so nothing is being described.
    #[error("the sheet names no file")]
    NoFile,

    /// A TRACK outside any FILE.
    #[error("line {line}: TRACK before any FILE")]
    TrackBeforeFile {
        /// Line number, one-based.
        line: usize,
    },

    /// An INDEX, FLAGS or similar outside any TRACK.
    #[error("line {line}: {command} before any TRACK")]
    CommandBeforeTrack {
        /// Line number, one-based.
        line: usize,
        /// The command word.
        command: String,
    },

    /// A command whose arguments could not be read.
    #[error("line {line}: malformed {command}")]
    Malformed {
        /// Line number, one-based.
        line: usize,
        /// The command word.
        command: String,
    },

    /// More tracks than a CD can hold.
    #[error("the sheet declares {count} tracks; a CD holds at most {MAX_TRACKS}")]
    TooManyTracks {
        /// How many were declared.
        count: usize,
    },

    /// More files than the parser will accept.
    #[error("the sheet names {count} files, more than the {MAX_FILES} limit")]
    TooManyFiles {
        /// How many were named.
        count: usize,
    },

    /// More index points in one track than can exist.
    #[error("track {track} declares more than {MAX_INDEXES} indexes")]
    TooManyIndexes {
        /// Track number.
        track: u32,
    },

    /// The same track number twice.
    #[error("track {number} is declared twice")]
    DuplicateTrack {
        /// Track number.
        number: u32,
    },
}

/// Split a line into words, keeping quoted strings whole.
///
/// CUE quoting has no escape character: a quote opens or closes a run, and
/// that is the whole grammar. An unterminated quote therefore swallows the
/// rest of the line, which produces a file name that will not resolve rather
/// than a parse that silently reads the next command as data.
fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut started = false;

    for character in line.chars() {
        match character {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            _ if character.is_whitespace() && !quoted => {
                if started {
                    tokens.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            _ => {
                current.push(character);
                started = true;
            }
        }
    }
    if started {
        tokens.push(current);
    }
    tokens
}

/// The track a track-scoped command applies to.
fn current_track(files: &mut [CueFile]) -> Option<&mut CueTrack> {
    files.last_mut()?.tracks.last_mut()
}

/// Read a CUE sheet.
///
/// The bytes are the descriptor exactly as staged. Nothing here touches the
/// filesystem: the file names the sheet declares come back as the text they
/// were written as, and turning one into something openable is
/// [`resolve_references`]'s job and nobody else's.
///
/// # Errors
///
/// [`CueError`] when the text cannot be read as a sheet at all: too large,
/// too many lines, no FILE, a command whose arguments are unreadable, or
/// counts beyond what a CD can hold. Everything survivable is a warning on
/// the returned sheet.
#[allow(clippy::too_many_lines)]
pub fn parse(bytes: &[u8]) -> Result<CueSheet, CueError> {
    if bytes.len() > MAX_CUE_BYTES {
        return Err(CueError::TooLarge { bytes: bytes.len() });
    }

    let mut warnings = Vec::new();
    let text = if let Ok(text) = std::str::from_utf8(bytes) {
        std::borrow::Cow::Borrowed(text)
    } else {
        // Latin-1 track titles are common in sheets written by older Windows
        // tools. Decoding lossily keeps the structure readable; a name that
        // lost bytes will simply fail to resolve, which is a clear failure
        // rather than a wrong file.
        warnings.push(CueWarning::NonUtf8Text);
        String::from_utf8_lossy(bytes)
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);

    let mut files: Vec<CueFile> = Vec::new();
    let mut catalog: Option<String> = None;
    let mut session = 1_u32;
    let mut expected_track = 1_u32;
    let mut seen_tracks: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    let mut saw_command = false;

    for (offset, raw) in text.lines().enumerate() {
        let line = offset + 1;
        if line > MAX_LINES {
            return Err(CueError::TooManyLines);
        }
        if raw.len() > MAX_LINE_BYTES {
            return Err(CueError::LineTooLong { line });
        }

        let tokens = tokenize(raw.trim());
        let Some(first) = tokens.first() else {
            continue;
        };
        let command = first.to_ascii_uppercase();

        match command.as_str() {
            // Comments, except for the one convention that carries meaning:
            // multisession discs are marked with REM SESSION in practice, and
            // there is nowhere else in the grammar to say it.
            "REM" => {
                if let (Some(keyword), Some(value)) = (tokens.get(1), tokens.get(2))
                    && keyword.eq_ignore_ascii_case("SESSION")
                    && let Some(number) = parse_number(value)
                {
                    session = number.max(1);
                }
            }

            "CATALOG" => {
                saw_command = true;
                let Some(value) = tokens.get(1) else {
                    return Err(CueError::Malformed { line, command });
                };
                if value.len() != 13 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    warnings.push(CueWarning::MalformedCatalog {
                        value: value.clone(),
                    });
                }
                catalog = Some(value.clone());
            }

            "FILE" => {
                saw_command = true;
                let Some(name) = tokens.get(1) else {
                    return Err(CueError::Malformed { line, command });
                };
                if files.len() >= MAX_FILES {
                    return Err(CueError::TooManyFiles {
                        count: files.len() + 1,
                    });
                }
                files.push(CueFile {
                    name: name.clone(),
                    // BINARY when unstated. Universal in practice, and the
                    // formats that are not raw sectors are warned about below
                    // rather than assumed away.
                    format: tokens
                        .get(2)
                        .map_or_else(|| "BINARY".to_owned(), |text| text.to_ascii_uppercase()),
                    tracks: Vec::new(),
                });
            }

            "TRACK" => {
                saw_command = true;
                let (Some(number_text), Some(mode_text)) = (tokens.get(1), tokens.get(2)) else {
                    return Err(CueError::Malformed { line, command });
                };
                let Some(number) = parse_number(number_text) else {
                    return Err(CueError::Malformed { line, command });
                };
                let Some(file) = files.last_mut() else {
                    return Err(CueError::TrackBeforeFile { line });
                };

                if !seen_tracks.insert(number) {
                    return Err(CueError::DuplicateTrack { number });
                }
                if seen_tracks.len() > MAX_TRACKS {
                    return Err(CueError::TooManyTracks {
                        count: seen_tracks.len(),
                    });
                }
                if seen_tracks.len() == 1 && number != 1 {
                    warnings.push(CueWarning::FirstTrackIsNotOne { found: number });
                } else if number != expected_track {
                    warnings.push(CueWarning::TrackNumbersNotSequential {
                        expected: expected_track,
                        found: number,
                    });
                }
                expected_track = number.saturating_add(1);

                let mode = TrackMode::parse(mode_text);
                if mode.sector_bytes().is_none() {
                    warnings.push(CueWarning::UnknownTrackMode {
                        track: number,
                        mode: mode.as_str().to_owned(),
                    });
                }
                file.tracks.push(CueTrack {
                    number,
                    session,
                    mode,
                    indexes: Vec::new(),
                    pregap: None,
                    postgap: None,
                    flags: Vec::new(),
                    isrc: None,
                });
            }

            "INDEX" => {
                saw_command = true;
                let (Some(number_text), Some(time_text)) = (tokens.get(1), tokens.get(2)) else {
                    return Err(CueError::Malformed { line, command });
                };
                let (Some(number), Some(position)) =
                    (parse_number(number_text), Msf::parse(time_text))
                else {
                    return Err(CueError::Malformed { line, command });
                };
                let Some(track) = current_track(&mut files) else {
                    return Err(CueError::CommandBeforeTrack { line, command });
                };

                if track.indexes.len() >= MAX_INDEXES {
                    return Err(CueError::TooManyIndexes {
                        track: track.number,
                    });
                }
                if !position.is_well_formed() {
                    warnings.push(CueWarning::ImpossibleTimecode {
                        track: track.number,
                        timecode: position.to_string(),
                    });
                }
                if track.indexes.iter().any(|index| index.number == number) {
                    warnings.push(CueWarning::DuplicateIndex {
                        track: track.number,
                        index: number,
                    });
                } else if track
                    .indexes
                    .last()
                    .is_some_and(|last| number < last.number || position < last.position)
                {
                    warnings.push(CueWarning::IndexesOutOfOrder {
                        track: track.number,
                    });
                }
                track.indexes.push(CueIndex { number, position });
            }

            "PREGAP" | "POSTGAP" => {
                saw_command = true;
                let Some(position) = tokens.get(1).and_then(|text| Msf::parse(text)) else {
                    return Err(CueError::Malformed { line, command });
                };
                let Some(track) = current_track(&mut files) else {
                    return Err(CueError::CommandBeforeTrack { line, command });
                };
                if !position.is_well_formed() {
                    warnings.push(CueWarning::ImpossibleTimecode {
                        track: track.number,
                        timecode: position.to_string(),
                    });
                }
                if command == "PREGAP" {
                    track.pregap = Some(position);
                } else {
                    track.postgap = Some(position);
                }
            }

            "FLAGS" => {
                saw_command = true;
                let flags: Vec<String> = tokens
                    .iter()
                    .skip(1)
                    .map(|flag| flag.to_ascii_uppercase())
                    .collect();
                let Some(track) = current_track(&mut files) else {
                    return Err(CueError::CommandBeforeTrack { line, command });
                };
                for flag in &flags {
                    if TrackFlag::parse(flag).is_none() {
                        warnings.push(CueWarning::UnknownFlag {
                            track: track.number,
                            flag: flag.clone(),
                        });
                    }
                }
                track.flags = flags;
            }

            "ISRC" => {
                saw_command = true;
                let Some(value) = tokens.get(1).cloned() else {
                    return Err(CueError::Malformed { line, command });
                };
                let Some(track) = current_track(&mut files) else {
                    return Err(CueError::CommandBeforeTrack { line, command });
                };
                if !tangible_domain::cd::is_isrc(&value) {
                    warnings.push(CueWarning::MalformedIsrc {
                        track: track.number,
                        value: value.clone(),
                    });
                }
                track.isrc = Some(value);
            }

            // Known and deliberately unused. CD-TEXT has nowhere to go in the
            // v1alpha1 manifest, and inventing a place for it here would be
            // guessing at a schema rather than reading a sheet.
            "TITLE" | "PERFORMER" | "SONGWRITER" | "CDTEXTFILE" => saw_command = true,

            _ => warnings.push(CueWarning::UnknownCommand {
                line,
                command: command.clone(),
            }),
        }
    }

    if files.is_empty() {
        return Err(if saw_command {
            CueError::NoFile
        } else {
            CueError::Empty
        });
    }

    for file in &files {
        if file.tracks.is_empty() {
            warnings.push(CueWarning::FileWithoutTracks {
                name: file.name.clone(),
            });
        }
        if !file.is_raw_sectors() {
            warnings.push(CueWarning::NotRawSectors {
                name: file.name.clone(),
                format: file.format.clone(),
            });
        }
        for track in &file.tracks {
            if track.index_one().is_none() {
                warnings.push(CueWarning::MissingIndexOne {
                    track: track.number,
                });
            }
            if track.pregap.is_some() && track.indexes.iter().any(|index| index.number == 0) {
                warnings.push(CueWarning::PregapAndIndexZero {
                    track: track.number,
                });
            }
        }
    }

    let track_count = files.iter().map(|file| file.tracks.len()).sum::<usize>();
    let complete = track_count > 0
        && files.iter().all(|file| {
            file.is_raw_sectors()
                && file
                    .tracks
                    .iter()
                    .all(|track| track.index_one().is_some() && track.mode.sector_bytes().is_some())
        });

    let mut evidence = vec![format!(
        "{} file reference(s), {track_count} track(s)",
        files.len()
    )];
    let mut modes: BTreeMap<&str, usize> = BTreeMap::new();
    for file in &files {
        for track in &file.tracks {
            *modes.entry(track.mode.as_str()).or_insert(0) += 1;
        }
    }
    if !modes.is_empty() {
        let described: Vec<String> = modes
            .iter()
            .map(|(mode, count)| format!("{mode} x{count}"))
            .collect();
        evidence.push(format!("modes: {}", described.join(", ")));
    }
    if catalog.is_some() {
        evidence.push("a catalogue number is declared".to_owned());
    }

    Ok(CueSheet {
        catalog,
        files,
        warnings,
        evidence,
        // Structure only. Whether the files it names actually exist is a
        // separate question, and the caller that answers it is the one that
        // may raise this.
        confidence: if complete {
            0.95
        } else if track_count > 0 {
            0.8
        } else {
            0.6
        },
    })
}

// --- reference resolution ------------------------------------------------------

/// Why a file the sheet names could not be turned into a staged file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReferenceFailure {
    /// The name is not something this project will ever open.
    #[error("{reason}")]
    Unsafe {
        /// What is wrong with it.
        reason: String,
    },

    /// Nothing staged matches it.
    #[error("no staged file matches")]
    NotStaged,

    /// Several staged files match equally well.
    #[error("several staged files match: {}", candidates.join(", "))]
    Ambiguous {
        /// The staged files that matched.
        candidates: Vec<String>,
    },

    /// An earlier reference already claimed the file this one matched.
    #[error("already used by {by}")]
    AlreadyClaimed {
        /// The name that claimed it first.
        by: String,
    },
}

/// One file the sheet names, and what it resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedReference {
    /// The name exactly as the sheet wrote it.
    pub declared: String,
    /// The staged file it names, when one could be found safely.
    pub resolved: Option<LogicalPath>,
    /// Why not, when it could not.
    pub failure: Option<ReferenceFailure>,
}

/// What resolution concluded, one entry per FILE in declaration order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceResolution {
    /// One per FILE, in the order the sheet declared them.
    pub files: Vec<ResolvedReference>,
    /// Observations about how the matching went.
    pub warnings: Vec<CueWarning>,
}

impl ReferenceResolution {
    /// Whether every reference found a staged file.
    #[must_use]
    pub fn all_resolved(&self) -> bool {
        self.files.iter().all(|file| file.resolved.is_some())
    }

    /// The staged files, in declaration order, if every one resolved.
    #[must_use]
    pub fn paths(&self) -> Option<Vec<LogicalPath>> {
        self.files
            .iter()
            .map(|file| file.resolved.clone())
            .collect()
    }
}

/// Match the files a sheet names against the files already staged.
///
/// This is the trust boundary, and it is drawn so that crossing it is not
/// possible rather than merely difficult: the only paths this can return are
/// paths that are already in `staged`. A name that traverses, that is
/// absolute, that carries a drive letter or that simply is not there resolves
/// to nothing. There is no branch in which the sheet's text reaches the
/// filesystem, because the sheet's text is never what gets opened.
///
/// Matching is a ladder, and each rung below the first is warned about:
///
/// 1. the name, exactly, after backslashes are read as separators;
/// 2. the last segment of the name, matching one staged file;
/// 3. the last segment ignoring case, matching one staged file.
///
/// Rungs two and three exist because real sheets are written on one machine
/// and read on another. A sheet that says `D:\dump\TRACK01.BIN` beside a
/// staged `track01.bin` is describing that file, and refusing it would be
/// pedantry at the operator's expense. Ambiguity is refused rather than
/// guessed.
#[must_use]
pub fn resolve_references(sheet: &CueSheet, staged: &[LogicalPath]) -> ReferenceResolution {
    let mut warnings = Vec::new();
    let mut files = Vec::with_capacity(sheet.files.len());
    let mut claimed: BTreeMap<String, String> = BTreeMap::new();

    for file in &sheet.files {
        let declared = file.name.clone();
        let mut push = |resolved, failure| {
            files.push(ResolvedReference {
                declared: declared.clone(),
                resolved,
                failure,
            });
        };

        let Some(candidate) = normalise_reference(&file.name, &mut warnings) else {
            push(
                None,
                Some(ReferenceFailure::Unsafe {
                    reason: "the name is not one this project will look for".to_owned(),
                }),
            );
            continue;
        };

        let matched = match match_staged(&candidate.path, staged) {
            Ok(matched) => matched,
            Err(failure) => {
                push(None, Some(failure));
                continue;
            }
        };

        if let Some(previous) = claimed.get(matched.as_str()) {
            // Two references to one file would give two tracks the same bytes
            // at incompatible offsets. Whichever is wrong, the sheet cannot be
            // laid out as written.
            push(
                None,
                Some(ReferenceFailure::AlreadyClaimed {
                    by: previous.clone(),
                }),
            );
            continue;
        }
        claimed.insert(matched.as_str().to_owned(), file.name.clone());

        if candidate.reduced || matched.as_str() != candidate.path {
            let wanted = last_segment(&candidate.path);
            if !candidate.reduced
                && matched.file_name() != wanted
                && matched.file_name().eq_ignore_ascii_case(wanted)
            {
                warnings.push(CueWarning::CaseInsensitiveMatch {
                    declared: file.name.clone(),
                    matched: matched.as_str().to_owned(),
                });
            } else {
                warnings.push(CueWarning::MatchedByFileName {
                    declared: file.name.clone(),
                    matched: matched.as_str().to_owned(),
                });
            }
        }
        push(Some(matched), None);
    }

    ReferenceResolution { files, warnings }
}

/// The part of a reference after the last separator.
fn last_segment(candidate: &str) -> &str {
    candidate.rsplit('/').next().unwrap_or(candidate)
}

/// A declared name, reduced to something that can be looked for.
struct Normalised {
    /// The relative path to look for.
    path: String,
    /// Whether everything but the last segment had to be thrown away.
    reduced: bool,
}

/// Turn a declared name into something safe to look for, or reject it.
///
/// `LogicalPath` refuses to translate backslashes, for the good reason that
/// `a\b` is one filename on Unix and two segments on Windows and guessing
/// changes what gets *written*. Nothing is written here. The output of this
/// function is only ever used to pick one of the files already staged, so the
/// guess costs nothing and reads the sheets that need it.
///
/// A name that is absolute, drive-lettered or full of traversal is not refused
/// outright either. It is reduced to its last segment, which is the only part
/// of `D:\dumps\old\TRACK01.BIN` that could ever have meant a staged file
/// anyway. That is safe for the same structural reason: a reduced name still
/// has to match something in `staged` or it resolves to nothing.
fn normalise_reference(name: &str, warnings: &mut Vec<CueWarning>) -> Option<Normalised> {
    if name.contains('\0') {
        return None;
    }

    let candidate = if name.contains('\\') {
        warnings.push(CueWarning::BackslashSeparators {
            name: name.to_owned(),
        });
        name.replace('\\', "/")
    } else {
        name.to_owned()
    };

    if LogicalPath::parse(&candidate).is_ok() {
        return Some(Normalised {
            path: candidate,
            reduced: false,
        });
    }

    let segment = last_segment(&candidate).to_owned();
    if segment != candidate && LogicalPath::parse(&segment).is_ok() {
        return Some(Normalised {
            path: segment,
            reduced: true,
        });
    }
    None
}

/// Find the one staged file a reference names.
fn match_staged(candidate: &str, staged: &[LogicalPath]) -> Result<LogicalPath, ReferenceFailure> {
    if let Some(exact) = staged.iter().find(|path| path.as_str() == candidate) {
        return Ok(exact.clone());
    }

    let wanted = last_segment(candidate);
    let by_name: Vec<&LogicalPath> = staged
        .iter()
        .filter(|path| path.file_name() == wanted)
        .collect();
    if let [only] = by_name.as_slice() {
        return Ok((*only).clone());
    }
    if by_name.len() > 1 {
        return Err(ReferenceFailure::Ambiguous {
            candidates: by_name
                .iter()
                .map(|path| path.as_str().to_owned())
                .collect(),
        });
    }

    let insensitive: Vec<&LogicalPath> = staged
        .iter()
        .filter(|path| path.file_name().eq_ignore_ascii_case(wanted))
        .collect();
    match insensitive.as_slice() {
        [only] => Ok((*only).clone()),
        [] => Err(ReferenceFailure::NotStaged),
        _ => Err(ReferenceFailure::Ambiguous {
            candidates: insensitive
                .iter()
                .map(|path| path.as_str().to_owned())
                .collect(),
        }),
    }
}

// --- track layout --------------------------------------------------------------

/// Why a sheet could not be laid out.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    /// No tracks to lay out.
    #[error("the sheet declares no tracks")]
    NoTracks,

    /// A size was not supplied for every file.
    #[error("sizes were given for {given} files but the sheet names {declared}")]
    SizeCountMismatch {
        /// How many sizes were supplied.
        given: usize,
        /// How many files the sheet names.
        declared: usize,
    },

    /// A file that is not raw sectors, where offsets in sectors mean nothing.
    #[error("{name} is stored as {format} rather than raw sectors")]
    NotRawSectors {
        /// The name as written.
        name: String,
        /// The declared format.
        format: String,
    },

    /// A mode whose sector size this parser does not know.
    #[error("track {track} has the unrecognised mode {mode}, so its sector size is unknown")]
    UnknownSectorSize {
        /// Track number.
        track: u32,
        /// The mode as written.
        mode: String,
    },

    /// Nothing says where the track starts.
    #[error("track {track} has no INDEX 01")]
    MissingIndexOne {
        /// Track number.
        track: u32,
    },

    /// A timecode that cannot be converted to a position.
    #[error("track {track} has an impossible timecode")]
    ImpossibleTimecode {
        /// Track number.
        track: u32,
    },

    /// Tracks that do not advance through their file.
    #[error("track {track} does not start after the track before it")]
    TracksOutOfOrder {
        /// Track number.
        track: u32,
    },

    /// A track that starts where the file has no sectors.
    #[error("track {track} starts at sector {start}, outside the {sectors} sectors of {name}")]
    StartsOutsideFile {
        /// Track number.
        track: u32,
        /// Where it claims to start.
        start: u64,
        /// How many sectors the file holds.
        sectors: u64,
        /// The file name as written.
        name: String,
    },
}

/// Where one track's bytes are, and where the track sits on the disc.
///
/// Three quantities that are easy to conflate and are not the same:
///
/// * `start_lba` is the disc position of the first sector of this track that
///   is present in the file.
/// * `sector_count` is how many sectors of it the file holds.
/// * `pregap_sectors` is the whole gap before INDEX 01, whether the burner
///   generates it (a PREGAP command) or the file carries it (an INDEX 00).
///   The in-file part of that gap is inside `sector_count`; the generated part
///   is not, and precedes `start_lba`.
///
/// Positions are relative to the start of the image. A pressed disc puts
/// track one's INDEX 01 at MSF 00:02:00 rather than at zero, and that 150
/// sector lead-in is deliberately *not* folded in here: it belongs to the
/// engine that writes the disc, and adding it in two places is how an image
/// comes out shifted by two seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackLayout {
    /// Track number as declared.
    pub number: u32,
    /// Session this track belongs to.
    pub session: u32,
    /// Mode as the sheet wrote it.
    pub mode: String,
    /// Which file, by index into the sheet's files.
    pub file: usize,
    /// Byte offset of the track's first present sector within that file.
    pub file_offset_bytes: u64,
    /// Image-relative LBA of that same sector.
    pub start_lba: u64,
    /// Sectors of this track present in the file.
    pub sector_count: u64,
    /// Total gap before INDEX 01, generated plus in-file.
    pub pregap_sectors: u64,
    /// Index points, as LBAs relative to the track's first present sector.
    pub indexes: Vec<(u32, u64)>,
    /// International Standard Recording Code, when the track declared a
    /// well-formed one.
    pub isrc: Option<String>,
    /// How the track's file stores audio samples, from its declared format.
    pub sample_byte_order: Option<SampleByteOrder>,
    /// The flags the track declared that are flags, in the order given and
    /// without repeats. Anything else was warned about by the parser.
    pub flags: Vec<TrackFlag>,
}

/// A whole disc, laid out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdLayout {
    /// Media catalogue number, when the sheet declared a well-formed one.
    ///
    /// Filtered here rather than passed through, so that everything
    /// downstream can trust the shape of what it is given. Nothing is lost by
    /// dropping a malformed one: the sheet itself is preserved as a component,
    /// exactly as received, and the parser warned about it on the way past.
    pub catalog: Option<String>,
    /// Tracks in disc order.
    pub tracks: Vec<TrackLayout>,
    /// How many sessions the tracks span.
    pub session_count: u32,
    /// Observations about the layout that do not invalidate it.
    pub warnings: Vec<CueWarning>,
}

impl CdLayout {
    /// Total sectors the image accounts for.
    #[must_use]
    pub fn total_sectors(&self) -> u64 {
        self.tracks
            .iter()
            .map(|track| track.sector_count)
            .fold(0, u64::saturating_add)
    }
}

/// Where one track's bytes sit in its file.
struct Extent {
    /// Sector the track's bytes begin at, within the file.
    start: u64,
    /// Sectors of the track the file holds.
    sectors: u64,
    /// Bytes per sector for this track's mode.
    sector_bytes: u64,
    /// Gap the burner generates, absent from the file.
    generated_pregap: u64,
    /// Gap the file carries, between INDEX 00 and INDEX 01.
    in_file_pregap: u64,
}

/// Work out one track's extent, or say why it cannot be worked out.
fn extent_of(
    file: &CueFile,
    position: usize,
    track: &CueTrack,
    size: u64,
    warnings: &mut Vec<CueWarning>,
) -> Result<Extent, LayoutError> {
    let sector_bytes = track
        .mode
        .sector_bytes()
        .filter(|bytes| *bytes > 0)
        .ok_or_else(|| LayoutError::UnknownSectorSize {
            track: track.number,
            mode: track.mode.as_str().to_owned(),
        })?;
    let sector_bytes = u64::from(sector_bytes);
    let sectors_in_file = size / sector_bytes;
    if !size.is_multiple_of(sector_bytes) {
        warnings.push(CueWarning::FileNotSectorAligned {
            name: file.name.clone(),
            bytes: size,
            sector_bytes,
        });
    }

    if track
        .indexes
        .iter()
        .any(|index| !index.position.is_well_formed())
        || track.pregap.is_some_and(|gap| !gap.is_well_formed())
    {
        return Err(LayoutError::ImpossibleTimecode {
            track: track.number,
        });
    }

    let (Some(first), Some(index_one)) = (track.first_index(), track.index_one()) else {
        return Err(LayoutError::MissingIndexOne {
            track: track.number,
        });
    };
    let start = first.position.to_lba();
    if start >= sectors_in_file {
        return Err(LayoutError::StartsOutsideFile {
            track: track.number,
            start,
            sectors: sectors_in_file,
            name: file.name.clone(),
        });
    }

    // Where this track ends is where the next one begins, and the last track
    // in a file runs to the end of it. Nothing in the sheet states a length,
    // which is why a truncated BIN shows up as a short final track rather than
    // as an error.
    let next = match file
        .tracks
        .get(position + 1)
        .and_then(CueTrack::first_index)
    {
        Some(index) => index.position.to_lba(),
        None => sectors_in_file,
    };
    if next <= start {
        return Err(LayoutError::TracksOutOfOrder {
            track: track.number,
        });
    }

    Ok(Extent {
        start,
        sectors: next - start,
        sector_bytes,
        generated_pregap: track.pregap.map_or(0, Msf::to_lba),
        in_file_pregap: index_one.position.to_lba().saturating_sub(start),
    })
}

/// The flags a track declared that are flags, deduplicated in declared order.
fn known_flags(declared: &[String]) -> Vec<TrackFlag> {
    let mut flags = Vec::new();
    for flag in declared.iter().filter_map(|flag| TrackFlag::parse(flag)) {
        if !flags.contains(&flag) {
            flags.push(flag);
        }
    }
    flags
}

/// Turn a parsed sheet and the sizes of the files it names into track extents.
///
/// `file_sizes` is one length per FILE, in the order the sheet declared them,
/// which is the order [`resolve_references`] returns.
///
/// # Errors
///
/// [`LayoutError`] when the sheet cannot be laid out: an unknown sector size,
/// a missing INDEX 01, tracks that do not advance, or a track that starts
/// where its file has nothing. A sheet that cannot be laid out is still a
/// sheet, and the artifact is still imported; what is refused is the claim to
/// know where its tracks are.
pub fn layout(sheet: &CueSheet, file_sizes: &[u64]) -> Result<CdLayout, LayoutError> {
    if file_sizes.len() != sheet.files.len() {
        return Err(LayoutError::SizeCountMismatch {
            given: file_sizes.len(),
            declared: sheet.files.len(),
        });
    }
    if sheet.track_count() == 0 {
        return Err(LayoutError::NoTracks);
    }

    let mut warnings = Vec::new();
    let mut tracks = Vec::new();
    let mut running_lba = 0_u64;

    for (file_index, file) in sheet.files.iter().enumerate() {
        if !file.is_raw_sectors() {
            return Err(LayoutError::NotRawSectors {
                name: file.name.clone(),
                format: file.format.clone(),
            });
        }
        let size = file_sizes.get(file_index).copied().unwrap_or(0);

        for (position, track) in file.tracks.iter().enumerate() {
            let extent = extent_of(file, position, track, size, &mut warnings)?;

            // A generated gap occupies the disc without occupying the file, so
            // it moves the running position and contributes no sectors of its
            // own to read back.
            running_lba = running_lba.saturating_add(extent.generated_pregap);
            tracks.push(TrackLayout {
                number: track.number,
                session: track.session,
                mode: track.mode.as_str().to_owned(),
                file: file_index,
                file_offset_bytes: extent.start.saturating_mul(extent.sector_bytes),
                start_lba: running_lba,
                sector_count: extent.sectors,
                pregap_sectors: extent
                    .generated_pregap
                    .saturating_add(extent.in_file_pregap),
                indexes: track
                    .indexes
                    .iter()
                    .map(|index| {
                        (
                            index.number,
                            index.position.to_lba().saturating_sub(extent.start),
                        )
                    })
                    .collect(),
                isrc: track
                    .isrc
                    .clone()
                    .filter(|isrc| tangible_domain::cd::is_isrc(isrc)),
                sample_byte_order: file.sample_byte_order(),
                flags: known_flags(&track.flags),
            });
            running_lba = running_lba.saturating_add(extent.sectors);
        }
    }

    let session_count = tracks
        .iter()
        .map(|track| track.session)
        .max()
        .unwrap_or(1)
        .max(1);

    Ok(CdLayout {
        catalog: sheet
            .catalog
            .clone()
            .filter(|catalog| tangible_domain::cd::is_catalog_number(catalog)),
        tracks,
        session_count,
        warnings,
    })
}
