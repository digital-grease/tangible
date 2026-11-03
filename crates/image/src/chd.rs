// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! CHD images (MAME's Compressed Hunks of Data) read as originals, version 5
//! (docs/plans/chd-images.md C1).
//!
//! A CHD is a header, a map of compressed hunks, and a chain of metadata
//! entries. This reads the header and the metadata, never the hunks: what the
//! disc was and what tracks it has are in the metadata, and the data is only
//! ever read by chdman, in a derivation. No specification is published beyond
//! MAME's own source (`src/lib/util/chd.cpp`, `cdrom.h`), which this follows;
//! it is checked against chdman's reading of fixtures it made
//! (fixtures/tool-output/chdman).
//!
//! Untrusted bytes, so everything is bounded: the header's fields against
//! each other, the metadata chain against the file's length, a loop in it,
//! the number of entries and how much of each is read, and track text, which
//! must be one of the shapes chdman writes.

use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom};

/// The signature every CHD starts with.
pub const SIGNATURE: &[u8; 8] = b"MComprHD";

/// A version 5 header's length, the first bytes [`parse_header`] needs.
pub const HEADER_BYTES: usize = 124;

/// The one version read: what chdman has written since 0.146.
const VERSION: u32 = 5;

/// A CD frame as a CHD holds it: 2352 bytes of sector and 96 of subcode.
pub const CD_FRAME_BYTES: u32 = 2448;

/// The sector a DVD CHD holds.
pub const DVD_SECTOR_BYTES: u32 = 2048;

/// chdman pads every CD track to a whole number of these frames.
const CD_TRACK_PADDING: u64 = 4;

/// Most metadata entries followed, and most bytes of one read.
const MAX_ENTRIES: usize = 1024;
const MAX_ENTRY_READ: u32 = 64 << 10;
/// Most bytes of entry data read from one chain: entries may overlap, so
/// without a total a small file could make every entry read its most.
const MAX_CHAIN_READ: usize = 1 << 20;

/// The entries whose data is read: track text. Of the rest only the tag
/// matters.
const TRACK_TAGS: [&[u8; 4]; 3] = [b"CHT2", b"CHTR", b"CHGD"];
const ENTRY_HEADER: usize = 16;

/// Most tracks a disc has, and most frames one track may claim: more than
/// any CD or GD-ROM holds.
const MAX_TRACKS: usize = 99;
const MAX_TRACK_FRAMES: u64 = 1 << 24;

/// A version 5 header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChdHeader {
    /// The compressors hunks may use, as their four-letter codes, in order.
    pub compressors: Vec<String>,
    /// The size of the data, uncompressed.
    pub logical_bytes: u64,
    /// Where the hunk map is.
    pub map_offset: u64,
    /// Where the first metadata entry is, or 0 for none.
    pub meta_offset: u64,
    /// Bytes in a hunk, the unit of compression.
    pub hunk_bytes: u32,
    /// Bytes in a unit: a CD frame, a DVD or hard disk sector.
    pub unit_bytes: u32,
    /// SHA-1 of the data, uncompressed: chdman's "Data SHA1".
    pub raw_sha1: [u8; 20],
    /// SHA-1 of the data and the metadata that is checksummed: "SHA1".
    pub sha1: [u8; 20],
    /// The parent's SHA-1, when this is a delta that needs its parent to be
    /// read.
    pub parent_sha1: Option<[u8; 20]>,
}

impl ChdHeader {
    /// How many units the data holds.
    #[must_use]
    pub fn unit_count(&self) -> u64 {
        self.logical_bytes / u64::from(self.unit_bytes)
    }
}

/// One metadata entry, as much of it as was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataEntry {
    /// Its four-letter tag, such as `CHT2`.
    pub tag: [u8; 4],
    /// Its flags: 1 means it is in the SHA-1.
    pub flags: u8,
    /// Its length as declared.
    pub length: u32,
    /// Its first bytes, at most 64 KiB of them, for a track entry; empty for
    /// any other, whose tag is all that is read.
    pub data: Vec<u8>,
}

impl MetadataEntry {
    /// The tag as text, with anything not printable shown as `?`.
    #[must_use]
    pub fn tag_text(&self) -> String {
        fourcc(self.tag)
    }
}

