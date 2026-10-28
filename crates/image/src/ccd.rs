// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! CloneCD images: the `.ccd` descriptor, parsed, and its tracks laid out
//! over the `.img` beside it (docs/plans/ccd-images.md).
//!
//! A CloneCD image is three files named alike: `disc.ccd`, an INI-like text
//! file holding the disc's raw table of contents; `disc.img`, the main
//! channel as raw 2352-byte sectors; and `disc.sub`, the subchannel, 96 bytes
//! a sector with the P to W channels each 12 bytes in turn. The descriptor
//! is hostile input like a CUE sheet, and handled in the same steps:
//!
//! 1. [`parse`] reads the text into sections and values, every number range
//!    checked. No filesystem.
//! 2. The IMG and SUB are found among what was staged by the descriptor's
//!    own name ([`companion_names`]), through the same resolver a CUE sheet's
//!    files go through: nothing the text says is ever opened.
//! 3. [`layout`] turns the table of contents and the files' sizes into the
//!    track layout every descriptor produces.
//!
//! # The format
//!
//! No specification was ever published. What is followed here is what
//! CloneCD writes as its open-source readers take it (libmirage's CCD
//! parser and GNU ccd2cue, read to learn the format; no code is taken from
//! either):
//!
//! - `[Entry N]` sections are raw TOC entries. `Point` 1 to 99 starts a
//!   track at `PLBA`, counted from the disc's start; `0xA0` opens a session,
//!   `0xA1` names its last track, `0xA2` is its lead-out. `Point`, `ADR` and
//!   `Control` are written in hex, the rest in decimal.
//! - `[TRACK N]` gives `MODE` (0 audio, 1 and 2 the data modes) and the
//!   track's `INDEX` points, relative to the session's start; a track's
//!   pregap is `INDEX 1` less `INDEX 0`.
//! - The IMG holds the tracks' sectors back to back from the first track's
//!   first sector: no lead-in, lead-out or generated pregap.
//! - `MODE` is not always right in real images, so the layout reads each
//!   track's first sector of `INDEX 1` (the sync pattern and mode byte)
//!   through a caller's reader, and says when the descriptor disagrees.
//!
//! # What the layout claims
//!
//! A single-session disc, unscrambled. A multi-session image, or one whose
//! data tracks are stored scrambled, is still imported byte for byte, but no
//! layout is claimed for it: where a second session's tracks sit in the IMG
//! has not been checked against a real image, and scrambled sectors are not
//! the disc's data.

use std::collections::{BTreeMap, BTreeSet};

use tangible_domain::cd::{self, SampleByteOrder, TrackFlag};

use crate::cue::{CdLayout, TrackLayout};

/// Largest descriptor this parser will read. A 99-track CCD is a few
/// kilobytes; CD-Text adds at most a few hundred more.
pub const MAX_CCD_BYTES: usize = 1 << 20;

/// Longest line accepted.
const MAX_LINE_BYTES: usize = 4096;

/// Most lines read.
const MAX_LINES: usize = 8192;

/// Most sections read.
const MAX_SECTIONS: usize = 1000;

/// Most TOC entries kept. A disc's TOC fits in a few hundred.
const MAX_ENTRIES: usize = 4096;

/// Most CD-Text packs counted.
const MAX_CDTEXT_PACKS: usize = 8192;

/// Bytes in one raw sector of the IMG.
pub const SECTOR_BYTES: u64 = 2352;

/// Bytes of subchannel per sector in the SUB.
pub const SUBCHANNEL_BYTES: u64 = 96;

/// How a manifest names the SUB's layout: 96 bytes a sector, the P to W
/// channels each 12 bytes in turn, not interleaved.
pub const SUBCHANNEL_REPRESENTATION: &str = "pw96_linear";

/// The last LBA a CD can address: 99 minutes, 59 seconds, 74 frames.
const MAX_LBA: u64 = 99 * 60 * 75 + 59 * 75 + 74;

/// The sync pattern that opens every data sector.
const SYNC: [u8; 12] = [
    0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0,
];

/// Bytes of a sector's start the layout reads to tell its mode.
pub const HEADER_BYTES: usize = 16;

/// The point of the entry that opens a session.
const POINT_FIRST_TRACK: u8 = 0xA0;

/// The point of the entry that names a session's last track.
const POINT_LAST_TRACK: u8 = 0xA1;

/// The point of the entry that places a session's lead-out.
const POINT_LEAD_OUT: u8 = 0xA2;

// --- the document ------------------------------------------------------------------

/// A minute, second and frame, as a TOC entry writes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EntryMsf {
    /// Minutes, 0 to 99.
    pub minute: u8,
    /// Seconds, 0 to 59.
    pub second: u8,
    /// Frames, 0 to 74.
    pub frame: u8,
}

/// One raw TOC entry: an `[Entry N]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CcdEntry {
    /// The section's number.
    pub number: u32,
    /// The session it belongs to.
    pub session: u32,
    /// What it describes: a track 1 to 99, or 0xA0, 0xA1, 0xA2 and others.
    pub point: u8,
    /// The kind of Q data, four bits.
    pub adr: u8,
    /// The control bits: 0x04 data, 0x02 copy permitted, 0x01 pre-emphasis,
    /// 0x08 four channels.
    pub control: u8,
    /// The track number field, 0 in a lead-in entry.
    pub track_no: u32,
    /// The absolute time where the entry was read.
    pub a_time: EntryMsf,
    /// That time as an LBA, which may be negative.
    pub alba: i64,
    /// The zero field.
    pub zero: u32,
    /// The time the entry points at.
    pub p_time: EntryMsf,
    /// That time as an LBA, from the disc's start.
    pub plba: u64,
}

/// What a `[TRACK N]` section says.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CcdTrack {
    /// The track's number.
    pub number: u32,
    /// The mode it declares: 0 audio, 1 mode 1, 2 mode 2.
    pub mode: Option<u8>,
    /// Where its pregap starts, from the session's start.
    pub index0: Option<u64>,
    /// Where it starts proper, from the session's start.
    pub index1: Option<u64>,
    /// Its ISRC as written, checked only when laid out.
    pub isrc: Option<String>,
}

/// A parsed descriptor.
#[derive(Debug, Clone, PartialEq)]
pub struct CcdDocument {
    /// `[CloneCD] Version`, when given.
    pub version: Option<u32>,
    /// `[Disc] TocEntries`, when given.
    pub toc_entries: Option<u32>,
    /// `[Disc] Sessions`, when given.
    pub sessions: Option<u32>,
    /// `[Disc] DataTracksScrambled`, when given.
    pub data_tracks_scrambled: Option<bool>,
    /// `[Disc] CATALOG` as written, checked only when laid out.
    pub catalog: Option<String>,
    /// The TOC entries, in the order of their section numbers.
    pub entries: Vec<CcdEntry>,
    /// The `[TRACK N]` sections, by number.
    pub tracks: BTreeMap<u32, CcdTrack>,
    /// CD-Text packs present, counted and checked, not decoded.
    pub cdtext_packs: usize,
    /// What was seen that does not stop the parse.
    pub warnings: Vec<CcdWarning>,
    /// Why this is believed to be a CloneCD descriptor.
    pub evidence: Vec<String>,
    /// How sure the parse is that it is one.
    pub confidence: f32,
}

