// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! cdrdao tables of contents: parsing and track layout.
//!
//! A TOC file is cdrdao's own descriptor for a CD, the format `cdrdao
//! read-cd` writes when it copies a disc and the one it reads to burn one. It
//! names files and says which bytes of them make each track, so it is hostile
//! input in the same way a CUE sheet is, and handled in the same three steps:
//!
//! 1. [`parse`] reads the text into statements. No filesystem, nothing
//!    resolved.
//! 2. The files it names are matched against what was staged by
//!    [`crate::cue::resolve_names`], the same trust boundary a CUE sheet goes
//!    through: nothing the text says is ever opened.
//! 3. [`layout`] turns statements and file sizes into the same track layout a
//!    CUE sheet produces, so a disc described either way is recorded, and
//!    burned, the same way.
//!
//! # The grammar
//!
//! Followed from cdrdao 1.2.4's own parser (`trackdb/TocParser.g`), the
//! version the burn worker runs, rather than from its manual, which leaves out
//! the byte offset `#n`, the optional lengths, and where each statement may
//! appear. Statements the grammar does not allow where they appear are
//! refused, as cdrdao refuses them: a file cdrdao itself would not accept is
//! not one to claim to understand.
//!
//! # What the layout can express
//!
//! A track recorded here is a gap the burner generates, followed by one
//! contiguous run of one file. That covers what cdrdao writes when it reads a
//! disc (`SILENCE`, then `FILE`, then `START`), what this project writes to
//! burn one (two `FILE` statements over adjacent bytes of one file with
//! `START` between them), and `PREGAP`. A track built differently, from two
//! files, with silence after its data, with subchannel data, or from a WAV or
//! MP3 file cdrdao would decode, is still imported, byte for byte, but no
//! layout is claimed for it.

use std::collections::BTreeMap;
use std::fmt;

use tangible_domain::cd::{self, SampleByteOrder, TrackFlag};

use crate::cue::{CdLayout, Msf, TrackLayout};

/// Largest TOC this parser will read. A 99-track TOC is a few kilobytes.
pub const MAX_TOC_BYTES: usize = 1 << 20;

/// Most tokens the lexer will produce before giving up.
const MAX_TOKENS: usize = 200_000;

/// Longest quoted string accepted, in bytes after escapes.
const MAX_STRING_BYTES: usize = 4096;

/// Most statements one track may hold.
const MAX_STATEMENTS_PER_TRACK: usize = 1000;

/// Deepest a CD-TEXT block may nest. The grammar allows three levels.
const MAX_BRACE_DEPTH: usize = 4;

/// Most index increments in one track, as cdrdao allows.
const MAX_INDEX_INCREMENTS: usize = 98;

/// Samples of stereo 16-bit audio in one sector.
const SAMPLES_PER_SECTOR: u64 = 588;

/// Bytes in one stereo 16-bit sample.
const BYTES_PER_SAMPLE: u64 = 4;

// --- the document ----------------------------------------------------------------

/// The disc type a TOC declares first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscType {
    /// `CD_DA`: audio only.
    Audio,
    /// `CD_ROM`: mode 1, possibly with audio.
    Rom,
    /// `CD_ROM_XA`: mode 2 forms.
    RomXa,
    /// `CD_I`.
    CdI,
}

impl DiscType {
    /// As the TOC spells it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Audio => "CD_DA",
            Self::Rom => "CD_ROM",
            Self::RomXa => "CD_ROM_XA",
            Self::CdI => "CD_I",
        }
    }
}

/// A track or data mode, as the TOC spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TocMode {
    /// `AUDIO`: 2352 bytes of samples.
    Audio,
    /// `MODE0`: zero data, only valid in a `ZERO` statement.
    Mode0,
    /// `MODE1`: 2048 bytes.
    Mode1,
    /// `MODE1_RAW`: 2352 bytes.
    Mode1Raw,
    /// `MODE2`: 2336 bytes of formless mode 2.
    Mode2,
    /// `MODE2_FORM1`: 2048 bytes.
    Mode2Form1,
    /// `MODE2_FORM2`: 2324 bytes.
    Mode2Form2,
    /// `MODE2_FORM_MIX`: 2336 bytes of form 1 and form 2 sectors.
    Mode2FormMix,
    /// `MODE2_RAW`: 2352 bytes.
    Mode2Raw,
}

impl TocMode {
    fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "AUDIO" => Self::Audio,
            "MODE0" => Self::Mode0,
            "MODE1" => Self::Mode1,
            "MODE1_RAW" => Self::Mode1Raw,
            "MODE2" => Self::Mode2,
            "MODE2_FORM1" => Self::Mode2Form1,
            "MODE2_FORM2" => Self::Mode2Form2,
            "MODE2_FORM_MIX" => Self::Mode2FormMix,
            "MODE2_RAW" => Self::Mode2Raw,
            _ => return None,
        })
    }

    /// As the TOC spells it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Audio => "AUDIO",
            Self::Mode0 => "MODE0",
            Self::Mode1 => "MODE1",
            Self::Mode1Raw => "MODE1_RAW",
            Self::Mode2 => "MODE2",
            Self::Mode2Form1 => "MODE2_FORM1",
            Self::Mode2Form2 => "MODE2_FORM2",
            Self::Mode2FormMix => "MODE2_FORM_MIX",
            Self::Mode2Raw => "MODE2_RAW",
        }
    }

    /// Bytes in one block of this mode, as cdrdao counts them.
    #[must_use]
    pub const fn block_bytes(self) -> u64 {
        match self {
            Self::Audio | Self::Mode1Raw | Self::Mode2Raw => 2352,
            Self::Mode0 | Self::Mode2 | Self::Mode2FormMix => 2336,
            Self::Mode1 | Self::Mode2Form1 => 2048,
            Self::Mode2Form2 => 2324,
        }
    }

    /// The mode in the vocabulary the rest of the project speaks, which is a
    /// CUE sheet's, when there is an exact equivalent.
    ///
    /// Formless `MODE2` has none. A CUE `MODE2/2336` track is form-mixed XA,
    /// and is burned as `MODE2_FORM_MIX`; recording a formless track as that
    /// would burn it back as something it was not.
    #[must_use]
    pub const fn descriptor_mode(self) -> Option<&'static str> {
        match self {
            Self::Audio => Some("AUDIO"),
            Self::Mode1 => Some("MODE1/2048"),
            Self::Mode1Raw => Some("MODE1/2352"),
            Self::Mode2Form1 => Some("MODE2/2048"),
            Self::Mode2Form2 => Some("MODE2/2324"),
            Self::Mode2FormMix => Some("MODE2/2336"),
            Self::Mode2Raw => Some("MODE2/2352"),
            Self::Mode0 | Self::Mode2 => None,
        }
    }
}