/// What a CHD holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChdMedium {
    /// A CD: track metadata and 2448-byte frames.
    Cd,
    /// A GD-ROM: GD track metadata.
    GdRom,
    /// A DVD: a `DVD ` entry and 2048-byte sectors.
    Dvd,
    /// A hard disk, which is not a disc.
    HardDisk,
    /// A laserdisc, likewise not a disc Tangible handles.
    Laserdisc,
    /// Nothing the metadata says.
    Unknown,
}

/// A CD track's data, as chdman names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChdTrackType {
    /// Mode 1, 2048 bytes a sector.
    Mode1,
    /// Mode 1, raw 2352 bytes a sector.
    Mode1Raw,
    /// Mode 2, 2336 bytes a sector.
    Mode2,
    /// Mode 2 form 1, 2048 bytes a sector.
    Mode2Form1,
    /// Mode 2 form 2, 2324 bytes a sector.
    Mode2Form2,
    /// Mode 2, forms mixed, 2336 bytes a sector.
    Mode2FormMix,
    /// Mode 2, raw 2352 bytes a sector.
    Mode2Raw,
    /// Audio, 2352 bytes a sector.
    Audio,
}

impl ChdTrackType {
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "MODE1" => Self::Mode1,
            "MODE1_RAW" => Self::Mode1Raw,
            "MODE2" => Self::Mode2,
            "MODE2_FORM1" => Self::Mode2Form1,
            "MODE2_FORM2" => Self::Mode2Form2,
            "MODE2_FORM_MIX" => Self::Mode2FormMix,
            "MODE2_RAW" => Self::Mode2Raw,
            "AUDIO" => Self::Audio,
            _ => return None,
        })
    }

    /// chdman's name for it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mode1 => "MODE1",
            Self::Mode1Raw => "MODE1_RAW",
            Self::Mode2 => "MODE2",
            Self::Mode2Form1 => "MODE2_FORM1",
            Self::Mode2Form2 => "MODE2_FORM2",
            Self::Mode2FormMix => "MODE2_FORM_MIX",
            Self::Mode2Raw => "MODE2_RAW",
            Self::Audio => "AUDIO",
        }
    }
}

/// A CD track's subcode, as chdman names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChdSubcode {
    /// None kept.
    None,
    /// R to W, cooked.
    Rw,
    /// R to W, raw.
    RwRaw,
}

impl ChdSubcode {
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "NONE" => Self::None,
            "RW" => Self::Rw,
            "RW_RAW" => Self::RwRaw,
            _ => return None,
        })
    }
}

/// One CD or GD-ROM track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChdTrack {
    /// Its number.
    pub number: u32,
    /// Its data.
    pub track_type: ChdTrackType,
    /// Its subcode.
    pub subcode: ChdSubcode,
    /// Frames of it the CHD holds, its pregap included when
    /// `pregap_in_data`.
    pub frames: u64,
    /// Its pregap, in frames.
    pub pregap: u64,
    /// Whether the pregap's frames are in the data (chdman's `V` before the
    /// pregap type), as a CUE sheet's INDEX 00 is, or generated, as PREGAP.
    pub pregap_in_data: bool,
    /// Its postgap, in frames, generated.
    pub postgap: u64,
}

/// What a CHD's header and metadata say it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChdDescription {
    /// The header.
    pub header: ChdHeader,
    /// What it holds.
    pub medium: ChdMedium,
    /// Its tracks, on a CD or GD-ROM.
    pub tracks: Vec<ChdTrack>,
    /// Every metadata entry's tag, in order.
    pub tags: Vec<String>,
    /// What is odd about it.
    pub warnings: Vec<String>,
}