/// Something in a descriptor that does not stop it being read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CcdWarning {
    /// The file was not UTF-8; it was read lossily.
    #[error("the descriptor is not UTF-8 text; it was read lossily")]
    NonUtf8Text,
    /// A section this parser does not know, whose keys were not read.
    #[error("line {line}: the section [{name}] is not one CloneCD writes, and was skipped")]
    UnknownSection {
        /// Where.
        line: usize,
        /// Its name as written, cut short.
        name: String,
    },
    /// A key this parser does not know in a known section.
    #[error("line {line}: {key} is not a key CloneCD writes in [{section}], and was skipped")]
    UnknownKey {
        /// Where.
        line: usize,
        /// The section.
        section: String,
        /// The key as written, cut short.
        key: String,
    },
    /// `[Disc] TocEntries` disagrees with the entries present.
    #[error("[Disc] says {declared} TOC entries and there are {present}")]
    EntryCountMismatch {
        /// What it says.
        declared: u32,
        /// What there are.
        present: usize,
    },
    /// A track's declared mode disagrees with its first sector.
    #[error(
        "track {track} declares {declared} but its first sector is {found}; the sector is believed"
    )]
    ModeMismatch {
        /// The track.
        track: u32,
        /// What `[TRACK]` or the control bits say.
        declared: String,
        /// What the sector says.
        found: String,
    },
    /// The IMG holds more sectors than the tracks account for.
    #[error("the image holds {extra} sectors past the lead-out, which belong to no track")]
    ExtraSectors {
        /// How many.
        extra: u64,
    },
    /// No SUB beside the IMG.
    #[error("there is no subchannel file beside the image, so none is kept")]
    NoSubchannel,
    /// `0xA0` or `0xA1` names a track other than the first or last present.
    #[error("the TOC names track {declared} as the session's {which} track, and it is {present}")]
    FirstOrLastMismatch {
        /// `first` or `last`.
        which: &'static str,
        /// What the entry says.
        declared: u32,
        /// What the tracks are.
        present: u32,
    },
    /// A malformed catalogue number, dropped.
    #[error("the catalogue number {0:?} is not thirteen digits, and was left out")]
    MalformedCatalog(String),
    /// A malformed ISRC, dropped.
    #[error("track {track}'s ISRC {isrc:?} is not well formed, and was left out")]
    MalformedIsrc {
        /// The track.
        track: u32,
        /// The ISRC as written.
        isrc: String,
    },
    /// A `[TRACK N]` with no TOC entry.
    #[error("[TRACK {0}] has no entry in the table of contents, and was not used")]
    TrackWithoutEntry(u32),
}

/// Why a descriptor could not be read at all.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CcdError {
    /// Nothing in it.
    #[error("the file is empty")]
    Empty,
    /// Larger than any descriptor.
    #[error("the file is larger than {MAX_CCD_BYTES} bytes")]
    TooLarge,
    /// A line past the limit.
    #[error("line {line} is longer than {MAX_LINE_BYTES} bytes")]
    LineTooLong {
        /// Which.
        line: usize,
    },
    /// More lines, sections or entries than a descriptor has.
    #[error("the file holds more {what} than a CloneCD descriptor does")]
    TooMany {
        /// Lines, sections, entries or CD-Text packs.
        what: &'static str,
    },
    /// Not CloneCD's at all.
    #[error("there is no [CloneCD] section and no TOC entries; it is not a CloneCD descriptor")]
    NotCcd,
    /// A line that is neither a section, a key nor blank.
    #[error("line {line}: {reason}")]
    Malformed {
        /// Where.
        line: usize,
        /// What is wrong.
        reason: &'static str,
    },
    /// A key before any section.
    #[error("line {line}: a value before any section")]
    OutsideSection {
        /// Where.
        line: usize,
    },
    /// The same section twice.
    #[error("line {line}: [{name}] appears twice")]
    DuplicateSection {
        /// Where.
        line: usize,
        /// Which.
        name: String,
    },
    /// The same key twice in one section.
    #[error("line {line}: {key} appears twice in its section")]
    DuplicateKey {
        /// Where.
        line: usize,
        /// Which.
        key: String,
    },
    /// A value that is not a number, or not one in range.
    #[error("line {line}: {key} = {value:?} is not {expected}")]
    BadValue {
        /// Where.
        line: usize,
        /// The key.
        key: String,
        /// The value, cut short.
        value: String,
        /// What it should be.
        expected: &'static str,
    },
    /// An entry missing a field the layout needs.
    #[error("[Entry {entry}] has no {key}")]
    MissingKey {
        /// Which entry.
        entry: u32,
        /// The key.
        key: &'static str,
    },
}

/// Text cut short for a message.
fn short(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(64).collect()
}

/// A section, as named in its header.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Section {
    CloneCd,
    Disc,
    Session(u32),
    Entry(u32),
    Track(u32),
    CdText,
    Unknown,
}

impl Section {
    fn name(&self) -> String {
        match self {
            Self::CloneCd => "CloneCD".to_owned(),
            Self::Disc => "Disc".to_owned(),
            Self::Session(n) => format!("Session {n}"),
            Self::Entry(n) => format!("Entry {n}"),
            Self::Track(n) => format!("TRACK {n}"),
            Self::CdText => "CDText".to_owned(),
            Self::Unknown => "unknown".to_owned(),
        }
    }
}

/// A section header's name and number, read loosely as CloneCD's readers
/// do: any case, optional space before the number.
fn section_of(header: &str, line: usize) -> Result<Option<Section>, CcdError> {
    let lowered = header.trim().to_ascii_lowercase();
    let numbered = |prefix: &str| -> Result<Option<u32>, CcdError> {
        let Some(rest) = lowered.strip_prefix(prefix) else {
            return Ok(None);
        };
        let digits = rest.trim();
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) || digits.len() > 4 {
            return Err(CcdError::Malformed {
                line,
                reason: "a section number is not a number",
            });
        }
        digits.parse().map(Some).map_err(|_| CcdError::Malformed {
            line,
            reason: "a section number is not a number",
        })
    };
    Ok(Some(match lowered.as_str() {
        "clonecd" => Section::CloneCd,
        "disc" => Section::Disc,
        "cdtext" => Section::CdText,
        _ => {
            if let Some(n) = numbered("session")? {
                Section::Session(n)
            } else if let Some(n) = numbered("entry")? {
                Section::Entry(n)
            } else if let Some(n) = numbered("track")? {
                Section::Track(n)
            } else {
                return Ok(None);
            }
        }
    }))
}

/// A number as CloneCD writes it: decimal, or hex after `0x`.
fn number(text: &str, hex_allowed: bool) -> Option<i64> {
    let text = text.trim();
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let value = if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        if !hex_allowed || hex.is_empty() || hex.len() > 8 {
            return None;
        }
        i64::from_str_radix(hex, 16).ok()?
    } else {
        if digits.is_empty() || digits.len() > 10 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.parse::<i64>().ok()?
    };
    Some(if negative { -value } else { value })
}

/// What one key may hold.
#[derive(Clone, Copy)]
struct Range {
    min: i64,
    max: i64,
    hex: bool,
    expected: &'static str,
}

const fn range(min: i64, max: i64, expected: &'static str) -> Range {
    Range {
        min,
        max,
        hex: false,
        expected,
    }
}

const fn hex_range(min: i64, max: i64, expected: &'static str) -> Range {
    Range {
        min,
        max,
        hex: true,
        expected,
    }
}