/// A length as written: a timecode, or a plain count whose unit the
/// statement decides (samples for audio, bytes for data).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Length {
    /// `MM:SS:FF`, one frame being one block.
    Msf(Msf),
    /// A plain number.
    Count(u64),
}

/// One statement that puts data into a track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// `FILE` or `AUDIOFILE`: audio samples from a file.
    AudioFile {
        /// Index into [`TocDocument::files`].
        file: usize,
        /// Whether the file's samples are little-endian (`SWAP`).
        swap: bool,
        /// Byte offset added before `start` (`#n`).
        offset: u64,
        /// Where in the file the data begins, in samples.
        start: Length,
        /// How much, in samples; to the end of the file when absent.
        length: Option<Length>,
    },
    /// `DATAFILE`: data sectors from a file.
    DataFile {
        /// Index into [`TocDocument::files`].
        file: usize,
        /// Byte offset where the data begins (`#n`).
        offset: u64,
        /// How much, in bytes or blocks; to the end of the file when absent.
        length: Option<Length>,
    },
    /// `FIFO`: data from a named pipe, which no import can hold.
    Fifo {
        /// How much.
        length: Length,
    },
    /// `SILENCE`: generated audio silence.
    Silence {
        /// How much, in samples or blocks.
        length: Length,
    },
    /// `ZERO`: generated zero data.
    Zero {
        /// The mode the zeros are written in, when it differs from the
        /// track's.
        mode: Option<TocMode>,
        /// A subchannel mode, when given.
        subchannel: bool,
        /// How much, in bytes or blocks.
        length: Length,
    },
    /// `START`: where the track proper begins, after its pregap. Without a
    /// timecode, here.
    Start(Option<Msf>),
    /// `END`: where a post-gap begins.
    End(Option<Msf>),
}

/// The flags a track declares. Each is off unless the TOC says so.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TocFlags {
    /// `COPY`.
    pub copy_permitted: bool,
    /// `PRE_EMPHASIS`.
    pub pre_emphasis: bool,
    /// `FOUR_CHANNEL_AUDIO`.
    pub four_channel: bool,
}

/// One track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TocTrack {
    /// One-based line the `TRACK` statement is on.
    pub line: usize,
    /// Its mode.
    pub mode: TocMode,
    /// Whether it declares a subchannel mode (`RW` or `RW_RAW`).
    pub subchannel: bool,
    /// The ISRC, exactly as written.
    pub isrc: Option<String>,
    /// The flags it declares.
    pub flags: TocFlags,
    /// `PREGAP`.
    pub pregap: Option<Msf>,
    /// Data statements, `START` and `END`, in order.
    pub items: Vec<Item>,
    /// `INDEX` increments, relative to the track's start.
    pub indexes: Vec<Msf>,
    /// Whether it carried a CD-TEXT block.
    pub cd_text: bool,
}

/// A parsed TOC.
#[derive(Debug, Clone, PartialEq)]
pub struct TocDocument {
    /// The declared disc type, if any.
    pub disc_type: Option<DiscType>,
    /// The catalogue number, exactly as written.
    pub catalog: Option<String>,
    /// The number of the first track.
    pub first_track: u32,
    /// The files named, each once, in the order first named.
    pub files: Vec<String>,
    /// The tracks.
    pub tracks: Vec<TocTrack>,
    /// Whether the disc carried CD-TEXT.
    pub cd_text: bool,
    /// What the parser observed that does not make the TOC unreadable.
    pub warnings: Vec<TocWarning>,
    /// What makes this recognisably a TOC, for the import record.
    pub evidence: Vec<String>,
    /// How sure the parser is that this is one.
    pub confidence: f32,
}

/// Something worth knowing about a TOC that does not stop it being read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TocWarning {
    /// A catalogue number that is not thirteen digits, not kept.
    MalformedCatalog(String),
    /// An ISRC that is not one, not kept.
    MalformedIsrc {
        /// The track.
        track: u32,
        /// What it said.
        isrc: String,
    },
    /// An all-zero code, which is what a drive reports when there is none.
    ZeroCode {
        /// The track, or `None` for the catalogue number.
        track: Option<u32>,
    },
    /// CD-TEXT, kept in the descriptor but not in the layout.
    CdTextNotKept,
    /// The declared disc type does not match the tracks.
    DiscTypeMismatch {
        /// What was declared.
        declared: &'static str,
        /// What the tracks make it.
        implied: &'static str,
    },
    /// A file ends partway through a sector, and the partial sector is not
    /// counted.
    PartialSector {
        /// The track.
        track: u32,
    },
}

impl fmt::Display for TocWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedCatalog(value) => write!(
                f,
                "the catalogue number {value:?} is not thirteen digits and is not kept"
            ),
            Self::MalformedIsrc { track, isrc } => {
                write!(f, "track {track}: {isrc:?} is not an ISRC and is not kept")
            }
            Self::ZeroCode { track: Some(track) } => write!(
                f,
                "track {track}: an all-zero ISRC, which a drive reports for a track without one, is not kept"
            ),
            Self::ZeroCode { track: None } => write!(
                f,
                "an all-zero catalogue number, which a drive reports for a disc without one, is not kept"
            ),
            Self::CdTextNotKept => write!(
                f,
                "CD-TEXT is kept in the descriptor but not recorded in the track layout"
            ),
            Self::DiscTypeMismatch { declared, implied } => write!(
                f,
                "the disc is declared {declared} but its tracks make it {implied}"
            ),
            Self::PartialSector { track } => write!(
                f,
                "track {track}: its file ends partway through a sector, which is not counted"
            ),
        }
    }
}