/// Why a CHD could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChdError {
    /// Not a CHD.
    #[error("the file is not a CHD: it does not start MComprHD")]
    NotChd,
    /// A version this reader does not read.
    #[error("CHD version {0} is not read; only version 5 is (update it with `chdman copy`)")]
    Version(u32),
    /// Shorter than its header.
    #[error("the file is shorter than a CHD header")]
    Truncated,
    /// Header fields that cannot all be true.
    #[error("the CHD header's {0}")]
    Header(&'static str),
    /// A metadata entry outside the file, or a chain that loops or runs on.
    #[error("the CHD metadata {0}")]
    Metadata(&'static str),
    /// Track metadata that is not the shape chdman writes.
    #[error("track metadata entry {index} is not one chdman writes: {reason}")]
    Track {
        /// Its place among the track entries, from 0.
        index: usize,
        /// Why.
        reason: &'static str,
    },
    /// Reading failed.
    #[error("the CHD could not be read: {0}")]
    Io(String),
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0_u8; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_be_bytes(word)
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0_u8; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_be_bytes(word)
}

fn sha1_at(bytes: &[u8], at: usize) -> [u8; 20] {
    let mut digest = [0_u8; 20];
    digest.copy_from_slice(&bytes[at..at + 20]);
    digest
}

/// Read a header from the first bytes of a file of `file_bytes` bytes.
///
/// # Errors
///
/// [`ChdError`] when it is not a version 5 CHD header that holds together.
pub fn parse_header(bytes: &[u8], file_bytes: u64) -> Result<ChdHeader, ChdError> {
    if !bytes.starts_with(SIGNATURE) {
        // The start of a signature, cut short, is a CHD too short to read.
        let cut_short = !bytes.is_empty() && SIGNATURE.starts_with(bytes);
        return Err(if cut_short {
            ChdError::Truncated
        } else {
            ChdError::NotChd
        });
    }
    if bytes.len() < 16 {
        return Err(ChdError::Truncated);
    }
    let length = u32_at(bytes, 8);
    let version = u32_at(bytes, 12);
    if version != VERSION {
        return Err(ChdError::Version(version));
    }
    if length as usize != HEADER_BYTES {
        return Err(ChdError::Header(
            "length is not 124 bytes, as version 5's is",
        ));
    }
    if bytes.len() < HEADER_BYTES {
        return Err(ChdError::Truncated);
    }
    let compressors = (0..4)
        .map(|n| u32_at(bytes, 16 + n * 4))
        .take_while(|&code| code != 0)
        .map(|code| fourcc(code.to_be_bytes()))
        .collect();
    let header = ChdHeader {
        compressors,
        logical_bytes: u64_at(bytes, 32),
        map_offset: u64_at(bytes, 40),
        meta_offset: u64_at(bytes, 48),
        hunk_bytes: u32_at(bytes, 56),
        unit_bytes: u32_at(bytes, 60),
        raw_sha1: sha1_at(bytes, 64),
        sha1: sha1_at(bytes, 84),
        parent_sha1: Some(sha1_at(bytes, 104)).filter(|digest| digest != &[0; 20]),
    };
    if header.unit_bytes == 0 || header.hunk_bytes == 0 {
        return Err(ChdError::Header("hunk or unit size is zero"));
    }
    if !header.hunk_bytes.is_multiple_of(header.unit_bytes) {
        return Err(ChdError::Header("hunk is not a whole number of units"));
    }
    if !header
        .logical_bytes
        .is_multiple_of(u64::from(header.unit_bytes))
    {
        return Err(ChdError::Header(
            "logical size is not a whole number of units",
        ));
    }
    let inside = |offset: u64| offset >= HEADER_BYTES as u64 && offset < file_bytes;
    if !inside(header.map_offset) {
        return Err(ChdError::Header("map lies outside the file"));
    }
    if header.meta_offset != 0 && !inside(header.meta_offset) {
        return Err(ChdError::Header("metadata lies outside the file"));
    }
    Ok(header)
}

/// A four-letter code as text, anything not printable shown as `?`.
fn fourcc(code: [u8; 4]) -> String {
    code.iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                char::from(b)
            } else {
                '?'
            }
        })
        .collect()
}