/// The keys an `[Entry]` holds and what each may be.
fn entry_key(key: &str) -> Option<Range> {
    #[allow(clippy::cast_possible_wrap)]
    let lba = MAX_LBA as i64;
    Some(match key {
        "session" => range(1, 99, "a session from 1 to 99"),
        "point" => hex_range(0, 0xFF, "a point from 0 to 0xFF"),
        "adr" => hex_range(0, 0x0F, "an ADR from 0 to 0x0F"),
        "control" => hex_range(0, 0x0F, "control bits from 0 to 0x0F"),
        "trackno" => range(0, 99, "a track number from 0 to 99"),
        "amin" | "pmin" => range(0, 99, "minutes from 0 to 99"),
        // A session's opening entry writes its disc type in PSec: 0x00, 0x10
        // or 0x20, in decimal.
        "asec" | "psec" | "zero" => range(0, 255, "a value from 0 to 255"),
        "aframe" | "pframe" => range(0, 74, "frames from 0 to 74"),
        "alba" => range(-450_150, lba, "an LBA"),
        "plba" => range(0, lba, "an LBA from 0 to the end of a CD"),
        _ => return None,
    })
}

/// One section's keys as read, by lower-cased name without spaces.
type Values = BTreeMap<String, (usize, String)>;

/// Read a descriptor.
///
/// # Errors
///
/// [`CcdError`] for a file that is not one, or is past the bounds a real
/// one keeps to. The bytes are still imported; what is refused is the claim
/// to understand them.
pub fn parse(bytes: &[u8]) -> Result<CcdDocument, CcdError> {
    if bytes.is_empty() {
        return Err(CcdError::Empty);
    }
    if bytes.len() > MAX_CCD_BYTES {
        return Err(CcdError::TooLarge);
    }
    let mut warnings = Vec::new();
    let text = if let Ok(text) = std::str::from_utf8(bytes) {
        std::borrow::Cow::Borrowed(text)
    } else {
        warnings.push(CcdWarning::NonUtf8Text);
        String::from_utf8_lossy(bytes)
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);

    let sections = read_sections(text, &mut warnings)?;

    let mut document = CcdDocument {
        version: None,
        toc_entries: None,
        sessions: None,
        data_tracks_scrambled: None,
        catalog: None,
        entries: Vec::new(),
        tracks: BTreeMap::new(),
        cdtext_packs: 0,
        warnings,
        evidence: Vec::new(),
        confidence: 0.0,
    };
    let mut saw_clonecd = false;
    for (section, _, values) in sections {
        match section {
            Section::CloneCd => {
                saw_clonecd = true;
                read_clonecd(&values, &mut document)?;
            }
            Section::Disc => read_disc(&values, &mut document)?,
            Section::Session(_) => read_known(
                &values,
                &section,
                &["pregapmode", "pregapsubc"],
                &mut document,
            )?,
            Section::Entry(number) => {
                if document.entries.len() >= MAX_ENTRIES {
                    return Err(CcdError::TooMany {
                        what: "TOC entries",
                    });
                }
                let entry = read_entry(number, &values, &mut document)?;
                document.entries.push(entry);
            }
            Section::Track(number) => {
                let track = read_track(number, &values, &mut document)?;
                document.tracks.insert(number, track);
            }
            Section::CdText => read_cdtext(&values, &mut document)?,
            Section::Unknown => {}
        }
    }
    if !saw_clonecd && document.entries.is_empty() {
        return Err(CcdError::NotCcd);
    }
    document.entries.sort_by_key(|entry| entry.number);
    if let Some(declared) = document.toc_entries
        && usize::try_from(declared).ok() != Some(document.entries.len())
    {
        document.warnings.push(CcdWarning::EntryCountMismatch {
            declared,
            present: document.entries.len(),
        });
    }
    if saw_clonecd {
        document.evidence.push("a [CloneCD] section".to_owned());
    }
    document
        .evidence
        .push(format!("{} TOC entries", document.entries.len()));
    document.confidence = if saw_clonecd && !document.entries.is_empty() {
        0.95
    } else {
        0.8
    };
    Ok(document)
}

/// The sections of a descriptor's text, each with the line it starts on and
/// its keys.
fn read_sections(
    text: &str,
    warnings: &mut Vec<CcdWarning>,
) -> Result<Vec<(Section, usize, Values)>, CcdError> {
    let mut sections: Vec<(Section, usize, Values)> = Vec::new();
    let mut seen: BTreeSet<Section> = BTreeSet::new();
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        if line > MAX_LINES {
            return Err(CcdError::TooMany { what: "lines" });
        }
        if raw.len() > MAX_LINE_BYTES {
            return Err(CcdError::LineTooLong { line });
        }
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with(';') || trimmed.starts_with('#') {
            continue;
        }
        if let Some(inner) = trimmed.strip_prefix('[') {
            let Some(header) = inner.strip_suffix(']') else {
                return Err(CcdError::Malformed {
                    line,
                    reason: "a section header is not closed with ]",
                });
            };
            if sections.len() >= MAX_SECTIONS {
                return Err(CcdError::TooMany { what: "sections" });
            }
            let section = if let Some(section) = section_of(header, line)? {
                if !seen.insert(section.clone()) {
                    return Err(CcdError::DuplicateSection {
                        line,
                        name: section.name(),
                    });
                }
                section
            } else {
                warnings.push(CcdWarning::UnknownSection {
                    line,
                    name: short(header),
                });
                Section::Unknown
            };
            sections.push((section, line, Values::new()));
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            return Err(CcdError::Malformed {
                line,
                reason: "a line that is neither a [section] nor a key = value",
            });
        };
        let Some((_, _, values)) = sections.last_mut() else {
            return Err(CcdError::OutsideSection { line });
        };
        // "INDEX 1" and "INDEX1" are the same key; case does not matter.
        let key: String = key
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_ascii_lowercase();
        if key.is_empty() {
            return Err(CcdError::Malformed {
                line,
                reason: "a value with no key",
            });
        }
        if values
            .insert(key.clone(), (line, value.trim().to_owned()))
            .is_some()
        {
            return Err(CcdError::DuplicateKey {
                line,
                key: short(&key),
            });
        }
    }
    Ok(sections)
}

/// A value read as a number in `range`.
fn value_in(key: &str, (line, value): &(usize, String), range: Range) -> Result<i64, CcdError> {
    number(value, range.hex)
        .filter(|n| (range.min..=range.max).contains(n))
        .ok_or_else(|| CcdError::BadValue {
            line: *line,
            key: key.to_owned(),
            value: short(value),
            expected: range.expected,
        })
}

/// Warn about every key not in `known`.
fn read_known(
    values: &Values,
    section: &Section,
    known: &[&str],
    document: &mut CcdDocument,
) -> Result<(), CcdError> {
    for (key, (line, value)) in values {
        if known.contains(&key.as_str()) {
            // Read to check its shape; not otherwise used.
            value_in(
                key,
                &(*line, value.clone()),
                range(0, i64::from(u32::MAX), "a number"),
            )?;
        } else {
            document.warnings.push(CcdWarning::UnknownKey {
                line: *line,
                section: section.name(),
                key: short(key),
            });
        }
    }
    Ok(())
}

fn as_u32(value: i64) -> u32 {
    u32::try_from(value).unwrap_or(0)
}

fn as_u8(value: i64) -> u8 {
    u8::try_from(value).unwrap_or(0)
}