/// Why a TOC could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TocError {
    /// Nothing at all.
    #[error("the file is empty")]
    Empty,
    /// More than this parser reads.
    #[error("the file is larger than {MAX_TOC_BYTES} bytes")]
    TooLarge,
    /// More tokens than a disc could need.
    #[error("the file holds more than {MAX_TOKENS} tokens")]
    TooManyTokens,
    /// A character no TOC contains.
    #[error("line {line}: unexpected character {found:?}")]
    UnexpectedCharacter {
        /// One-based line.
        line: usize,
        /// The character.
        found: char,
    },
    /// A string that does not end on its line, or is too long.
    #[error("line {line}: a quoted string is unterminated or too long")]
    BadString {
        /// One-based line.
        line: usize,
    },
    /// A string that is not text.
    #[error("line {line}: a quoted string is not UTF-8")]
    NonUtf8String {
        /// One-based line.
        line: usize,
    },
    /// A number too large to mean anything.
    #[error("line {line}: a number is too large")]
    NumberTooLarge {
        /// One-based line.
        line: usize,
    },
    /// A token the grammar does not allow here.
    #[error("line {line}: expected {expected}, found {found}")]
    Unexpected {
        /// One-based line.
        line: usize,
        /// What the grammar wanted.
        expected: &'static str,
        /// What was there.
        found: String,
    },
    /// A timecode with seconds or frames out of range.
    #[error("line {line}: {value} is not a timecode")]
    BadTimecode {
        /// One-based line.
        line: usize,
        /// What was written.
        value: String,
    },
    /// No tracks.
    #[error("no TRACK statements")]
    NoTracks,
    /// More tracks than a CD holds, or numbered past 99.
    #[error("more tracks than a CD can hold")]
    TooManyTracks,
    /// A statement cdrdao refuses in this place.
    #[error("line {line}: {reason}")]
    Refused {
        /// One-based line.
        line: usize,
        /// Why.
        reason: &'static str,
    },
}

// --- lexing ------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    Integer(u64),
    Str(String),
    Punct(char),
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Word(word) => write!(f, "{word}"),
            Self::Integer(value) => write!(f, "{value}"),
            Self::Str(_) => write!(f, "a quoted string"),
            Self::Punct(c) => write!(f, "{c:?}"),
        }
    }
}

fn lex(bytes: &[u8]) -> Result<Vec<(Token, usize)>, TocError> {
    let mut tokens = Vec::new();
    let mut line = 1_usize;
    let mut i = 0;
    while i < bytes.len() {
        if tokens.len() >= MAX_TOKENS {
            return Err(TocError::TooManyTokens);
        }
        let byte = bytes[i];
        match byte {
            b'\n' => {
                line += 1;
                i += 1;
            }
            b' ' | b'\t' | b'\r' => i += 1,
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'"' => {
                let (text, next) = lex_string(bytes, i + 1, line)?;
                tokens.push((Token::Str(text), line));
                i = next;
            }
            b'0'..=b'9' => {
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                let digits = std::str::from_utf8(&bytes[start..i]).unwrap_or("");
                let value = digits
                    .parse::<u64>()
                    .map_err(|_| TocError::NumberTooLarge { line })?;
                tokens.push((Token::Integer(value), line));
            }
            b'A'..=b'Z' | b'a'..=b'z' | b'_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                if i - start > 64 {
                    return Err(TocError::Unexpected {
                        line,
                        expected: "a keyword",
                        found: "an overlong word".to_owned(),
                    });
                }
                let word = String::from_utf8_lossy(&bytes[start..i]).into_owned();
                tokens.push((Token::Word(word), line));
            }
            b':' | b'#' | b'{' | b'}' | b',' => {
                tokens.push((Token::Punct(byte as char), line));
                i += 1;
            }
            _ => {
                let found = std::str::from_utf8(&bytes[i..])
                    .ok()
                    .and_then(|rest| rest.chars().next())
                    .unwrap_or(char::REPLACEMENT_CHARACTER);
                return Err(TocError::UnexpectedCharacter { line, found });
            }
        }
    }
    Ok(tokens)
}

/// Read a quoted string from just after its opening quote.
///
/// The escapes cdrdao's lexer knows: `\"` for a quote and `\ooo` for an octal
/// byte. Any other backslash is itself. A newline or a tab ends the string in
/// error, as it does for cdrdao.
fn lex_string(bytes: &[u8], mut i: usize, line: usize) -> Result<(String, usize), TocError> {
    let mut out = Vec::new();
    loop {
        if out.len() > MAX_STRING_BYTES {
            return Err(TocError::BadString { line });
        }
        match bytes.get(i) {
            None | Some(b'\n' | b'\t') => return Err(TocError::BadString { line }),
            Some(b'"') => {
                let text = String::from_utf8(out).map_err(|_| TocError::NonUtf8String { line })?;
                return Ok((text, i + 1));
            }
            Some(b'\\') => {
                if bytes.get(i + 1) == Some(&b'"') {
                    out.push(b'"');
                    i += 2;
                } else if let Some(octal) = bytes
                    .get(i + 1..i + 4)
                    .filter(|d| d.iter().all(u8::is_ascii_digit))
                {
                    let value = octal
                        .iter()
                        .fold(0_u32, |acc, digit| acc * 8 + u32::from(digit - b'0'));
                    out.push(u8::try_from(value & 0xff).unwrap_or(0));
                    i += 4;
                } else {
                    out.push(b'\\');
                    i += 1;
                }
            }
            Some(&byte) => {
                out.push(byte);
                i += 1;
            }
        }
    }
}

// --- parsing -----------------------------------------------------------------------