/// Follow the metadata chain from the header, reading each entry's first
/// bytes, from a file of `file_bytes` bytes.
///
/// # Errors
///
/// [`ChdError::Metadata`] for an entry outside the file, a loop, or a chain
/// longer than 1024 entries; [`ChdError::Io`] when reading fails.
pub fn read_metadata<R: Read + Seek>(
    reader: &mut R,
    header: &ChdHeader,
    file_bytes: u64,
) -> Result<Vec<MetadataEntry>, ChdError> {
    let io = |error: std::io::Error| ChdError::Io(error.to_string());
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    let mut read_total = 0_usize;
    let mut offset = header.meta_offset;
    while offset != 0 {
        if entries.len() == MAX_ENTRIES {
            return Err(ChdError::Metadata("chain is longer than 1024 entries"));
        }
        if !seen.insert(offset) {
            return Err(ChdError::Metadata("chain loops"));
        }
        let end = offset
            .checked_add(ENTRY_HEADER as u64)
            .filter(|&end| offset >= HEADER_BYTES as u64 && end <= file_bytes)
            .ok_or(ChdError::Metadata("entry lies outside the file"))?;
        reader.seek(SeekFrom::Start(offset)).map_err(io)?;
        let mut head = [0_u8; ENTRY_HEADER];
        reader.read_exact(&mut head).map_err(io)?;
        let packed = u32_at(&head, 4);
        let length = packed & 0x00FF_FFFF;
        if end + u64::from(length) > file_bytes {
            return Err(ChdError::Metadata("entry's data lies outside the file"));
        }
        let tag = [head[0], head[1], head[2], head[3]];
        let wanted = if TRACK_TAGS.contains(&&tag) {
            length.min(MAX_ENTRY_READ) as usize
        } else {
            0
        };
        read_total += wanted;
        if read_total > MAX_CHAIN_READ {
            return Err(ChdError::Metadata(
                "track entries hold more than any disc's",
            ));
        }
        let mut data = vec![0_u8; wanted];
        reader.read_exact(&mut data).map_err(io)?;
        entries.push(MetadataEntry {
            tag,
            flags: head[4],
            length,
            data,
        });
        offset = u64_at(&head, 8);
    }
    Ok(entries)
}

/// Read a CHD's header and metadata from a file.
///
/// # Errors
///
/// [`ChdError`] when either cannot be read.
pub fn read<R: Read + Seek>(reader: &mut R, file_bytes: u64) -> Result<ChdDescription, ChdError> {
    let io = |error: std::io::Error| ChdError::Io(error.to_string());
    reader.seek(SeekFrom::Start(0)).map_err(io)?;
    let mut prefix = Vec::with_capacity(HEADER_BYTES);
    reader
        .take(HEADER_BYTES as u64)
        .read_to_end(&mut prefix)
        .map_err(io)?;
    let header = parse_header(&prefix, file_bytes)?;
    let entries = read_metadata(reader, &header, file_bytes)?;
    describe(header, &entries)
}

/// What a header and its metadata say the CHD holds.
///
/// # Errors
///
/// [`ChdError::Track`] for CD or GD-ROM track metadata that is not the
/// shape chdman writes.
pub fn describe(header: ChdHeader, entries: &[MetadataEntry]) -> Result<ChdDescription, ChdError> {
    let tags: Vec<String> = entries.iter().map(MetadataEntry::tag_text).collect();
    let has = |tag: &[u8; 4]| entries.iter().any(|entry| &entry.tag == tag);
    let mut warnings = Vec::new();
    let mut tracks = Vec::new();

    let gd = has(b"CHGD");
    let medium = if has(b"CHT2") || has(b"CHTR") || has(b"CHCD") || gd {
        let wanted: &[&[u8; 4]] = if gd { &[b"CHGD"] } else { &[b"CHT2", b"CHTR"] };
        for (index, entry) in entries
            .iter()
            .filter(|entry| wanted.contains(&&entry.tag))
            .enumerate()
        {
            if index == MAX_TRACKS {
                return Err(ChdError::Track {
                    index,
                    reason: "more tracks than a disc holds",
                });
            }
            tracks.push(track(index, entry)?);
        }
        if tracks.is_empty() {
            warnings.push(
                "its CD metadata is the old binary kind (CHCD), which is not read; update it \
                 with `chdman copy`"
                    .to_owned(),
            );
        }
        check_tracks(&header, &tracks, &mut warnings)?;
        if gd { ChdMedium::GdRom } else { ChdMedium::Cd }
    } else if has(b"DVD ") {
        if header.unit_bytes != DVD_SECTOR_BYTES {
            warnings.push(format!(
                "a DVD CHD holds 2048-byte sectors, and this one's units are {} bytes",
                header.unit_bytes
            ));
        }
        ChdMedium::Dvd
    } else if has(b"GDDD") || has(b"IDNT") {
        warnings.push("it is a hard disk image, not a disc".to_owned());
        ChdMedium::HardDisk
    } else if has(b"AVAV") || has(b"AVLD") {
        warnings.push("it is a laserdisc image, which Tangible does not handle".to_owned());
        ChdMedium::Laserdisc
    } else {
        warnings.push("its metadata does not say what disc it is".to_owned());
        ChdMedium::Unknown
    };
    if header.parent_sha1.is_some() {
        warnings
            .push("it is a delta of another CHD and cannot be read without that parent".to_owned());
    }
    Ok(ChdDescription {
        header,
        medium,
        tracks,
        tags,
        warnings,
    })
}