fn read_clonecd(values: &Values, document: &mut CcdDocument) -> Result<(), CcdError> {
    for (key, item) in values {
        match key.as_str() {
            "version" => {
                document.version = Some(as_u32(value_in(
                    key,
                    item,
                    range(1, 99, "a version number"),
                )?));
            }
            _ => document.warnings.push(CcdWarning::UnknownKey {
                line: item.0,
                section: Section::CloneCd.name(),
                key: short(key),
            }),
        }
    }
    Ok(())
}

fn read_disc(values: &Values, document: &mut CcdDocument) -> Result<(), CcdError> {
    for (key, item) in values {
        match key.as_str() {
            "tocentries" => {
                document.toc_entries = Some(as_u32(value_in(
                    key,
                    item,
                    range(0, 4096, "a count of TOC entries"),
                )?));
            }
            "sessions" => {
                document.sessions = Some(as_u32(value_in(
                    key,
                    item,
                    range(1, 99, "a count of sessions"),
                )?));
            }
            "datatracksscrambled" => {
                document.data_tracks_scrambled =
                    Some(value_in(key, item, range(0, 1, "0 or 1"))? == 1);
            }
            "cdtextlength" => {
                value_in(key, item, range(0, 1 << 20, "a length"))?;
            }
            "catalog" => {
                if item.1.len() > 64 {
                    return Err(CcdError::BadValue {
                        line: item.0,
                        key: key.clone(),
                        value: short(&item.1),
                        expected: "a catalogue number",
                    });
                }
                document.catalog = Some(item.1.clone());
            }
            _ => document.warnings.push(CcdWarning::UnknownKey {
                line: item.0,
                section: Section::Disc.name(),
                key: short(key),
            }),
        }
    }
    Ok(())
}

fn read_entry(
    number: u32,
    values: &Values,
    document: &mut CcdDocument,
) -> Result<CcdEntry, CcdError> {
    let mut read: BTreeMap<&'static str, i64> = BTreeMap::new();
    for (key, item) in values {
        let Some(range) = entry_key(key) else {
            document.warnings.push(CcdWarning::UnknownKey {
                line: item.0,
                section: Section::Entry(number).name(),
                key: short(key),
            });
            continue;
        };
        let name: &'static str = match key.as_str() {
            "session" => "Session",
            "point" => "Point",
            "adr" => "ADR",
            "control" => "Control",
            "trackno" => "TrackNo",
            "amin" => "AMin",
            "asec" => "ASec",
            "aframe" => "AFrame",
            "alba" => "ALBA",
            "zero" => "Zero",
            "pmin" => "PMin",
            "psec" => "PSec",
            "pframe" => "PFrame",
            _ => "PLBA",
        };
        read.insert(name, value_in(key, item, range)?);
    }
    let get = |key: &'static str| -> Result<i64, CcdError> {
        read.get(key)
            .copied()
            .ok_or(CcdError::MissingKey { entry: number, key })
    };
    let optional = |key: &'static str| read.get(key).copied().unwrap_or(0);
    let msf = |minute: &'static str, second: &'static str, frame: &'static str| EntryMsf {
        minute: as_u8(optional(minute)),
        second: as_u8(optional(second)),
        frame: as_u8(optional(frame)),
    };
    Ok(CcdEntry {
        number,
        session: as_u32(get("Session")?),
        point: as_u8(get("Point")?),
        adr: as_u8(optional("ADR")),
        control: as_u8(get("Control")?),
        track_no: as_u32(optional("TrackNo")),
        a_time: msf("AMin", "ASec", "AFrame"),
        alba: optional("ALBA"),
        zero: as_u32(optional("Zero")),
        p_time: msf("PMin", "PSec", "PFrame"),
        plba: u64::try_from(get("PLBA")?).unwrap_or(0),
    })
}

fn read_track(
    number: u32,
    values: &Values,
    document: &mut CcdDocument,
) -> Result<CcdTrack, CcdError> {
    if !(1..=99).contains(&number) {
        return Err(CcdError::BadValue {
            line: values.values().next().map_or(0, |(line, _)| *line),
            key: "TRACK".to_owned(),
            value: number.to_string(),
            expected: "a track from 1 to 99",
        });
    }
    #[allow(clippy::cast_possible_wrap)]
    let lba = MAX_LBA as i64;
    let mut track = CcdTrack {
        number,
        ..CcdTrack::default()
    };
    for (key, item) in values {
        match key.as_str() {
            "mode" => track.mode = Some(as_u8(value_in(key, item, range(0, 2, "a mode, 0 to 2"))?)),
            "index0" => {
                track.index0 =
                    Some(u64::try_from(value_in(key, item, range(0, lba, "an LBA"))?).unwrap_or(0));
            }
            "index1" => {
                track.index1 =
                    Some(u64::try_from(value_in(key, item, range(0, lba, "an LBA"))?).unwrap_or(0));
            }
            "isrc" => {
                if item.1.len() > 64 {
                    return Err(CcdError::BadValue {
                        line: item.0,
                        key: key.clone(),
                        value: short(&item.1),
                        expected: "an ISRC",
                    });
                }
                track.isrc = Some(item.1.clone());
            }
            // Further indexes are kept by nothing here.
            other if other.starts_with("index") => {
                value_in(key, item, range(0, lba, "an LBA"))?;
            }
            _ => document.warnings.push(CcdWarning::UnknownKey {
                line: item.0,
                section: Section::Track(number).name(),
                key: short(key),
            }),
        }
    }
    Ok(track)
}

fn read_cdtext(values: &Values, document: &mut CcdDocument) -> Result<(), CcdError> {
    for (key, (line, value)) in values {
        if key == "entries" {
            value_in(
                key,
                &(*line, value.clone()),
                range(0, 8192, "a count of packs"),
            )?;
            continue;
        }
        if key
            .strip_prefix("entry")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        {
            let hex: Vec<&str> = value.split_whitespace().collect();
            let well_formed = hex.len() == 18
                && hex
                    .iter()
                    .all(|byte| byte.len() == 2 && byte.bytes().all(|b| b.is_ascii_hexdigit()));
            if !well_formed {
                return Err(CcdError::BadValue {
                    line: *line,
                    key: key.clone(),
                    value: short(value),
                    expected: "a CD-Text pack: 18 bytes in hex",
                });
            }
            document.cdtext_packs += 1;
            if document.cdtext_packs > MAX_CDTEXT_PACKS {
                return Err(CcdError::TooMany {
                    what: "CD-Text packs",
                });
            }
            continue;
        }
        document.warnings.push(CcdWarning::UnknownKey {
            line: *line,
            section: Section::CdText.name(),
            key: short(key),
        });
    }
    Ok(())
}

// --- companions --------------------------------------------------------------------

/// The names the IMG and SUB beside a descriptor would have: its own name
/// with `img` and `sub` for its extension, as CloneCD writes them.
#[must_use]
pub fn companion_names(descriptor: &str) -> Option<(String, String)> {
    let (stem, extension) = descriptor.rsplit_once('.')?;
    if !extension.eq_ignore_ascii_case("ccd") || stem.is_empty() {
        return None;
    }
    Some((format!("{stem}.img"), format!("{stem}.sub")))
}

// --- layout ------------------------------------------------------------------------