struct Parser {
    tokens: Vec<(Token, usize)>,
    position: usize,
    files: Vec<String>,
    file_index: BTreeMap<String, usize>,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position).map(|(token, _)| token)
    }

    fn peek_word(&self) -> Option<&str> {
        match self.peek() {
            Some(Token::Word(word)) => Some(word),
            _ => None,
        }
    }

    fn line(&self) -> usize {
        self.tokens
            .get(self.position)
            .or_else(|| self.tokens.last())
            .map_or(1, |(_, line)| *line)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self
            .tokens
            .get(self.position)
            .map(|(token, _)| token.clone());
        if token.is_some() {
            self.position += 1;
        }
        token
    }

    fn unexpected(&self, expected: &'static str) -> TocError {
        TocError::Unexpected {
            line: self.line(),
            expected,
            found: self
                .peek()
                .map_or_else(|| "the end of the file".to_owned(), ToString::to_string),
        }
    }

    fn eat_word(&mut self, word: &str) -> bool {
        if self.peek_word() == Some(word) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn eat_punct(&mut self, c: char) -> bool {
        if self.peek() == Some(&Token::Punct(c)) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn integer(&mut self, expected: &'static str) -> Result<u64, TocError> {
        match self.peek() {
            Some(Token::Integer(value)) => {
                let value = *value;
                self.position += 1;
                Ok(value)
            }
            _ => Err(self.unexpected(expected)),
        }
    }

    fn string(&mut self, expected: &'static str) -> Result<String, TocError> {
        match self.peek() {
            Some(Token::Str(text)) => {
                let text = text.clone();
                self.position += 1;
                Ok(text)
            }
            _ => Err(self.unexpected(expected)),
        }
    }

    fn file(&mut self) -> Result<usize, TocError> {
        let name = self.string("a quoted file name")?;
        if let Some(index) = self.file_index.get(&name) {
            return Ok(*index);
        }
        let index = self.files.len();
        self.files.push(name.clone());
        self.file_index.insert(name, index);
        Ok(index)
    }

    /// `MM:SS:FF`, starting at an integer already peeked.
    fn msf(&mut self) -> Result<Msf, TocError> {
        let line = self.line();
        let minutes = self.integer("a timecode")?;
        if !self.eat_punct(':') {
            return Err(self.unexpected("':' in a timecode"));
        }
        let seconds = self.integer("a timecode")?;
        if !self.eat_punct(':') {
            return Err(self.unexpected("':' in a timecode"));
        }
        let frames = self.integer("a timecode")?;
        let value = format!("{minutes}:{seconds}:{frames}");
        let (Ok(minutes), Ok(seconds), Ok(frames)) = (
            u32::try_from(minutes),
            u32::try_from(seconds),
            u32::try_from(frames),
        ) else {
            return Err(TocError::BadTimecode { line, value });
        };
        let msf = Msf {
            minutes,
            seconds,
            frames,
        };
        if msf.is_well_formed() {
            Ok(msf)
        } else {
            Err(TocError::BadTimecode { line, value })
        }
    }

    /// A timecode or a plain count.
    fn length(&mut self) -> Result<Length, TocError> {
        if !matches!(self.peek(), Some(Token::Integer(_))) {
            return Err(self.unexpected("a length"));
        }
        if self.tokens.get(self.position + 1).map(|(token, _)| token) == Some(&Token::Punct(':')) {
            self.msf().map(Length::Msf)
        } else {
            self.integer("a length").map(Length::Count)
        }
    }

    fn at_integer(&self) -> bool {
        matches!(self.peek(), Some(Token::Integer(_)))
    }

    /// Step over a `{ ... }` block, which must come next.
    ///
    /// CD-TEXT is kept in the descriptor, preserved byte for byte, and not
    /// recorded in the layout, so all that matters here is that it is a
    /// well-formed block and that it ends.
    fn skip_block(&mut self) -> Result<(), TocError> {
        if !self.eat_punct('{') {
            return Err(self.unexpected("'{'"));
        }
        let mut depth = 1_usize;
        while depth > 0 {
            match self.next() {
                None => return Err(self.unexpected("'}'")),
                Some(Token::Punct('{')) => {
                    depth += 1;
                    if depth > MAX_BRACE_DEPTH {
                        return Err(TocError::Refused {
                            line: self.line(),
                            reason: "CD-TEXT nested deeper than the format allows",
                        });
                    }
                }
                Some(Token::Punct('}')) => depth -= 1,
                Some(_) => {}
            }
        }
        Ok(())
    }

    fn track(&mut self) -> Result<TocTrack, TocError> {
        let line = self.line();
        if !self.eat_word("TRACK") {
            return Err(self.unexpected("TRACK"));
        }
        let mode = self
            .peek_word()
            .and_then(TocMode::parse)
            .filter(|mode| *mode != TocMode::Mode0)
            .ok_or_else(|| self.unexpected("a track mode"))?;
        self.position += 1;
        let subchannel = self.eat_word("RW") || self.eat_word("RW_RAW");

        let mut track = TocTrack {
            line,
            mode,
            subchannel,
            isrc: None,
            flags: TocFlags::default(),
            pregap: None,
            items: Vec::new(),
            indexes: Vec::new(),
            cd_text: false,
        };
        self.flags(&mut track)?;

        if self.eat_word("CD_TEXT") {
            self.skip_block()?;
            track.cd_text = true;
        }

        let mut marks = Marks::default();
        if self.eat_word("PREGAP") {
            let line = self.line();
            let gap = self.msf()?;
            if gap.to_lba() == 0 {
                return Err(TocError::Refused {
                    line,
                    reason: "a PREGAP of zero length",
                });
            }
            track.pregap = Some(gap);
            marks.start = true;
        }

        while let Some(item) = self.data_statement(&track, &mut marks)? {
            if track.items.len() >= MAX_STATEMENTS_PER_TRACK {
                return Err(TocError::Refused {
                    line: self.line(),
                    reason: "more statements in one track than any disc needs",
                });
            }
            track.items.push(item);
        }
        if !track
            .items
            .iter()
            .any(|item| !matches!(item, Item::Start(_) | Item::End(_)))
        {
            return Err(self.unexpected("FILE, DATAFILE, SILENCE or ZERO"));
        }

        while self.eat_word("INDEX") {
            if track.indexes.len() >= MAX_INDEX_INCREMENTS {
                return Err(TocError::Refused {
                    line: self.line(),
                    reason: "more than 98 index increments",
                });
            }
            track.indexes.push(self.msf()?);
        }

        Ok(track)
    }

    /// Flags, in any order and any number, before anything else.
    fn flags(&mut self, track: &mut TocTrack) -> Result<(), TocError> {
        loop {
            if self.eat_word("ISRC") {
                track.isrc = Some(self.string("a quoted ISRC")?);
            } else if self.eat_word("NO") {
                if self.eat_word("COPY") {
                    track.flags.copy_permitted = false;
                } else if self.eat_word("PRE_EMPHASIS") {
                    track.flags.pre_emphasis = false;
                } else {
                    return Err(self.unexpected("COPY or PRE_EMPHASIS after NO"));
                }
            } else if self.eat_word("COPY") {
                track.flags.copy_permitted = true;
            } else if self.eat_word("PRE_EMPHASIS") {
                track.flags.pre_emphasis = true;
            } else if self.eat_word("TWO_CHANNEL_AUDIO") {
                track.flags.four_channel = false;
            } else if self.eat_word("FOUR_CHANNEL_AUDIO") {
                track.flags.four_channel = true;
            } else {
                return Ok(());
            }
        }
    }

    /// The next data statement, `START` or `END`, if one comes next.
    fn data_statement(
        &mut self,
        track: &TocTrack,
        marks: &mut Marks,
    ) -> Result<Option<Item>, TocError> {
        let line = self.line();
        let audio_only = |reason| {
            if track.mode == TocMode::Audio && !track.subchannel {
                Ok(())
            } else {
                Err(TocError::Refused { line, reason })
            }
        };
        let item = match self.peek_word() {
            Some("FILE" | "AUDIOFILE") => {
                self.position += 1;
                let file = self.file()?;
                let swap = self.eat_word("SWAP");
                let offset = self.offset()?;
                let start = self.length()?;
                let length = self.optional_length()?;
                audio_only("FILE is only allowed in an audio track without a subchannel mode")?;
                marks.kind(true, line)?;
                Item::AudioFile {
                    file,
                    swap,
                    offset,
                    start,
                    length,
                }
            }
            Some("DATAFILE") => {
                self.position += 1;
                let file = self.file()?;
                let offset = self.offset()?;
                let length = self.optional_length()?;
                marks.kind(false, line)?;
                Item::DataFile {
                    file,
                    offset,
                    length,
                }
            }
            Some("FIFO") => {
                self.position += 1;
                self.string("a quoted FIFO path")?;
                Item::Fifo {
                    length: self.length()?,
                }
            }
            Some("SILENCE") => {
                self.position += 1;
                let length = self.length()?;
                audio_only("SILENCE is only allowed in an audio track without a subchannel mode")?;
                refuse_zero(length, line, "a SILENCE of zero length")?;
                marks.kind(true, line)?;
                Item::Silence { length }
            }
            Some("ZERO") => {
                self.position += 1;
                let mode = self.peek_word().and_then(TocMode::parse);
                if mode.is_some() {
                    self.position += 1;
                }
                let subchannel = self.eat_word("RW") || self.eat_word("RW_RAW");
                let length = self.length()?;
                refuse_zero(length, line, "a ZERO of zero length")?;
                marks.kind(false, line)?;
                Item::Zero {
                    mode,
                    subchannel,
                    length,
                }
            }
            Some("START") => {
                self.position += 1;
                marks.once(true, line)?;
                Item::Start(self.optional_msf()?)
            }
            Some("END") => {
                self.position += 1;
                marks.once(false, line)?;
                Item::End(self.optional_msf()?)
            }
            _ => return Ok(None),
        };
        Ok(Some(item))
    }

    fn offset(&mut self) -> Result<u64, TocError> {
        if self.eat_punct('#') {
            self.integer("a byte offset after '#'")
        } else {
            Ok(0)
        }
    }

    fn optional_length(&mut self) -> Result<Option<Length>, TocError> {
        if self.at_integer() {
            self.length().map(Some)
        } else {
            Ok(None)
        }
    }

    fn optional_msf(&mut self) -> Result<Option<Msf>, TocError> {
        if self.at_integer() {
            self.msf().map(Some)
        } else {
            Ok(None)
        }
    }
}

/// What a track has declared so far, for the statements cdrdao allows once
/// or refuses to mix.
#[derive(Default)]
struct Marks {
    start: bool,
    end: bool,
    /// `Some(true)` once an audio statement is seen, `Some(false)` for data.
    kind: Option<bool>,
}

impl Marks {
    fn once(&mut self, start: bool, line: usize) -> Result<(), TocError> {
        let (seen, reason) = if start {
            (&mut self.start, "the track start is already defined")
        } else {
            (&mut self.end, "the track end is already defined")
        };
        if *seen {
            return Err(TocError::Refused { line, reason });
        }
        *seen = true;
        Ok(())
    }

    /// cdrdao refuses a track that mixes audio statements with data ones.
    fn kind(&mut self, audio: bool, line: usize) -> Result<(), TocError> {
        match self.kind {
            Some(previous) if previous != audio => Err(TocError::Refused {
                line,
                reason: "a track mixes FILE or SILENCE with DATAFILE or ZERO",
            }),
            _ => {
                self.kind = Some(audio);
                Ok(())
            }
        }
    }
}

fn refuse_zero(length: Length, line: usize, reason: &'static str) -> Result<(), TocError> {
    if length_is_zero(length) {
        Err(TocError::Refused { line, reason })
    } else {
        Ok(())
    }
}

impl Parser {}

fn length_is_zero(length: Length) -> bool {
    match length {
        Length::Msf(msf) => msf.to_lba() == 0,
        Length::Count(count) => count == 0,
    }
}

/// Read a TOC.
///
/// # Errors
///
/// [`TocError`] for anything cdrdao itself would refuse to read, and for
/// input past the bounds this parser sets.
pub fn parse(bytes: &[u8]) -> Result<TocDocument, TocError> {
    if bytes.len() > MAX_TOC_BYTES {
        return Err(TocError::TooLarge);
    }
    let tokens = lex(bytes)?;
    if tokens.is_empty() {
        return Err(TocError::Empty);
    }
    let mut parser = Parser {
        tokens,
        position: 0,
        files: Vec::new(),
        file_index: BTreeMap::new(),
    };

    let mut disc_type = None;
    let mut catalog = None;
    loop {
        if parser.eat_word("CATALOG") {
            catalog = Some(parser.string("a quoted catalogue number")?);
            continue;
        }
        let declared = match parser.peek_word() {
            Some("CD_DA") => DiscType::Audio,
            Some("CD_ROM") => DiscType::Rom,
            Some("CD_ROM_XA") => DiscType::RomXa,
            Some("CD_I") => DiscType::CdI,
            _ => break,
        };
        parser.position += 1;
        disc_type = Some(declared);
    }

    let mut first_track = 1_u32;
    if parser.eat_word("FIRST_TRACK_NO") {
        let line = parser.line();
        let value = parser.integer("a track number")?;
        first_track = u32::try_from(value)
            .ok()
            .filter(|number| (1..=cd::MAX_TRACKS).contains(number))
            .ok_or(TocError::Refused {
                line,
                reason: "a first track number outside 1 to 99",
            })?;
    }

    let mut cd_text = false;
    if parser.eat_word("CD_TEXT") {
        parser.skip_block()?;
        cd_text = true;
    }

    let mut tracks = Vec::new();
    while parser.peek().is_some() {
        tracks.push(parser.track()?);
        let last = first_track as usize + tracks.len() - 1;
        if last > cd::MAX_TRACKS as usize {
            return Err(TocError::TooManyTracks);
        }
    }
    if tracks.is_empty() {
        return Err(TocError::NoTracks);
    }

    let cd_text = cd_text || tracks.iter().any(|track| track.cd_text);
    let mut warnings = Vec::new();
    if cd_text {
        warnings.push(TocWarning::CdTextNotKept);
    }
    if let Some(declared) = disc_type {
        let implied = implied_disc_type(&tracks);
        if declared != implied && declared != DiscType::CdI {
            warnings.push(TocWarning::DiscTypeMismatch {
                declared: declared.as_str(),
                implied: implied.as_str(),
            });
        }
    }

    let mut evidence = vec![format!(
        "cdrdao table of contents: {} track(s) from {} file(s)",
        tracks.len(),
        parser.files.len()
    )];
    if let Some(declared) = disc_type {
        evidence.push(format!("declared {}", declared.as_str()));
    }
    let mut modes: Vec<&str> = tracks.iter().map(|track| track.mode.as_str()).collect();
    modes.sort_unstable();
    modes.dedup();
    evidence.push(format!("modes: {}", modes.join(", ")));

    Ok(TocDocument {
        confidence: if disc_type.is_some() { 0.95 } else { 0.85 },
        disc_type,
        catalog,
        first_track,
        files: parser.files,
        tracks,
        cd_text,
        warnings,
        evidence,
    })
}

fn implied_disc_type(tracks: &[TocTrack]) -> DiscType {
    let mode2 = tracks.iter().any(|track| {
        matches!(
            track.mode,
            TocMode::Mode2
                | TocMode::Mode2Form1
                | TocMode::Mode2Form2
                | TocMode::Mode2FormMix
                | TocMode::Mode2Raw
        )
    });
    if mode2 {
        DiscType::RomXa
    } else if tracks.iter().any(|track| track.mode != TocMode::Audio) {
        DiscType::Rom
    } else {
        DiscType::Audio
    }
}

// --- layout ------------------------------------------------------------------------

/// Why a TOC's tracks could not be laid out.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TocLayoutError {
    /// One size per named file is needed.
    #[error("{given} file sizes were given for {declared} named files")]
    SizeCountMismatch {
        /// Sizes given.
        given: usize,
        /// Files named.
        declared: usize,
    },
    /// A track built in a way the layout cannot record exactly.
    #[error("track {track}: {reason}")]
    NotExpressible {
        /// The track.
        track: u32,
        /// Why.
        reason: String,
    },
    /// A track that names bytes past the end of its file.
    #[error("track {track}: reads past the end of {name}")]
    OutsideFile {
        /// The track.
        track: u32,
        /// The file as named.
        name: String,
    },
    /// A track start, end or index that falls outside the track.
    #[error("track {track}: {reason}")]
    BadPosition {
        /// The track.
        track: u32,
        /// What is wrong.
        reason: &'static str,
    },
}