/// One track entry's text, `KEY:VALUE` words as chdman writes them.
fn track(index: usize, entry: &MetadataEntry) -> Result<ChdTrack, ChdError> {
    let refuse = |reason| ChdError::Track { index, reason };
    if entry.length as usize != entry.data.len() {
        return Err(refuse("it is longer than any track entry"));
    }
    let text = entry.data.strip_suffix(&[0]).unwrap_or(&entry.data);
    let text = std::str::from_utf8(text)
        .ok()
        .filter(|text| text.bytes().all(|b| b.is_ascii_graphic() || b == b' '))
        .ok_or(refuse("it is not plain text"))?;
    let mut number = None;
    let mut track_type = None;
    let mut subcode = None;
    let mut frames = None;
    let mut pregap = 0;
    let mut pregap_in_data = false;
    let mut postgap = 0;
    let count = |value: &str| -> Result<u64, ChdError> {
        // Digits only: Rust would also take a leading `+`.
        Some(value)
            .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|digits| digits.parse::<u64>().ok())
            .filter(|&n| n <= MAX_TRACK_FRAMES)
            .ok_or(refuse("a count is not a number a disc holds"))
    };
    for word in text.split(' ').filter(|word| !word.is_empty()) {
        let (key, value) = word
            .split_once(':')
            .ok_or(refuse("a word is not KEY:VALUE"))?;
        match key {
            "TRACK" => number = Some(count(value)?),
            "TYPE" => {
                track_type = Some(
                    ChdTrackType::parse(value).ok_or(refuse("its type is not one chdman names"))?,
                );
            }
            "SUBTYPE" => {
                subcode = Some(
                    ChdSubcode::parse(value)
                        .ok_or(refuse("its subcode is not one chdman names"))?,
                );
            }
            "FRAMES" => frames = Some(count(value)?),
            "PREGAP" => pregap = count(value)?,
            "PGTYPE" => pregap_in_data = value.starts_with('V'),
            "POSTGAP" => postgap = count(value)?,
            // The pregap's subcode, and a GD-ROM track's padding: read by
            // chdman when it extracts, nothing Tangible decides on.
            "PGSUB" | "PAD" => {}
            _ => return Err(refuse("it has a key chdman does not write")),
        }
    }
    let number = number.ok_or(refuse("it has no TRACK"))?;
    let number = u32::try_from(number)
        .ok()
        .filter(|n| (1..=99).contains(n))
        .ok_or(refuse("its number is not 1 to 99"))?;
    let frames = frames.ok_or(refuse("it has no FRAMES"))?;
    if pregap_in_data && pregap > frames {
        return Err(refuse("its pregap is longer than the track"));
    }
    Ok(ChdTrack {
        number,
        track_type: track_type.ok_or(refuse("it has no TYPE"))?,
        subcode: subcode.ok_or(refuse("it has no SUBTYPE"))?,
        frames,
        pregap,
        pregap_in_data,
        postgap,
    })
}

/// Tracks numbered one after another from 1, in frames the data holds.
fn check_tracks(
    header: &ChdHeader,
    tracks: &[ChdTrack],
    warnings: &mut Vec<String>,
) -> Result<(), ChdError> {
    for (index, track) in tracks.iter().enumerate() {
        if usize::try_from(track.number).ok() != Some(index + 1) {
            return Err(ChdError::Track {
                index,
                reason: "tracks are not numbered one after another from 1",
            });
        }
    }
    if header.unit_bytes != CD_FRAME_BYTES {
        warnings.push(format!(
            "a CD CHD holds 2448-byte frames, and this one's units are {} bytes",
            header.unit_bytes
        ));
        return Ok(());
    }
    let padded: u64 = tracks
        .iter()
        .map(|track| track.frames.div_ceil(CD_TRACK_PADDING) * CD_TRACK_PADDING)
        .sum();
    if !tracks.is_empty() && padded != header.unit_count() {
        warnings.push(format!(
            "its tracks come to {padded} frames and its data holds {}",
            header.unit_count()
        ));
    }
    Ok(())
}