/// Why a descriptor's tracks could not be laid out.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CcdLayoutError {
    /// More than one session.
    #[error("a CloneCD image of {sessions} sessions is not laid out yet")]
    MultiSession {
        /// How many.
        sessions: u32,
    },
    /// Data sectors stored scrambled, as read from the drive.
    #[error("the image stores its data tracks scrambled, which is not the disc's data")]
    Scrambled,
    /// No tracks in the TOC.
    #[error("the table of contents has no tracks")]
    NoTracks,
    /// No `0xA2` entry for the session.
    #[error("the table of contents has no lead-out")]
    NoLeadOut,
    /// The same track twice.
    #[error("track {0} appears twice in the table of contents")]
    DuplicateTrack(u32),
    /// A track that does not start after the one before, or its pregap.
    #[error("track {track}: {reason}")]
    BadPosition {
        /// The track.
        track: u32,
        /// What is wrong.
        reason: &'static str,
    },
    /// The IMG is not a whole number of sectors.
    #[error("the image is {bytes} bytes, not a whole number of 2352-byte sectors")]
    NotWholeSectors {
        /// Its size.
        bytes: u64,
    },
    /// The tracks reach past the IMG.
    #[error("the tracks need {needed} sectors and the image holds {present}")]
    PastImage {
        /// Sectors the tracks need.
        needed: u64,
        /// Sectors in the IMG.
        present: u64,
    },
    /// A SUB that is not 96 bytes for every sector of the IMG.
    #[error(
        "the subchannel file is {bytes} bytes; {expected} were expected, 96 for each of the image's sectors"
    )]
    SubchannelMismatch {
        /// Its size.
        bytes: u64,
        /// What it should be.
        expected: u64,
    },
}

/// A laid-out CloneCD image, and what laying it out observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CcdLayout {
    /// The tracks, in the shape every descriptor produces. Its one file is
    /// the IMG.
    pub layout: CdLayout,
    /// Whether a SUB of the right size is beside it.
    pub subchannel: bool,
    /// Observations that do not invalidate it.
    pub warnings: Vec<CcdWarning>,
}

/// A track's mode in words, as a CUE sheet would write it.
fn mode_text(mode: u8) -> &'static str {
    match mode {
        0 => "AUDIO",
        1 => "MODE1/2352",
        _ => "MODE2/2352",
    }
}

/// What a sector's first bytes say its mode is: a data mode after the sync
/// pattern, audio without one. `None` for a mode byte that is neither 1 nor
/// 2, which says nothing either way.
fn sector_mode(header: &[u8; HEADER_BYTES]) -> Option<u8> {
    if header[..12] != SYNC {
        return Some(0);
    }
    match header[15] {
        1 => Some(1),
        2 => Some(2),
        _ => None,
    }
}