/// A laid-out TOC, and what laying it out observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TocLayout {
    /// The tracks, in the shape every descriptor produces.
    pub layout: CdLayout,
    /// Observations that do not invalidate it.
    pub warnings: Vec<TocWarning>,
}

/// Sectors in a length counted in samples or timecode blocks, if whole.
fn sample_sectors(length: Length) -> Option<u64> {
    match length {
        Length::Msf(msf) => Some(msf.to_lba()),
        Length::Count(samples) => samples
            .is_multiple_of(SAMPLES_PER_SECTOR)
            .then_some(samples / SAMPLES_PER_SECTOR),
    }
}

/// Bytes in a length counted in bytes or timecode blocks.
fn data_bytes(length: Length, block: u64) -> u64 {
    match length {
        Length::Msf(msf) => msf.to_lba().saturating_mul(block),
        Length::Count(bytes) => bytes,
    }
}

/// Bytes in a length counted in samples or timecode blocks.
fn audio_bytes(length: Length) -> u64 {
    match length {
        Length::Msf(msf) => msf
            .to_lba()
            .saturating_mul(SAMPLES_PER_SECTOR)
            .saturating_mul(BYTES_PER_SAMPLE),
        Length::Count(samples) => samples.saturating_mul(BYTES_PER_SAMPLE),
    }
}