/// A SHA-1 as lowercase hex.
#[must_use]
pub fn hex(digest: &[u8; 20]) -> String {
    use std::fmt::Write as _;
    digest
        .iter()
        .fold(String::with_capacity(40), |mut text, b| {
            let _ = write!(text, "{b:02x}");
            text
        })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/../../fixtures/images/chd/{name}.chd",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn read_fixture(name: &str) -> ChdDescription {
        let bytes = fixture(name);
        let length = bytes.len() as u64;
        read(&mut Cursor::new(bytes), length).unwrap()
    }

    /// Labelled values, in order.
    type Pairs = Vec<(String, String)>;

    /// What chdman's `info -v` said of a fixture: its labelled fields, and
    /// each metadata entry's tag and text.
    fn chdman_info(name: &str) -> (Pairs, Pairs) {
        let text = std::fs::read_to_string(format!(
            "{}/../../fixtures/tool-output/chdman/{name}.txt",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let mut fields = Vec::new();
        let mut metadata = Vec::new();
        let mut lines = text.lines().peekable();
        while let Some(line) = lines.next() {
            if let Some(rest) = line.strip_prefix("Metadata:") {
                let tag = rest.split('\'').nth(1).unwrap().to_owned();
                let value = lines
                    .next()
                    .unwrap()
                    .trim()
                    .trim_end_matches('.')
                    .to_owned();
                metadata.push((tag, value));
            } else if let Some((label, value)) = line.split_once(':') {
                fields.push((label.trim().to_owned(), value.trim().to_owned()));
            }
        }
        (fields, metadata)
    }

    fn field<'a>(fields: &'a [(String, String)], label: &str) -> Option<&'a str> {
        fields
            .iter()
            .find(|(l, _)| l == label)
            .map(|(_, v)| v.as_str())
    }

    fn number(text: &str) -> u64 {
        text.split(' ')
            .next()
            .unwrap()
            .replace(',', "")
            .parse()
            .unwrap()
    }

    #[test]
    fn every_fixture_reads_as_chdman_reads_it() {
        for name in [
            "single-data",
            "mixed-mode",
            "generated-pregap",
            "cooked",
            "delta",
            "dvd",
            "hd",
        ] {
            let read = read_fixture(name);
            let (fields, metadata) = chdman_info(name);
            let header = &read.header;
            assert_eq!(field(&fields, "File Version"), Some("5"), "{name}");
            assert_eq!(
                header.logical_bytes,
                number(field(&fields, "Logical size").unwrap()),
                "{name}"
            );
            assert_eq!(
                u64::from(header.hunk_bytes),
                number(field(&fields, "Hunk Size").unwrap()),
                "{name}"
            );
            assert_eq!(
                u64::from(header.unit_bytes),
                number(field(&fields, "Unit Size").unwrap()),
                "{name}"
            );
            assert_eq!(
                header.unit_count(),
                number(field(&fields, "Total Units").unwrap()),
                "{name}"
            );
            let compression = field(&fields, "Compression").unwrap();
            if compression == "none" {
                assert!(header.compressors.is_empty(), "{name}");
            } else {
                let named: Vec<&str> = compression
                    .split(", ")
                    .map(|c| c.split(' ').next().unwrap())
                    .collect();
                assert_eq!(header.compressors, named, "{name}");
            }
            if let Some(sha1) = field(&fields, "SHA1") {
                assert_eq!(hex(&header.sha1), sha1, "{name}");
            }
            if let Some(raw) = field(&fields, "Data SHA1") {
                assert_eq!(hex(&header.raw_sha1), raw, "{name}");
            }
            assert_eq!(
                header.parent_sha1.as_ref().map(hex).as_deref(),
                field(&fields, "Parent SHA1"),
                "{name}"
            );
            let tags: Vec<&str> = metadata.iter().map(|(tag, _)| tag.as_str()).collect();
            assert_eq!(read.tags, tags, "{name}");
            // Every CD track entry as chdman printed it.
            let texts: Vec<String> = read
                .tracks
                .iter()
                .map(|t| {
                    format!(
                        "TRACK:{} TYPE:{} FRAMES:{} PREGAP:{}",
                        t.number,
                        t.track_type.as_str(),
                        t.frames,
                        t.pregap
                    )
                })
                .collect();
            let printed: Vec<String> = metadata
                .iter()
                .filter(|(tag, _)| tag == "CHT2")
                .map(|(_, text)| {
                    let word = |key: &str| {
                        text.split(' ')
                            .find(|w| w.starts_with(&format!("{key}:")))
                            .unwrap()
                            .to_owned()
                    };
                    format!(
                        "{} {} {} {}",
                        word("TRACK"),
                        word("TYPE"),
                        word("FRAMES"),
                        word("PREGAP")
                    )
                })
                .collect();
            assert_eq!(texts, printed, "{name}");
        }
    }

    #[test]
    fn says_what_each_fixture_holds() {
        let medium = |name: &str| read_fixture(name).medium;
        for name in [
            "single-data",
            "mixed-mode",
            "generated-pregap",
            "cooked",
            "delta",
        ] {
            assert_eq!(medium(name), ChdMedium::Cd, "{name}");
        }
        assert_eq!(medium("dvd"), ChdMedium::Dvd);
        assert_eq!(medium("hd"), ChdMedium::HardDisk);
        assert!(read_fixture("hd").warnings[0].contains("hard disk"));
        assert!(read_fixture("single-data").warnings.is_empty());
        assert!(read_fixture("dvd").warnings.is_empty());

        let mixed = read_fixture("mixed-mode");
        assert!(mixed.warnings.is_empty(), "{:?}", mixed.warnings);
        assert_eq!(mixed.tracks[1].pregap, 150);
        assert!(mixed.tracks[1].pregap_in_data, "INDEX 00 is in the data");
        assert_eq!(mixed.tracks[1].track_type, ChdTrackType::Audio);
        let generated = read_fixture("generated-pregap");
        assert!(!generated.tracks[1].pregap_in_data, "PREGAP is generated");
        assert_eq!(
            read_fixture("cooked").tracks[0].track_type,
            ChdTrackType::Mode1
        );

        let delta = read_fixture("delta");
        assert_eq!(
            delta.header.parent_sha1,
            Some(read_fixture("single-data").header.sha1)
        );
        assert!(delta.warnings.iter().any(|w| w.contains("parent")));
    }

    #[test]
    fn refuses_what_is_not_a_version_five_chd() {
        let good = fixture("single-data");
        let length = good.len() as u64;
        assert_eq!(parse_header(b"not a chd at all", 16), Err(ChdError::NotChd));
        assert_eq!(parse_header(b"MCom", 4), Err(ChdError::Truncated));
        assert_eq!(parse_header(&good[..100], length), Err(ChdError::Truncated));
        let with = |at: usize, value: &[u8]| {
            let mut bytes = good.clone();
            bytes[at..at + value.len()].copy_from_slice(value);
            parse_header(&bytes, length)
        };
        assert_eq!(with(12, &4_u32.to_be_bytes()), Err(ChdError::Version(4)));
        assert!(matches!(
            with(8, &108_u32.to_be_bytes()),
            Err(ChdError::Header(_))
        ));
        assert!(matches!(
            with(60, &0_u32.to_be_bytes()),
            Err(ChdError::Header(_))
        ));
        assert!(matches!(
            with(60, &2047_u32.to_be_bytes()),
            Err(ChdError::Header(_))
        ));
        assert!(matches!(
            with(40, &u64::MAX.to_be_bytes()),
            Err(ChdError::Header(_))
        ));
        assert!(matches!(
            with(48, &length.to_be_bytes()),
            Err(ChdError::Header(_))
        ));
        assert!(parse_header(&good, length).is_ok());
    }

    #[test]
    fn refuses_a_metadata_chain_that_loops_or_leaves_the_file() {
        let good = fixture("mixed-mode");
        let length = good.len() as u64;
        let header = parse_header(&good, length).unwrap();
        let first = usize::try_from(header.meta_offset).unwrap();
        let chain = |bytes: Vec<u8>| {
            let length = bytes.len() as u64;
            read_metadata(&mut Cursor::new(bytes), &header, length)
        };
        // The first entry pointing at itself.
        let mut looped = good.clone();
        looped[first + 8..first + 16].copy_from_slice(&header.meta_offset.to_be_bytes());
        assert_eq!(chain(looped), Err(ChdError::Metadata("chain loops")));
        // Its next entry past the end, and its data past the end.
        let mut away = good.clone();
        away[first + 8..first + 16].copy_from_slice(&(length - 4).to_be_bytes());
        assert!(matches!(chain(away), Err(ChdError::Metadata(_))));
        let mut long = good.clone();
        long[first + 5..first + 8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
        assert!(matches!(chain(long), Err(ChdError::Metadata(_))));
        assert_eq!(chain(good.clone()).unwrap().len(), 3);

        // Found by review: entries 16 bytes apart, each claiming 64 KiB of
        // track text over the ones after it, would read far more than the
        // file holds. The total is capped.
        let mut overlapping = good[..first].to_vec();
        let count = 1000_u64;
        for n in 0..count {
            let at = header.meta_offset + n * 16;
            let next = if n + 1 == count { 0 } else { at + 16 };
            overlapping.extend_from_slice(b"CHT2");
            overlapping.extend_from_slice(&(0x0100_0000_u32 | 0xFFFF).to_be_bytes());
            overlapping.extend_from_slice(&next.to_be_bytes());
        }
        overlapping.resize(overlapping.len() + 0x1_0000, 0);
        assert_eq!(
            chain(overlapping),
            Err(ChdError::Metadata(
                "track entries hold more than any disc's"
            ))
        );
    }

    #[test]
    fn refuses_track_text_chdman_does_not_write() {
        let entry = |text: &str| MetadataEntry {
            tag: *b"CHT2",
            flags: 1,
            length: u32::try_from(text.len()).unwrap(),
            data: text.as_bytes().to_vec(),
        };
        let good = "TRACK:1 TYPE:MODE1_RAW SUBTYPE:NONE FRAMES:300 PREGAP:0 PGTYPE:MODE1 \
                    PGSUB:NONE POSTGAP:0\0";
        assert!(track(0, &entry(good)).is_ok());
        for bad in [
            "TRACK:1 TYPE:MODE3 SUBTYPE:NONE FRAMES:300",
            "TRACK:1 TYPE:MODE1 SUBTYPE:XX FRAMES:300",
            "TRACK:0 TYPE:MODE1 SUBTYPE:NONE FRAMES:300",
            "TRACK:100 TYPE:MODE1 SUBTYPE:NONE FRAMES:300",
            "TRACK:1 TYPE:MODE1 SUBTYPE:NONE",
            "TRACK:1 TYPE:MODE1 SUBTYPE:NONE FRAMES:-1",
            "TRACK:1 TYPE:MODE1 SUBTYPE:NONE FRAMES:99999999999",
            "TRACK:1 TYPE:MODE1 SUBTYPE:NONE FRAMES:10 PREGAP:20 PGTYPE:VMODE1",
            "TRACK:1 TYPE:MODE1 SUBTYPE:NONE FRAMES:300 COLOUR:BLUE",
            "TRACK:1 TYPE:MODE1\tSUBTYPE:NONE FRAMES:300",
            "TRACK:1 TYPE:MODE1 SUBTYPE:NONE FRAMES:300 junk",
            "TRACK:+1 TYPE:MODE1 SUBTYPE:NONE FRAMES:300",
        ] {
            assert!(
                matches!(track(0, &entry(bad)), Err(ChdError::Track { .. })),
                "{bad:?}"
            );
        }
        // Tracks out of order.
        let header = read_fixture("mixed-mode").header;
        let two = entry("TRACK:2 TYPE:AUDIO SUBTYPE:NONE FRAMES:300");
        assert!(matches!(
            describe(header, &[two]),
            Err(ChdError::Track { index: 0, .. })
        ));
    }
}