/// Lay out a parsed descriptor over an IMG of `img_bytes` and, when there is
/// one, a SUB of `sub_bytes`. `header` answers the first [`HEADER_BYTES`] of
/// the IMG sector at a given sector offset, or `None` when it cannot.
///
/// # Errors
///
/// [`CcdLayoutError`] when the tracks cannot be placed exactly. The image is
/// still imported; what is refused is the claim to know where its tracks are.
#[allow(clippy::too_many_lines)]
pub fn layout(
    document: &CcdDocument,
    img_bytes: u64,
    sub_bytes: Option<u64>,
    header: &mut dyn FnMut(u64) -> Option<[u8; HEADER_BYTES]>,
) -> Result<CcdLayout, CcdLayoutError> {
    let sessions: BTreeSet<u32> = document.entries.iter().map(|e| e.session).collect();
    let session_count = document
        .sessions
        .unwrap_or(1)
        .max(u32::try_from(sessions.len()).unwrap_or(u32::MAX));
    if session_count > 1 {
        return Err(CcdLayoutError::MultiSession {
            sessions: session_count,
        });
    }
    if document.data_tracks_scrambled == Some(true) {
        return Err(CcdLayoutError::Scrambled);
    }
    if !img_bytes.is_multiple_of(SECTOR_BYTES) {
        return Err(CcdLayoutError::NotWholeSectors { bytes: img_bytes });
    }
    let img_sectors = img_bytes / SECTOR_BYTES;
    let mut warnings = Vec::new();
    let subchannel = match sub_bytes {
        None => {
            warnings.push(CcdWarning::NoSubchannel);
            false
        }
        Some(bytes) => {
            let expected = img_sectors.saturating_mul(SUBCHANNEL_BYTES);
            if bytes != expected {
                return Err(CcdLayoutError::SubchannelMismatch { bytes, expected });
            }
            true
        }
    };

    let mut tracks: BTreeMap<u8, &CcdEntry> = BTreeMap::new();
    let mut lead_out = None;
    let mut first = None;
    let mut last = None;
    for entry in &document.entries {
        match entry.point {
            1..=99 => match tracks.entry(entry.point) {
                std::collections::btree_map::Entry::Occupied(_) => {
                    return Err(CcdLayoutError::DuplicateTrack(u32::from(entry.point)));
                }
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(entry);
                }
            },
            POINT_FIRST_TRACK => first = Some(entry.p_time.minute),
            POINT_LAST_TRACK => last = Some(entry.p_time.minute),
            POINT_LEAD_OUT => lead_out = Some(entry.plba),
            _ => {}
        }
    }
    let lead_out = lead_out.ok_or(CcdLayoutError::NoLeadOut)?;
    let (Some(&first_present), Some(&last_present)) = (tracks.keys().next(), tracks.keys().last())
    else {
        return Err(CcdLayoutError::NoTracks);
    };
    for (which, declared, present) in [
        ("first", first, first_present),
        ("last", last, last_present),
    ] {
        if let Some(declared) = declared
            && declared != present
        {
            warnings.push(CcdWarning::FirstOrLastMismatch {
                which,
                declared: u32::from(declared),
                present: u32::from(present),
            });
        }
    }
    for number in document.tracks.keys() {
        if !u8::try_from(*number).is_ok_and(|n| tracks.contains_key(&n)) {
            warnings.push(CcdWarning::TrackWithoutEntry(*number));
        }
    }

    // Each track's pregap, from its INDEX points: the first track's INDEX 0
    // is the session's start when not written.
    let pregap = |point: u8, first_in_session: bool| -> Result<u64, CcdLayoutError> {
        let Some(info) = document.tracks.get(&u32::from(point)) else {
            return Ok(0);
        };
        match (info.index0, info.index1) {
            (Some(zero), Some(one)) => one.checked_sub(zero).ok_or(CcdLayoutError::BadPosition {
                track: u32::from(point),
                reason: "its INDEX 1 is before its INDEX 0",
            }),
            (None, Some(one)) if first_in_session => Ok(one),
            _ => Ok(0),
        }
    };
    let catalog = document.catalog.as_ref().and_then(|value| {
        if cd::is_catalog_number(value) {
            Some(value.clone())
        } else {
            warnings.push(CcdWarning::MalformedCatalog(short(value)));
            None
        }
    });

    let points: Vec<u8> = tracks.keys().copied().collect();
    let mut starts = Vec::with_capacity(points.len());
    for (at, point) in points.iter().enumerate() {
        let gap = pregap(*point, at == 0)?;
        let plba = tracks[point].plba;
        let start = plba.checked_sub(gap).ok_or(CcdLayoutError::BadPosition {
            track: u32::from(*point),
            reason: "its pregap reaches before the disc's start",
        })?;
        starts.push((start, gap));
    }
    let origin = starts[0].0;
    let mut laid = Vec::with_capacity(points.len());
    for (at, point) in points.iter().enumerate() {
        let (start, gap) = starts[at];
        let end = starts.get(at + 1).map_or(lead_out, |(next, _)| *next);
        if end <= start || start < origin {
            return Err(CcdLayoutError::BadPosition {
                track: u32::from(*point),
                reason: "it does not end after it starts",
            });
        }
        let entry = tracks[point];
        let offset = start - origin;
        let sectors = end - start;
        if gap >= sectors {
            return Err(CcdLayoutError::BadPosition {
                track: u32::from(*point),
                reason: "its INDEX 1 is not inside it",
            });
        }
        if offset + sectors > img_sectors {
            return Err(CcdLayoutError::PastImage {
                needed: offset + sectors,
                present: img_sectors,
            });
        }

        // The sector believed over the descriptor, which real images get
        // wrong; the descriptor when the sector says nothing.
        let declared = document
            .tracks
            .get(&u32::from(*point))
            .and_then(|info| info.mode)
            // The control bits say data or audio, and data is mode 1
            // unless the track says otherwise.
            .unwrap_or_else(|| u8::from(entry.control & 0x04 != 0));
        let found = header(offset + gap).as_ref().and_then(sector_mode);
        let mode = match found {
            Some(found) if found != declared => {
                warnings.push(CcdWarning::ModeMismatch {
                    track: u32::from(*point),
                    declared: mode_text(declared).to_owned(),
                    found: mode_text(found).to_owned(),
                });
                found
            }
            Some(found) => found,
            None => declared,
        };
        let audio = mode == 0;
        let isrc = document
            .tracks
            .get(&u32::from(*point))
            .and_then(|info| info.isrc.as_ref())
            .and_then(|value| {
                if cd::is_isrc(value) {
                    Some(value.clone())
                } else {
                    warnings.push(CcdWarning::MalformedIsrc {
                        track: u32::from(*point),
                        isrc: short(value),
                    });
                    None
                }
            });
        let mut flags = Vec::new();
        if entry.control & 0x02 != 0 {
            flags.push(TrackFlag::DigitalCopyPermitted);
        }
        if audio && entry.control & 0x08 != 0 {
            flags.push(TrackFlag::FourChannel);
        }
        if audio && entry.control & 0x01 != 0 {
            flags.push(TrackFlag::PreEmphasis);
        }
        let mut indexes = Vec::with_capacity(2);
        if gap > 0 {
            indexes.push((0, 0));
        }
        indexes.push((1, gap));
        laid.push(TrackLayout {
            number: u32::from(*point),
            session: 1,
            mode: mode_text(mode).to_owned(),
            file: 0,
            file_offset_bytes: offset * SECTOR_BYTES,
            start_lba: offset,
            sector_count: sectors,
            pregap_sectors: gap,
            indexes,
            isrc,
            // Raw sectors, as a CUE sheet's BINARY file: audio samples least
            // significant byte first.
            sample_byte_order: Some(SampleByteOrder::LittleEndian),
            flags,
        });
    }
    let used = lead_out - origin;
    if img_sectors > used {
        warnings.push(CcdWarning::ExtraSectors {
            extra: img_sectors - used,
        });
    }
    Ok(CcdLayout {
        layout: CdLayout {
            catalog,
            tracks: laid,
            session_count: 1,
            warnings: Vec::new(),
        },
        subchannel,
        warnings,
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/../../fixtures/images/ccd/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    /// A data sector's first bytes, of `mode`.
    fn data_header(mode: u8) -> [u8; HEADER_BYTES] {
        let mut header = [0_u8; HEADER_BYTES];
        header[..12].copy_from_slice(&SYNC);
        header[15] = mode;
        header
    }

    /// Sector headers: mode 1 data at the sectors listed, audio elsewhere.
    fn sectors(data: &'static [u64]) -> impl FnMut(u64) -> Option<[u8; HEADER_BYTES]> {
        move |at| {
            Some(if data.contains(&at) {
                data_header(1)
            } else {
                [0x55; HEADER_BYTES]
            })
        }
    }

    fn whole(sectors: u64) -> (u64, Option<u64>) {
        (sectors * SECTOR_BYTES, Some(sectors * SUBCHANNEL_BYTES))
    }

    #[test]
    fn a_recorded_data_disc_reads_and_lays_out_as_one_track() {
        let document = parse(&fixture("data-only.ccd")).unwrap();
        assert_eq!(document.version, Some(3));
        assert_eq!(document.sessions, Some(1));
        assert_eq!(document.data_tracks_scrambled, Some(false));
        assert_eq!(document.entries.len(), 4);
        assert!(document.warnings.is_empty(), "{:?}", document.warnings);
        assert!(document.confidence > 0.9);
        let (img, sub) = whole(300);
        let laid = layout(&document, img, sub, &mut sectors(&[0])).unwrap();
        assert!(laid.subchannel);
        assert!(laid.warnings.is_empty(), "{:?}", laid.warnings);
        let [track] = laid.layout.tracks.as_slice() else {
            panic!("{:?}", laid.layout.tracks);
        };
        assert_eq!(track.number, 1);
        assert_eq!(track.mode, "MODE1/2352");
        assert_eq!((track.start_lba, track.sector_count), (0, 300));
        assert_eq!(track.pregap_sectors, 0);
        assert_eq!(track.indexes, [(1, 0)]);
        assert_eq!(laid.layout.session_count, 1);
    }

    #[test]
    fn a_mixed_mode_disc_lays_out_with_its_pregap_codes_and_flags() {
        let document = parse(&fixture("mixed-mode.ccd")).unwrap();
        let (img, sub) = whole(750);
        let laid = layout(&document, img, sub, &mut sectors(&[0])).unwrap();
        assert!(laid.warnings.is_empty(), "{:?}", laid.warnings);
        assert_eq!(laid.layout.catalog.as_deref(), Some("0123456789012"));
        let tracks = &laid.layout.tracks;
        let summary: Vec<_> = tracks
            .iter()
            .map(|t| {
                (
                    t.number,
                    t.mode.as_str(),
                    t.file_offset_bytes,
                    t.start_lba,
                    t.sector_count,
                    t.pregap_sectors,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (1, "MODE1/2352", 0, 0, 300, 0),
                (2, "AUDIO", 300 * SECTOR_BYTES, 300, 300, 150),
                (3, "AUDIO", 600 * SECTOR_BYTES, 600, 150, 0),
            ]
        );
        assert_eq!(tracks[1].indexes, [(0, 0), (1, 150)]);
        assert_eq!(tracks[1].isrc.as_deref(), Some("USRC17607839"));
        assert_eq!(tracks[2].flags, [TrackFlag::DigitalCopyPermitted]);
        assert!(tracks[0].flags.is_empty());
        assert_eq!(laid.layout.total_sectors(), 750);
    }

    /// GNU ccd2cue's sheet for a fixture, laid out by the CUE parser over an
    /// image of `sectors`.
    fn ccd2cue(name: &str, sectors: u64) -> CdLayout {
        let text = std::fs::read(format!(
            "{}/../../fixtures/tool-output/ccd2cue/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let sheet = crate::cue::parse(&text).unwrap();
        crate::cue::layout(&sheet, &[sectors * SECTOR_BYTES]).unwrap()
    }

    #[test]
    fn an_independent_reader_lays_the_fixtures_out_the_same() {
        for (name, count) in [("data-only", 300), ("mixed-mode", 750)] {
            let document = parse(&fixture(&format!("{name}.ccd"))).unwrap();
            let (img, sub) = whole(count);
            let ours = layout(&document, img, sub, &mut sectors(&[0]))
                .unwrap()
                .layout;
            let theirs = ccd2cue(&format!("{name}.cue"), count);
            assert_eq!(ours.catalog, theirs.catalog, "{name}");
            assert_eq!(ours.tracks.len(), theirs.tracks.len(), "{name}");
            for (ours, theirs) in ours.tracks.iter().zip(&theirs.tracks) {
                let shape = |t: &TrackLayout| {
                    (
                        t.number,
                        t.mode.clone(),
                        t.file_offset_bytes,
                        t.start_lba,
                        t.sector_count,
                        t.pregap_sectors,
                        t.indexes.clone(),
                        t.isrc.clone(),
                    )
                };
                assert_eq!(shape(ours), shape(theirs), "{name} track {}", ours.number);
            }
        }
    }

    #[test]
    fn the_first_sector_is_believed_over_the_descriptor() {
        let document = parse(&fixture("data-only.ccd")).unwrap();
        let (img, sub) = whole(300);
        let laid = layout(&document, img, sub, &mut |_| Some(data_header(2))).unwrap();
        assert_eq!(laid.layout.tracks[0].mode, "MODE2/2352");
        assert!(matches!(
            laid.warnings.as_slice(),
            [CcdWarning::ModeMismatch { track: 1, .. }]
        ));
        // Nothing read: the descriptor is taken.
        let laid = layout(&document, img, sub, &mut |_| None).unwrap();
        assert_eq!(laid.layout.tracks[0].mode, "MODE1/2352");
        assert!(laid.warnings.is_empty());
        // A sector whose mode byte says nothing: the descriptor too.
        let laid = layout(&document, img, sub, &mut |_| Some(data_header(0))).unwrap();
        assert_eq!(laid.layout.tracks[0].mode, "MODE1/2352");
    }

    #[test]
    fn the_files_sizes_must_fit_the_table_of_contents() {
        let document = parse(&fixture("data-only.ccd")).unwrap();
        let mut read = sectors(&[0]);
        // No SUB: laid out, said so.
        let laid = layout(&document, 300 * SECTOR_BYTES, None, &mut read).unwrap();
        assert!(!laid.subchannel);
        assert_eq!(laid.warnings, [CcdWarning::NoSubchannel]);
        // More sectors than the tracks use.
        let (img, sub) = whole(310);
        let laid = layout(&document, img, sub, &mut read).unwrap();
        assert_eq!(laid.warnings, [CcdWarning::ExtraSectors { extra: 10 }]);
        for (img, sub, expected) in [
            (
                300 * SECTOR_BYTES + 1,
                None,
                CcdLayoutError::NotWholeSectors {
                    bytes: 300 * SECTOR_BYTES + 1,
                },
            ),
            (
                299 * SECTOR_BYTES,
                Some(299 * SUBCHANNEL_BYTES),
                CcdLayoutError::PastImage {
                    needed: 300,
                    present: 299,
                },
            ),
            (
                300 * SECTOR_BYTES,
                Some(300 * SUBCHANNEL_BYTES - 1),
                CcdLayoutError::SubchannelMismatch {
                    bytes: 300 * SUBCHANNEL_BYTES - 1,
                    expected: 300 * SUBCHANNEL_BYTES,
                },
            ),
        ] {
            assert_eq!(
                layout(&document, img, sub, &mut read).unwrap_err(),
                expected
            );
        }
    }

    #[test]
    fn a_multi_session_or_scrambled_image_is_read_but_not_laid_out() {
        let document = parse(&fixture("multi-session.ccd")).unwrap();
        assert_eq!(document.sessions, Some(2));
        let (img, sub) = whole(600);
        assert_eq!(
            layout(&document, img, sub, &mut sectors(&[])).unwrap_err(),
            CcdLayoutError::MultiSession { sessions: 2 }
        );
        let text = String::from_utf8(fixture("data-only.ccd"))
            .unwrap()
            .replace("DataTracksScrambled=0", "DataTracksScrambled=1");
        let document = parse(text.as_bytes()).unwrap();
        let (img, sub) = whole(300);
        assert_eq!(
            layout(&document, img, sub, &mut sectors(&[0])).unwrap_err(),
            CcdLayoutError::Scrambled
        );
    }

    #[test]
    fn readers_spellings_are_taken_as_clonecd_readers_take_them() {
        let text = "\u{feff}; a comment\r\n[clonecd]\r\nversion = 2\r\n[ENTRY 0]\r\nSession=1\r\n\
                    Point=0XA2\r\nControl=0x04\r\nPLBA=150\r\n[Entry1]\r\nSession=1\r\nPoint=1\r\n\
                    Control=0x04\r\nPLBA=0\r\n[Track 1]\r\nmode=1\r\nINDEX1=0\r\n";
        let document = parse(text.as_bytes()).unwrap();
        assert_eq!(document.version, Some(2));
        assert_eq!(document.entries[0].point, POINT_LEAD_OUT);
        assert_eq!(document.tracks[&1].index1, Some(0));
        let laid = layout(&document, 150 * SECTOR_BYTES, None, &mut |_| None).unwrap();
        assert_eq!(laid.layout.tracks[0].sector_count, 150);
    }

    #[test]
    fn what_clonecd_does_not_write_is_warned_about_and_skipped() {
        let mut text = String::from_utf8(fixture("data-only.ccd")).unwrap();
        text.push_str("[Mystery]\r\nAnything=1\r\n[TRACK 7]\r\nMODE=1\r\nPEAK=3\r\n");
        let text = text.replace("Version=3", "Version=3\r\nEdition=Gold");
        let document = parse(text.as_bytes()).unwrap();
        assert!(
            document
                .warnings
                .iter()
                .any(|w| matches!(w, CcdWarning::UnknownSection { name, .. } if name == "Mystery"))
        );
        assert!(
            document
                .warnings
                .iter()
                .any(|w| matches!(w, CcdWarning::UnknownKey { key, .. } if key == "edition"))
        );
        assert!(
            document
                .warnings
                .iter()
                .any(|w| matches!(w, CcdWarning::UnknownKey { key, .. } if key == "peak"))
        );
        let (img, sub) = whole(300);
        let laid = layout(&document, img, sub, &mut sectors(&[0])).unwrap();
        assert_eq!(laid.warnings, [CcdWarning::TrackWithoutEntry(7)]);
    }

    #[test]
    fn malformed_codes_are_left_out_saying_so() {
        let text = String::from_utf8(fixture("mixed-mode.ccd"))
            .unwrap()
            .replace("CATALOG=0123456789012", "CATALOG=12345")
            .replace("ISRC=USRC17607839", "ISRC=not-an-isrc");
        let document = parse(text.as_bytes()).unwrap();
        let (img, sub) = whole(750);
        let laid = layout(&document, img, sub, &mut sectors(&[0])).unwrap();
        assert_eq!(laid.layout.catalog, None);
        assert_eq!(laid.layout.tracks[1].isrc, None);
        assert!(
            laid.warnings
                .contains(&CcdWarning::MalformedCatalog("12345".to_owned()))
        );
        assert!(
            laid.warnings
                .iter()
                .any(|w| matches!(w, CcdWarning::MalformedIsrc { track: 2, .. }))
        );
    }

    #[test]
    fn a_table_of_contents_that_cannot_be_laid_out_says_why() {
        let base = String::from_utf8(fixture("mixed-mode.ccd")).unwrap();
        let (img, sub) = whole(750);
        let fails = |text: &str| {
            let document = parse(text.as_bytes()).unwrap();
            layout(&document, img, sub, &mut sectors(&[0])).unwrap_err()
        };
        assert_eq!(
            fails(&base.replace("Point=0xa2", "Point=0xb0")),
            CcdLayoutError::NoLeadOut
        );
        assert_eq!(
            fails(&base.replace("Point=0x03", "Point=0x02")),
            CcdLayoutError::DuplicateTrack(2)
        );
        assert!(matches!(
            fails(&base.replace("INDEX 0=300", "INDEX 0=500")),
            CcdLayoutError::BadPosition { track: 2, .. }
        ));
        // Track 3 placed where track 2 starts: track 2 would end before it
        // begins.
        assert!(matches!(
            fails(&base.replace("PLBA=600", "PLBA=300")),
            CcdLayoutError::BadPosition { track: 2, .. }
        ));
        // A lead-out before the last track's start.
        assert!(matches!(
            fails(&base.replace("PLBA=750", "PLBA=500")),
            CcdLayoutError::BadPosition { track: 3, .. }
        ));
        // Track 3's pregap reaching back to track 2's INDEX 1: nothing of
        // track 2 is left after it.
        assert!(matches!(
            fails(&base.replace(
                "MODE=0\r\nINDEX 1=600",
                "MODE=0\r\nINDEX 0=450\r\nINDEX 1=600"
            )),
            CcdLayoutError::BadPosition { track: 2, .. }
        ));
        let only_markers =
            "[CloneCD]\nVersion=3\n[Entry 0]\nSession=1\nPoint=0xa2\nControl=0\nPLBA=10\n";
        let document = parse(only_markers.as_bytes()).unwrap();
        assert_eq!(
            layout(&document, 10 * SECTOR_BYTES, None, &mut |_| None).unwrap_err(),
            CcdLayoutError::NoTracks
        );
        // A first or last track the TOC names wrongly is only noted.
        let renamed = base.replace(
            "PMin=3\r\nPSec=0\r\nPFrame=0\r\nPLBA=13350",
            "PMin=4\r\nPSec=0\r\nPFrame=0\r\nPLBA=13350",
        );
        let document = parse(renamed.as_bytes()).unwrap();
        let laid = layout(&document, img, sub, &mut sectors(&[0])).unwrap();
        assert!(laid.warnings.iter().any(|w| matches!(
            w,
            CcdWarning::FirstOrLastMismatch {
                which: "last",
                declared: 4,
                present: 3
            }
        )));
    }

    #[test]
    fn what_is_not_a_descriptor_is_refused_saying_why() {
        let entry =
            "[CloneCD]\nVersion=3\n[Entry 0]\nSession=1\nPoint=0x01\nControl=0x04\nPLBA=0\n";
        assert_eq!(parse(b""), Err(CcdError::Empty));
        assert_eq!(
            parse(&vec![b'a'; MAX_CCD_BYTES + 1]),
            Err(CcdError::TooLarge)
        );
        assert_eq!(
            parse(format!("[CloneCD]\n{}\n", "x".repeat(MAX_LINE_BYTES + 1)).as_bytes()),
            Err(CcdError::LineTooLong { line: 2 })
        );
        assert_eq!(
            parse(b"Version=3\n"),
            Err(CcdError::OutsideSection { line: 1 })
        );
        assert_eq!(parse(b"[Settings]\nColour=red\n"), Err(CcdError::NotCcd));
        assert!(matches!(
            parse(b"[CloneCD\n"),
            Err(CcdError::Malformed { line: 1, .. })
        ));
        assert!(matches!(
            parse(b"[CloneCD]\njust words\n"),
            Err(CcdError::Malformed { line: 2, .. })
        ));
        assert!(matches!(
            parse(b"[CloneCD]\n[CloneCD]\n"),
            Err(CcdError::DuplicateSection { line: 2, .. })
        ));
        assert!(matches!(
            parse(b"[CloneCD]\nVersion=3\nversion=3\n"),
            Err(CcdError::DuplicateKey { line: 3, .. })
        ));
        assert!(matches!(
            parse(b"[Entry x]\n"),
            Err(CcdError::Malformed { .. })
        ));
        for (from, to) in [
            ("Point=0x01", "Point=0x1ff"),
            ("Point=0x01", "Point=1x"),
            ("Session=1", "Session=0"),
            ("PLBA=0", "PLBA=-1"),
            ("PLBA=0", "PLBA=999999999"),
            ("Control=0x04", "Control=0x10"),
        ] {
            let text = entry.replace(from, to);
            assert!(
                matches!(parse(text.as_bytes()), Err(CcdError::BadValue { .. })),
                "{to}: {:?}",
                parse(text.as_bytes())
            );
        }
        assert_eq!(
            parse(entry.replace("PLBA=0\n", "").as_bytes()),
            Err(CcdError::MissingKey {
                entry: 0,
                key: "PLBA"
            })
        );
        assert!(matches!(
            parse(b"[CloneCD]\n[TRACK 100]\nMODE=1\n"),
            Err(CcdError::BadValue { .. })
        ));
        assert!(matches!(
            parse(b"[CloneCD]\n[TRACK 1]\nMODE=3\n"),
            Err(CcdError::BadValue { .. })
        ));
        assert!(matches!(
            parse(b"[CloneCD]\n[CDText]\nEntries=1\nEntry 0=00 01\n"),
            Err(CcdError::BadValue { .. })
        ));
        let sections: String = std::iter::once("[CloneCD]\n".to_owned())
            .chain((0..=MAX_SECTIONS).map(|n| format!("[Session {n}]\n")))
            .collect();
        assert_eq!(
            parse(sections.as_bytes()),
            Err(CcdError::TooMany { what: "sections" })
        );
    }

    #[test]
    fn cd_text_packs_are_counted_not_decoded() {
        let pack = "80 00 00 00 54 61 6E 67 69 62 6C 65 00 00 00 00 12 34";
        let text =
            format!("[CloneCD]\nVersion=3\n[CDText]\nEntries=2\nEntry 0={pack}\nEntry 1={pack}\n");
        assert_eq!(parse(text.as_bytes()).unwrap().cdtext_packs, 2);
    }

    #[test]
    fn the_image_and_subchannel_are_named_after_the_descriptor() {
        assert_eq!(
            companion_names("Game (Disc 1).ccd"),
            Some((
                "Game (Disc 1).img".to_owned(),
                "Game (Disc 1).sub".to_owned()
            ))
        );
        assert_eq!(
            companion_names("dir/GAME.CCD"),
            Some(("dir/GAME.img".to_owned(), "dir/GAME.sub".to_owned()))
        );
        assert_eq!(companion_names("game.cue"), None);
        assert_eq!(companion_names(".ccd"), None);
        assert_eq!(companion_names("ccd"), None);
    }

    #[test]
    fn numbers_read_as_clonecd_writes_them() {
        assert_eq!(number("0xa0", true), Some(0xA0));
        assert_eq!(number("0XA0", true), Some(0xA0));
        assert_eq!(number("0xa0", false), None);
        assert_eq!(number("-150", false), Some(-150));
        assert_eq!(number(" 12 ", false), Some(12));
        for bad in ["", "0x", "1.5", "+3", "12345678901", "0x123456789", "--1"] {
            assert_eq!(number(bad, true), None, "{bad}");
        }
    }
}