/// Where in a file a run of a track's bytes lies.
struct Region {
    file: usize,
    start: u64,
    end: u64,
    swap: bool,
}

/// Files whose audio cdrdao decodes rather than reading raw.
fn decoded_audio(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".wav", ".mp3", ".ogg", ".flac", ".au"]
        .iter()
        .any(|extension| lower.ends_with(extension))
}

/// The code a TOC carried, if it is one worth keeping.
fn kept_code(
    value: Option<&String>,
    valid: fn(&str) -> bool,
    track: Option<u32>,
    warnings: &mut Vec<TocWarning>,
) -> Option<String> {
    let value = value?;
    if !value.is_empty() && value.bytes().all(|byte| byte == b'0') {
        warnings.push(TocWarning::ZeroCode { track });
        return None;
    }
    if valid(value) {
        return Some(value.clone());
    }
    warnings.push(match track {
        Some(track) => TocWarning::MalformedIsrc {
            track,
            isrc: value.clone(),
        },
        None => TocWarning::MalformedCatalog(value.clone()),
    });
    None
}

/// Turn a parsed TOC and the sizes of the files it names into track extents.
///
/// `file_sizes` is one length per entry in [`TocDocument::files`], in that
/// order.
///
/// # Errors
///
/// [`TocLayoutError`] when a track cannot be recorded exactly. The TOC is
/// still a TOC and the artifact is still imported; what is refused is the
/// claim to know where its tracks are.
pub fn layout(document: &TocDocument, file_sizes: &[u64]) -> Result<TocLayout, TocLayoutError> {
    if file_sizes.len() != document.files.len() {
        return Err(TocLayoutError::SizeCountMismatch {
            given: file_sizes.len(),
            declared: document.files.len(),
        });
    }
    let mut warnings = Vec::new();
    let catalog = kept_code(
        document.catalog.as_ref(),
        cd::is_catalog_number,
        None,
        &mut warnings,
    );

    let mut tracks = Vec::with_capacity(document.tracks.len());
    let mut running_lba = 0_u64;
    for (position, track) in document.tracks.iter().enumerate() {
        let number = document.first_track + u32::try_from(position).unwrap_or(u32::MAX);
        let laid = lay_out_track(
            document,
            track,
            number,
            position == 0,
            file_sizes,
            &mut warnings,
        )?;
        running_lba = running_lba.saturating_add(laid.generated);
        let isrc = kept_code(
            track.isrc.as_ref(),
            cd::is_isrc,
            Some(number),
            &mut warnings,
        );
        let mut flags = Vec::new();
        if track.flags.copy_permitted {
            flags.push(TrackFlag::DigitalCopyPermitted);
        }
        if track.flags.four_channel {
            flags.push(TrackFlag::FourChannel);
        }
        if track.flags.pre_emphasis {
            flags.push(TrackFlag::PreEmphasis);
        }
        tracks.push(TrackLayout {
            number,
            session: 1,
            mode: laid.mode.to_owned(),
            file: laid.region.file,
            file_offset_bytes: laid.region.start,
            start_lba: running_lba,
            sector_count: laid.sectors,
            pregap_sectors: laid.generated.saturating_add(laid.in_file_pregap),
            indexes: laid.indexes,
            isrc,
            sample_byte_order: (track.mode == TocMode::Audio).then_some(if laid.region.swap {
                SampleByteOrder::LittleEndian
            } else {
                SampleByteOrder::BigEndian
            }),
            flags,
        });
        running_lba = running_lba.saturating_add(laid.sectors);
    }

    Ok(TocLayout {
        layout: CdLayout {
            catalog,
            tracks,
            session_count: 1,
            warnings: Vec::new(),
        },
        warnings,
    })
}

/// One track's extent.
struct LaidOut {
    mode: &'static str,
    region: Region,
    sectors: u64,
    generated: u64,
    in_file_pregap: u64,
    indexes: Vec<(u32, u64)>,
}

#[allow(clippy::too_many_lines)]
fn lay_out_track(
    document: &TocDocument,
    track: &TocTrack,
    number: u32,
    first: bool,
    file_sizes: &[u64],
    warnings: &mut Vec<TocWarning>,
) -> Result<LaidOut, TocLayoutError> {
    let refuse = |reason: &str| TocLayoutError::NotExpressible {
        track: number,
        reason: reason.to_owned(),
    };
    if track.subchannel {
        return Err(refuse(
            "it carries subchannel data, which the layout does not record",
        ));
    }
    let mode = track
        .mode
        .descriptor_mode()
        .ok_or_else(|| refuse("formless MODE2 has no equivalent that would burn back the same"))?;
    let block = track.mode.block_bytes();

    // The PREGAP statement is a generated gap that ends where the track
    // begins.
    let mut generated = track.pregap.map_or(0, Msf::to_lba);
    let mut start_at: Option<u64> = track.pregap.map(Msf::to_lba);
    let mut region: Option<Region> = None;
    // Sectors so far, for a START without a timecode.
    let mut length_so_far = generated;

    for item in &track.items {
        match item {
            Item::Silence { length } => {
                if region.is_some() {
                    return Err(refuse("silence follows its data"));
                }
                let sectors = sample_sectors(*length)
                    .ok_or_else(|| refuse("a SILENCE that is not a whole number of sectors"))?;
                generated += sectors;
                length_so_far += sectors;
            }
            Item::Zero {
                mode: zero_mode,
                subchannel,
                length,
            } => {
                if region.is_some() {
                    return Err(refuse("zero data follows its data"));
                }
                if *subchannel {
                    return Err(refuse("a ZERO with subchannel data"));
                }
                let zero_block = zero_mode.unwrap_or(track.mode).block_bytes();
                let bytes = data_bytes(*length, zero_block);
                if !bytes.is_multiple_of(zero_block) {
                    return Err(refuse("a ZERO that is not a whole number of sectors"));
                }
                generated += bytes / zero_block;
                length_so_far += bytes / zero_block;
            }
            Item::Fifo { .. } => {
                return Err(refuse("data from a FIFO, which no import can hold"));
            }
            Item::AudioFile {
                file,
                swap,
                offset,
                start,
                length,
            } => {
                let name = &document.files[*file];
                if decoded_audio(name) {
                    return Err(refuse(
                        "audio from a file cdrdao decodes, which is not raw sectors",
                    ));
                }
                let size = file_sizes[*file];
                let begin = offset.saturating_add(audio_bytes(*start));
                let end = match length {
                    Some(length) => {
                        let bytes = audio_bytes(*length);
                        if !bytes.is_multiple_of(block) {
                            return Err(refuse(
                                "a FILE length that is not a whole number of sectors",
                            ));
                        }
                        begin.saturating_add(bytes)
                    }
                    None => size,
                };
                extend(
                    &mut region,
                    *file,
                    begin,
                    end,
                    *swap,
                    number,
                    name,
                    size,
                    &refuse,
                )?;
                length_so_far = region_sectors(region.as_ref(), block);
                length_so_far += generated;
            }
            Item::DataFile {
                file,
                offset,
                length,
            } => {
                let name = &document.files[*file];
                let size = file_sizes[*file];
                let begin = *offset;
                let end = match length {
                    Some(length) => {
                        let bytes = data_bytes(*length, block);
                        if !bytes.is_multiple_of(block) {
                            return Err(refuse(
                                "a DATAFILE length that is not a whole number of sectors",
                            ));
                        }
                        begin.saturating_add(bytes)
                    }
                    None => size,
                };
                extend(
                    &mut region,
                    *file,
                    begin,
                    end,
                    false,
                    number,
                    name,
                    size,
                    &refuse,
                )?;
                length_so_far = region_sectors(region.as_ref(), block);
                length_so_far += generated;
            }
            Item::Start(at) => {
                start_at = Some(at.map_or(length_so_far, Msf::to_lba));
            }
            Item::End(_) => {
                return Err(refuse("a post-gap, which the layout does not record"));
            }
        }
    }

    let region = region.ok_or_else(|| refuse("no data from a file"))?;
    let span = region.end - region.start;
    if !span.is_multiple_of(block) {
        warnings.push(TocWarning::PartialSector { track: number });
    }
    let sectors = span / block;
    if sectors == 0 {
        return Err(refuse("no whole sector of data"));
    }

    let start = start_at.unwrap_or(0);
    if start < generated {
        return Err(refuse(
            "its start falls inside generated silence, leaving silence in the track proper",
        ));
    }
    let in_file_pregap = start - generated;
    if in_file_pregap >= sectors {
        return Err(TocLayoutError::BadPosition {
            track: number,
            reason: "START is at or past the end of the track",
        });
    }
    if first && start > 0 {
        return Err(TocLayoutError::BadPosition {
            track: number,
            reason: "the first track must not have a pregap",
        });
    }

    let mut indexes = Vec::with_capacity(track.indexes.len() + 2);
    if in_file_pregap > 0 {
        indexes.push((0, 0));
    }
    indexes.push((1, in_file_pregap));
    let mut previous = 0_u64;
    for (k, increment) in track.indexes.iter().enumerate() {
        let at = increment.to_lba();
        if at == 0 {
            return Err(TocLayoutError::BadPosition {
                track: number,
                reason: "an INDEX at the start of the track",
            });
        }
        if at <= previous {
            return Err(TocLayoutError::BadPosition {
                track: number,
                reason: "INDEX increments that do not advance",
            });
        }
        if in_file_pregap + at >= sectors {
            return Err(TocLayoutError::BadPosition {
                track: number,
                reason: "an INDEX past the end of the track",
            });
        }
        previous = at;
        indexes.push((
            u32::try_from(k + 2).unwrap_or(u32::MAX),
            in_file_pregap + at,
        ));
    }

    Ok(LaidOut {
        mode,
        region,
        sectors,
        generated,
        in_file_pregap,
        indexes,
    })
}

/// Add a run of bytes to a track's region, which must stay one contiguous
/// run of one file read the same way.
#[allow(clippy::too_many_arguments)]
fn extend(
    region: &mut Option<Region>,
    file: usize,
    begin: u64,
    end: u64,
    swap: bool,
    number: u32,
    name: &str,
    size: u64,
    refuse: &dyn Fn(&str) -> TocLayoutError,
) -> Result<(), TocLayoutError> {
    if begin > size || end > size || end < begin {
        return Err(TocLayoutError::OutsideFile {
            track: number,
            name: name.to_owned(),
        });
    }
    match region {
        None => {
            *region = Some(Region {
                file,
                start: begin,
                end,
                swap,
            });
            Ok(())
        }
        Some(existing) => {
            if existing.file != file {
                return Err(refuse("its data comes from more than one file"));
            }
            if existing.end != begin {
                return Err(refuse("its data is not one contiguous run of its file"));
            }
            if existing.swap != swap {
                return Err(refuse("one file read with two byte orders"));
            }
            existing.end = end;
            Ok(())
        }
    }
}

/// Whole sectors the region holds so far.
fn region_sectors(region: Option<&Region>, block: u64) -> u64 {
    region.map_or(0, |region| (region.end - region.start) / block)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn document(text: &str) -> TocDocument {
        parse(text.as_bytes()).expect("a readable TOC")
    }

    #[test]
    fn strings_take_cdrdaos_escapes_and_nothing_else() {
        let toc = document("TRACK AUDIO\nFILE \"a \\\"b\\\" \\101\\\\c.bin\" 0\n");
        assert_eq!(toc.files, vec!["a \"b\" A\\\\c.bin".to_owned()]);
    }

    #[test]
    fn a_string_cannot_run_past_its_line() {
        assert!(matches!(
            parse(b"TRACK AUDIO\nFILE \"a.bin\n\" 0\n"),
            Err(TocError::BadString { line: 2 })
        ));
    }

    #[test]
    fn comments_are_skipped() {
        let toc = document("// made by hand\nCD_DA // audio\nTRACK AUDIO // one\nSILENCE 1:0:0\n");
        assert_eq!(toc.tracks.len(), 1);
        assert_eq!(toc.disc_type, Some(DiscType::Audio));
    }

    #[test]
    fn statements_out_of_their_place_are_refused_as_cdrdao_refuses_them() {
        for (text, why) in [
            ("TRACK AUDIO\nFILE \"a\" 0\nCOPY\n", "a flag after data"),
            ("TRACK MODE1\nFILE \"a\" 0\n", "FILE in a data track"),
            (
                "TRACK AUDIO\nDATAFILE \"a\"\nSILENCE 0:2:0\n",
                "mixed kinds",
            ),
            ("TRACK AUDIO\nSILENCE 0\n", "zero silence"),
            ("TRACK AUDIO\nPREGAP 0:0:0\nFILE \"a\" 0\n", "zero pregap"),
            (
                "TRACK AUDIO\nPREGAP 0:2:0\nSTART\nFILE \"a\" 0\n",
                "start twice",
            ),
            ("TRACK AUDIO\n", "no data"),
            ("TRACK MODE0\nZERO 0:2:0\n", "MODE0 track"),
            ("TRACK AUDIO\nFILE \"a\" 0\nBOGUS\n", "unknown statement"),
            ("TRACK AUDIO\nFILE \"a\" 0:60:0\n", "bad timecode"),
            ("TRACK AUDIO\nFILE \"a\" 0:0:75\n", "bad frames"),
            (
                "FIRST_TRACK_NO 0\nTRACK AUDIO\nSILENCE 1:0:0\n",
                "track number zero",
            ),
            ("CD_DA\n", "no tracks"),
            (
                "TRACK AUDIO\nCD_TEXT { { { { { } } } } }\nSILENCE 1:0:0\n",
                "deep CD-TEXT",
            ),
            ("TRACK AUDIO\nFILE \"a\" 0\n@\n", "stray character"),
        ] {
            assert!(parse(text.as_bytes()).is_err(), "{why} was accepted");
        }
    }

    #[test]
    fn a_track_number_past_99_is_refused() {
        let mut text = String::from("FIRST_TRACK_NO 98\n");
        for _ in 0..3 {
            text.push_str("TRACK AUDIO\nSILENCE 0:4:0\n");
        }
        assert_eq!(parse(text.as_bytes()), Err(TocError::TooManyTracks));
    }

    #[test]
    fn a_file_named_by_several_tracks_is_one_file() {
        let toc = document(
            "CD_ROM\nTRACK MODE1\nDATAFILE \"d.bin\" 0:10:0\nTRACK AUDIO\nFILE \"d.bin\" #1536000 0 0:20:0\n",
        );
        assert_eq!(toc.files, vec!["d.bin".to_owned()]);
    }

    #[test]
    fn cd_text_is_stepped_over_and_noted() {
        let toc = document(
            "CD_DA\nCD_TEXT {\n LANGUAGE_MAP { 0 : EN }\n LANGUAGE 0 { TITLE \"Disc\" SIZE_INFO { 0, 1, 2 } }\n}\nTRACK AUDIO\nCD_TEXT { LANGUAGE 0 { TITLE \"One\" } }\nSILENCE 0:4:0\n",
        );
        assert!(toc.cd_text);
        assert!(toc.warnings.contains(&TocWarning::CdTextNotKept));
    }

    #[test]
    fn the_input_is_bounded() {
        let huge = vec![b' '; MAX_TOC_BYTES + 1];
        assert_eq!(parse(&huge), Err(TocError::TooLarge));
        assert_eq!(parse(b"  // nothing\n"), Err(TocError::Empty));
        assert!(matches!(
            parse(b"TRACK AUDIO\nFILE \"a\" 99999999999999999999999\n"),
            Err(TocError::NumberTooLarge { .. })
        ));
    }

    #[test]
    fn no_corruption_of_a_real_toc_makes_the_parser_panic() {
        // Not fuzzing, which needs tooling this crate does not have yet, but
        // the same question asked deterministically: every truncation, and
        // every single-byte change to a few interesting values, of a TOC
        // cdrdao wrote.
        let original = include_bytes!("../../../fixtures/tool-output/cdrdao/readback-mixed.toc");
        for cut in 0..original.len() {
            let _ = parse(&original[..cut]);
        }
        for position in 0..original.len() {
            for replacement in [b'"', b'\\', b'#', b':', b'{', b'}', b'0', b'\n', 0xff, b'9'] {
                let mut bytes = original.to_vec();
                bytes[position] = replacement;
                if let Ok(toc) = parse(&bytes) {
                    let sizes = vec![10_000_000; toc.files.len()];
                    let _ = layout(&toc, &sizes);
                }
            }
        }
    }
}
