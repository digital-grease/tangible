// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Alcohol 120% MDS descriptors, version 1 (docs/plans/mds-images.md M1).
//!
//! An MDS file describes a disc and points at its data, an MDF file: a
//! header, session blocks, raw-TOC-like track blocks with an extra block
//! (pregap and length) each on a CD, and footers naming the data files. No
//! specification is published; this follows libmirage 3.2.10's reading of
//! it, checked against libmirage itself (fixtures/tool-output/libmirage).
//!
//! Untrusted bytes, so everything is bounded: the descriptor's size, every
//! offset and count against it, sessions, tracks and files per track, and
//! filenames, which must be plain names in the descriptor's own folder.
//! Unlike libmirage, nothing is read past what the descriptor holds.
//!
//! The MDF does not hold pregaps: a track's pregap is generated, as a CUE
//! sheet's PREGAP is. [`layout`] lays a CD image without subchannel out like
//! a CUE sheet's tracks over one data file; anything else is described but
//! not laid out, and says why.

use crate::cue::{CdLayout, TrackLayout};
use tangible_domain::cd::{SampleByteOrder, TrackFlag};

/// Largest descriptor read. A real one is a few kilobytes; DPM data can add
/// some more.
pub const MAX_DESCRIPTOR_BYTES: usize = 16 << 20;

/// The signature every descriptor starts with.
pub const SIGNATURE: &[u8; 16] = b"MEDIA DESCRIPTOR";

const HEADER: usize = 88;
const SESSION: usize = 24;
const TRACK: usize = 80;
const EXTRA: usize = 8;
const FOOTER: usize = 16;

/// Most sessions, tracks, and files per track.
const MAX_SESSIONS: usize = 99;
const MAX_TRACKS: usize = 99;
const MAX_FILES: usize = 256;

/// Longest data file name, in bytes as stored.
const MAX_NAME_BYTES: usize = 1024;

/// What the disc was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Medium {
    /// A pressed CD.
    CdRom,
    /// A CD-R.
    CdR,
    /// A CD-RW.
    CdRw,
    /// A pressed DVD.
    DvdRom,
    /// A DVD-R.
    DvdR,
}

impl Medium {
    /// Whether it is a DVD.
    #[must_use]
    pub const fn is_dvd(self) -> bool {
        matches!(self, Self::DvdRom | Self::DvdR)
    }
}

/// A track's mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackMode {
    /// Audio.
    Audio,
    /// Mode 1 data.
    Mode1,
    /// Mode 2 data, formless.
    Mode2,
    /// Mode 2 form 1.
    Mode2Form1,
    /// Mode 2 form 2.
    Mode2Form2,
}

/// What a track's sectors carry besides their main data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subchannel {
    /// Nothing.
    None,
    /// 96 bytes of P to W subchannel after every sector.
    PwInterleaved,
}

/// One session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdsSession {
    /// Its number.
    pub number: u32,
    /// Its first sector, possibly negative (the first session's pregap).
    pub start: i64,
    /// Its last sector.
    pub end: i64,
}

/// One track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdsTrack {
    /// Its number, 1 to 99.
    pub number: u32,
    /// Its session's number.
    pub session: u32,
    /// Its mode.
    pub mode: TrackMode,
    /// Its subchannel.
    pub subchannel: Subchannel,
    /// The control nibble: data, copy permitted, pre-emphasis.
    pub ctl: u8,
    /// Bytes per sector in the data file, subchannel included.
    pub sector_size: u32,
    /// The sector its data (INDEX 01) starts at.
    pub start_sector: i64,
    /// Where its data starts in the first data file.
    pub start_offset: u64,
    /// Its pregap, in sectors, which the data file does not hold.
    pub pregap: u64,
    /// Its length in sectors, its pregap aside.
    pub length: u64,
    /// The data files it is in, as named: `*.mdf` is the descriptor's own
    /// name with `.mdf`.
    pub files: Vec<String>,
}

/// A descriptor, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdsDescriptor {
    /// The version, `(1, minor)`.
    pub version: (u8, u8),
    /// The disc.
    pub medium: Medium,
    /// Its sessions.
    pub sessions: Vec<MdsSession>,
    /// Its tracks, in order.
    pub tracks: Vec<MdsTrack>,
    /// Whether it carries DPM (copy-protection timing) data, which is kept
    /// in the original and never interpreted.
    pub has_dpm: bool,
}

/// Why a descriptor could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MdsError {
    /// Larger than any descriptor.
    #[error("the descriptor is larger than {MAX_DESCRIPTOR_BYTES} bytes")]
    TooLarge,
    /// Not an MDS descriptor.
    #[error("the file is not an MDS descriptor: it does not start MEDIA DESCRIPTOR")]
    NotMds,
    /// A version this reader does not read.
    #[error("MDS version {0}.{1} is not read; only version 1 (Alcohol 120%) is")]
    Version(u8, u8),
    /// A medium this reader does not know.
    #[error("the descriptor's medium type {0:#04x} is not a CD or DVD this reader knows")]
    Medium(u16),
    /// A block or name outside the descriptor.
    #[error("the descriptor's {0} lies outside it")]
    OutOfBounds(&'static str),
    /// More than a disc holds.
    #[error("the descriptor names more {0} than a disc holds")]
    TooMany(&'static str),
    /// A track mode this reader does not know.
    #[error("track {track}'s mode {mode:#04x} is not one this reader knows")]
    Mode {
        /// The track.
        track: u32,
        /// Its mode byte.
        mode: u8,
    },
    /// A data file name that is not a plain name.
    #[error("track {track}'s data file name is not a plain file name in the descriptor's folder")]
    FileName {
        /// The track.
        track: u32,
    },
    /// Nothing a disc holds.
    #[error("the descriptor describes no tracks")]
    NoTracks,
}

/// Why a descriptor is described but not laid out.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MdsLayoutError {
    /// A DVD, which has no tracks to lay out.
    #[error("it is a DVD image, which is one track of 2048-byte sectors")]
    Dvd,
    /// More than one session.
    #[error("it has {0} sessions; only single-session images are laid out")]
    Sessions(usize),
    /// Subchannel after every sector, which a track layout cannot step over.
    #[error(
        "track {0} interleaves subchannel with its sectors, which a track layout cannot describe"
    )]
    Subchannel(u32),
    /// Tracks in more than one data file.
    #[error("its tracks are not all in one data file")]
    Files,
    /// A sector size that is not one a CUE sheet can name for the mode.
    #[error("track {track} has {size}-byte {mode:?} sectors, which no CUE mode names")]
    SectorSize {
        /// The track.
        track: u32,
        /// Its mode.
        mode: TrackMode,
        /// Its size.
        size: u32,
    },
    /// Tracks out of order or overlapping on the disc or in the file.
    #[error("track {0} does not follow the one before it, on the disc or in the data file")]
    Order(u32),
    /// A first track whose pregap is not the usual two seconds.
    #[error("the first track's pregap is {0} sectors, not the usual 150")]
    FirstPregap(u64),
    /// A track reaching past the end of the data file.
    #[error("track {0} reaches past the end of the data file")]
    PastEnd(u32),
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    bytes
        .get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn i32_at(bytes: &[u8], at: usize) -> Option<i32> {
    u32_at(bytes, at).map(|value| i32::from_le_bytes(value.to_le_bytes()))
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    bytes.get(at..at + 8).map(|b| {
        let mut word = [0_u8; 8];
        word.copy_from_slice(b);
        u64::from_le_bytes(word)
    })
}

/// `count` blocks of `size` bytes from `offset`, all inside the descriptor.
fn blocks(
    len: usize,
    offset: u32,
    count: usize,
    size: usize,
    what: &'static str,
) -> Result<usize, MdsError> {
    let start = usize::try_from(offset).map_err(|_| MdsError::OutOfBounds(what))?;
    let end = count
        .checked_mul(size)
        .and_then(|span| start.checked_add(span))
        .ok_or(MdsError::OutOfBounds(what))?;
    if start == 0 || end > len {
        return Err(MdsError::OutOfBounds(what));
    }
    Ok(start)
}

fn mode_of(byte: u8) -> Option<TrackMode> {
    match byte & 0x0F {
        0x01 | 0x09 => Some(TrackMode::Audio),
        0x02 | 0x0A => Some(TrackMode::Mode1),
        0x00 | 0x03 | 0x07 | 0x08 | 0x0B | 0x0F => Some(TrackMode::Mode2),
        0x04 | 0x0C => Some(TrackMode::Mode2Form1),
        0x05 | 0x0D => Some(TrackMode::Mode2Form2),
        _ => None,
    }
}

/// A plain file name: no folders, no parent, no NUL or control character.
fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', ':'])
        && !name.chars().any(char::is_control)
}

/// The data file name a footer gives.
fn file_name(bytes: &[u8], footer: usize, track: u32) -> Result<String, MdsError> {
    let offset = u32_at(bytes, footer).ok_or(MdsError::OutOfBounds("footer"))?;
    let wide = u32_at(bytes, footer + 4).ok_or(MdsError::OutOfBounds("footer"))? != 0;
    let start = usize::try_from(offset).map_err(|_| MdsError::OutOfBounds("data file name"))?;
    let rest = bytes
        .get(start..)
        .filter(|rest| start != 0 && !rest.is_empty())
        .ok_or(MdsError::OutOfBounds("data file name"))?;
    let rest = &rest[..rest.len().min(MAX_NAME_BYTES)];
    let name = if wide {
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .take_while(|&unit| unit != 0)
            .collect();
        String::from_utf16(&units).map_err(|_| MdsError::FileName { track })?
    } else {
        let end = rest
            .iter()
            .position(|&byte| byte == 0)
            .ok_or(MdsError::OutOfBounds("data file name"))?;
        String::from_utf8(rest[..end].to_vec()).map_err(|_| MdsError::FileName { track })?
    };
    if !plain_name(&name) {
        return Err(MdsError::FileName { track });
    }
    Ok(name)
}

/// Read a descriptor.
///
/// # Errors
///
/// [`MdsError`] for a file that is not a version 1 descriptor, or whose
/// blocks, counts or names are out of bounds.
#[allow(clippy::too_many_lines)]
pub fn parse(bytes: &[u8]) -> Result<MdsDescriptor, MdsError> {
    if bytes.len() > MAX_DESCRIPTOR_BYTES {
        return Err(MdsError::TooLarge);
    }
    if bytes.len() < HEADER || &bytes[..16] != SIGNATURE {
        return Err(MdsError::NotMds);
    }
    let version = (bytes[16], bytes[17]);
    if version.0 != 1 {
        return Err(MdsError::Version(version.0, version.1));
    }
    let medium_code = u16_at(bytes, 18).ok_or(MdsError::NotMds)?;
    let medium = match medium_code {
        0x00 => Medium::CdRom,
        0x01 => Medium::CdR,
        0x02 => Medium::CdRw,
        0x10 => Medium::DvdRom,
        0x12 => Medium::DvdR,
        other => return Err(MdsError::Medium(other)),
    };
    let session_count = usize::from(u16_at(bytes, 20).ok_or(MdsError::NotMds)?);
    if session_count > MAX_SESSIONS {
        return Err(MdsError::TooMany("sessions"));
    }
    let sessions_at = blocks(
        bytes.len(),
        u32_at(bytes, 80).ok_or(MdsError::NotMds)?,
        session_count,
        SESSION,
        "session blocks",
    )?;
    let has_dpm = u32_at(bytes, 84).is_some_and(|offset| offset != 0);

    let mut sessions = Vec::with_capacity(session_count);
    let mut tracks: Vec<MdsTrack> = Vec::new();
    for index in 0..session_count {
        let at = sessions_at + index * SESSION;
        let field = |value: Option<u32>| value.ok_or(MdsError::OutOfBounds("session block"));
        let number =
            u32::from(u16_at(bytes, at + 8).ok_or(MdsError::OutOfBounds("session block"))?);
        let session = MdsSession {
            number,
            start: i64::from(i32_at(bytes, at).ok_or(MdsError::OutOfBounds("session block"))?),
            end: i64::from(i32_at(bytes, at + 4).ok_or(MdsError::OutOfBounds("session block"))?),
        };
        let block_count = usize::from(bytes[at + 10]);
        let tracks_at = blocks(
            bytes.len(),
            field(u32_at(bytes, at + 20))?,
            block_count,
            TRACK,
            "track blocks",
        )?;
        for block in 0..block_count {
            let at = tracks_at + block * TRACK;
            let point = bytes[at + 4];
            if !(1..=99).contains(&point) {
                // A lead-in entry (A0 to A2): what the TOC says, not a track.
                continue;
            }
            let track_number = u32::from(point);
            if tracks.len() >= MAX_TRACKS {
                return Err(MdsError::TooMany("tracks"));
            }
            let mode = mode_of(bytes[at]).ok_or(MdsError::Mode {
                track: track_number,
                mode: bytes[at],
            })?;
            let subchannel = if bytes[at + 1] == 0x08 {
                Subchannel::PwInterleaved
            } else {
                Subchannel::None
            };
            let read = |value: Option<u32>| value.ok_or(MdsError::OutOfBounds("track block"));
            let extra = read(u32_at(bytes, at + 12))?;
            let sector_size =
                u32::from(u16_at(bytes, at + 16).ok_or(MdsError::OutOfBounds("track block"))?);
            let start_sector =
                i64::from(i32_at(bytes, at + 36).ok_or(MdsError::OutOfBounds("track block"))?);
            let start_offset =
                u64_at(bytes, at + 40).ok_or(MdsError::OutOfBounds("track block"))?;
            let file_count = usize::try_from(read(u32_at(bytes, at + 48))?)
                .map_err(|_| MdsError::TooMany("data files"))?;
            if file_count > MAX_FILES {
                return Err(MdsError::TooMany("data files"));
            }
            // On a CD the extra block holds the pregap and length; on a DVD
            // its offset field holds the length itself.
            let (pregap, length) = if medium.is_dvd() {
                (0, u64::from(extra))
            } else {
                let at = blocks(bytes.len(), extra, 1, EXTRA, "extra block")?;
                (
                    u64::from(read(u32_at(bytes, at))?),
                    u64::from(read(u32_at(bytes, at + 4))?),
                )
            };
            let mut files = Vec::with_capacity(file_count);
            if file_count > 0 {
                let footers_at = blocks(
                    bytes.len(),
                    read(u32_at(bytes, at + 52))?,
                    file_count,
                    FOOTER,
                    "footers",
                )?;
                for file in 0..file_count {
                    files.push(file_name(bytes, footers_at + file * FOOTER, track_number)?);
                }
            }
            tracks.push(MdsTrack {
                number: track_number,
                session: number,
                mode,
                subchannel,
                ctl: bytes[at + 2] & 0x0F,
                sector_size,
                start_sector,
                start_offset,
                pregap,
                length,
                files,
            });
        }
        sessions.push(session);
    }
    if tracks.is_empty() {
        return Err(MdsError::NoTracks);
    }
    Ok(MdsDescriptor {
        version,
        medium,
        sessions,
        tracks,
        has_dpm,
    })
}

/// The data file a declared name means: `*.mdf` (any case) is the
/// descriptor's own name with its extension `.mdf`; anything else is itself.
#[must_use]
pub fn data_file_name(declared: &str, descriptor: &str) -> String {
    if let Some(extension) = declared.strip_prefix("*.") {
        let stem = descriptor
            .rsplit_once('.')
            .map_or(descriptor, |(stem, _)| stem);
        format!("{stem}.{extension}")
    } else {
        declared.to_owned()
    }
}

/// The CUE mode a track's sectors are, or `None` for a size no CUE mode
/// names for it.
fn cue_mode(mode: TrackMode, size: u32) -> Option<&'static str> {
    match (mode, size) {
        (TrackMode::Audio, 2352) => Some("AUDIO"),
        (TrackMode::Mode1, 2352) => Some("MODE1/2352"),
        (TrackMode::Mode1, 2048) => Some("MODE1/2048"),
        (TrackMode::Mode2 | TrackMode::Mode2Form1 | TrackMode::Mode2Form2, 2352) => {
            Some("MODE2/2352")
        }
        (TrackMode::Mode2, 2336) => Some("MODE2/2336"),
        _ => None,
    }
}

/// The flags a track's control nibble sets, as a CUE sheet writes them.
fn flags(track: &MdsTrack) -> Vec<TrackFlag> {
    let audio = track.mode == TrackMode::Audio;
    let mut flags = Vec::new();
    if track.ctl & 0x02 != 0 {
        flags.push(TrackFlag::DigitalCopyPermitted);
    }
    if audio && track.ctl & 0x08 != 0 {
        flags.push(TrackFlag::FourChannel);
    }
    if audio && track.ctl & 0x01 != 0 {
        flags.push(TrackFlag::PreEmphasis);
    }
    flags
}

/// Lay a CD image without subchannel out like a CUE sheet's tracks over its
/// one data file, of `data_bytes` bytes. A first pregap of 150 sectors is
/// the disc's usual two seconds, which a CUE sheet leaves unsaid; later
/// pregaps are generated, as PREGAP is.
///
/// # Errors
///
/// [`MdsLayoutError`] saying why the image is not laid out.
pub fn layout(descriptor: &MdsDescriptor, data_bytes: u64) -> Result<CdLayout, MdsLayoutError> {
    if descriptor.medium.is_dvd() {
        return Err(MdsLayoutError::Dvd);
    }
    if descriptor.sessions.len() != 1 {
        return Err(MdsLayoutError::Sessions(descriptor.sessions.len()));
    }
    let first_file = descriptor
        .tracks
        .first()
        .and_then(|track| track.files.first());
    let mut laid = Vec::with_capacity(descriptor.tracks.len());
    let mut disc_next: i64 = 0;
    let mut file_next: u64 = 0;
    for (position, track) in descriptor.tracks.iter().enumerate() {
        if track.subchannel == Subchannel::PwInterleaved {
            return Err(MdsLayoutError::Subchannel(track.number));
        }
        if track.files.len() != 1 || track.files.first() != first_file {
            return Err(MdsLayoutError::Files);
        }
        let mode = cue_mode(track.mode, track.sector_size).ok_or(MdsLayoutError::SectorSize {
            track: track.number,
            mode: track.mode,
            size: track.sector_size,
        })?;
        let pregap = if position == 0 {
            if track.pregap != 150 {
                return Err(MdsLayoutError::FirstPregap(track.pregap));
            }
            0
        } else {
            track.pregap
        };
        let expected_start = disc_next
            .checked_add(i64::try_from(pregap).map_err(|_| MdsLayoutError::Order(track.number))?)
            .ok_or(MdsLayoutError::Order(track.number))?;
        if track.start_sector != expected_start || track.start_offset != file_next {
            return Err(MdsLayoutError::Order(track.number));
        }
        let bytes = track
            .length
            .checked_mul(u64::from(track.sector_size))
            .and_then(|span| span.checked_add(track.start_offset))
            .ok_or(MdsLayoutError::PastEnd(track.number))?;
        if bytes > data_bytes || track.length == 0 {
            return Err(MdsLayoutError::PastEnd(track.number));
        }
        let start_lba =
            u64::try_from(track.start_sector).map_err(|_| MdsLayoutError::Order(track.number))?;
        laid.push(TrackLayout {
            number: track.number,
            session: 1,
            mode: mode.to_owned(),
            file: 0,
            file_offset_bytes: track.start_offset,
            start_lba,
            sector_count: track.length,
            pregap_sectors: pregap,
            // The pregap is not in the file, so INDEX 01 is its first sector.
            indexes: vec![(1, 0)],
            isrc: None,
            // Raw sectors, as a CUE sheet's BINARY file.
            sample_byte_order: Some(SampleByteOrder::LittleEndian),
            flags: flags(track),
        });
        disc_next = track
            .start_sector
            .saturating_add(i64::try_from(track.length).unwrap_or(i64::MAX));
        file_next = bytes;
    }
    Ok(CdLayout {
        catalog: None,
        tracks: laid,
        session_count: 1,
        warnings: Vec::new(),
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    const RAW: u64 = 2352;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/../../fixtures/images/mds/{name}.mds",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    /// One track as libmirage read it.
    struct Read {
        file: String,
        session: u32,
        number: u32,
        kind: String,
        start: i64,
        length: u64,
        pregap: u64,
        ctl: u8,
        offset: u64,
        size: u32,
        sub: u32,
    }

    /// libmirage's reading of every fixture, one line per track.
    fn libmirage() -> Vec<Read> {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/tool-output/libmirage/mds-layouts.txt"
        ))
        .unwrap();
        let mut current = String::new();
        let mut rows = Vec::new();
        for line in text.lines() {
            if !line.starts_with(' ') {
                current = line
                    .split(':')
                    .next()
                    .unwrap()
                    .trim_end_matches(".mds")
                    .to_owned();
                continue;
            }
            let (head, fragments) = line.split_once('|').unwrap();
            let words: Vec<&str> = head.split_whitespace().collect();
            // The data is the last fragment, after any generated pregap.
            let data = fragments.rsplit(';').next().unwrap();
            let field = |name: &str| {
                data.split_whitespace()
                    .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
                    .unwrap()
                    .to_owned()
            };
            rows.push(Read {
                file: current.clone(),
                session: words[1].parse().unwrap(),
                number: words[3].parse().unwrap(),
                kind: words[5].to_owned(),
                start: words[7].parse().unwrap(),
                length: words[9].parse().unwrap(),
                pregap: words[11].parse().unwrap(),
                ctl: words[13].parse().unwrap(),
                offset: field("off").parse().unwrap(),
                size: field("size").parse().unwrap(),
                sub: field("sub").parse().unwrap(),
            });
        }
        rows
    }

    #[test]
    fn every_fixture_reads_as_libmirage_reads_it() {
        let rows = libmirage();
        assert_eq!(rows.len(), 9, "every track of every fixture");
        for read in rows {
            let descriptor = parse(&fixture(&read.file)).unwrap();
            let track = descriptor
                .tracks
                .iter()
                .find(|track| track.number == read.number)
                .unwrap_or_else(|| panic!("{} track {}", read.file, read.number));
            let context = format!("{} track {}", read.file, read.number);
            assert_eq!(track.session, read.session, "{context}");
            let mode = match track.mode {
                TrackMode::Audio => "audio",
                TrackMode::Mode1 => "mode1",
                TrackMode::Mode2 => "mode2",
                TrackMode::Mode2Form1 => "mode2-form1",
                TrackMode::Mode2Form2 => "mode2-form2",
            };
            assert_eq!(mode, read.kind, "{context}");
            // libmirage's track starts where its pregap does, and counts it.
            assert_eq!(
                track.start_sector - i64::try_from(track.pregap).unwrap(),
                read.start,
                "{context}"
            );
            assert_eq!(track.pregap + track.length, read.length, "{context}");
            assert_eq!(track.pregap, read.pregap, "{context}");
            assert_eq!(track.ctl, read.ctl, "{context}");
            assert_eq!(track.start_offset, read.offset, "{context}");
            let interleaved = u32::from(track.subchannel == Subchannel::PwInterleaved) * 96;
            assert_eq!(
                (track.sector_size - interleaved, interleaved),
                (read.size, read.sub),
                "{context}"
            );
        }
    }

    #[test]
    fn reads_media_versions_and_names() {
        let dvd = parse(&fixture("dvd")).unwrap();
        assert_eq!(dvd.medium, Medium::DvdRom);
        assert_eq!(dvd.version, (1, 5));
        assert_eq!(dvd.tracks[0].length, 64);
        assert_eq!(dvd.tracks[0].files, ["*.mdf"]);
        assert!(!dvd.has_dpm);
        let wide = parse(&fixture("wide-name")).unwrap();
        assert_eq!(wide.tracks[0].files, ["Wide Name.mdf"]);
        assert_eq!(data_file_name("*.mdf", "Game.mds"), "Game.mdf");
        assert_eq!(data_file_name("*.MDF", "Game.v2.MDS"), "Game.v2.MDF");
        assert_eq!(data_file_name("Other.mdf", "Game.mds"), "Other.mdf");
        assert_eq!(parse(&fixture("two-sessions")).unwrap().sessions.len(), 2);
    }

    #[test]
    fn lays_out_a_cd_like_a_cue_sheets_tracks_with_generated_pregaps() {
        let descriptor = parse(&fixture("mixed-mode")).unwrap();
        let laid = layout(&descriptor, 450 * RAW).unwrap();
        let tracks = &laid.tracks;
        assert_eq!(tracks.len(), 3);
        assert_eq!(
            (
                tracks[0].mode.as_str(),
                tracks[0].start_lba,
                tracks[0].pregap_sectors
            ),
            ("MODE1/2352", 0, 0)
        );
        assert_eq!(tracks[0].sector_count, 300);
        // Track 2's pregap is on the disc, not in the file.
        assert_eq!(
            (
                tracks[1].mode.as_str(),
                tracks[1].start_lba,
                tracks[1].pregap_sectors
            ),
            ("AUDIO", 450, 150)
        );
        assert_eq!(tracks[1].file_offset_bytes, 300 * RAW);
        assert_eq!(tracks[1].indexes, [(1, 0)]);
        assert_eq!(
            (
                tracks[2].start_lba,
                tracks[2].file_offset_bytes,
                tracks[2].sector_count
            ),
            (550, 400 * RAW, 50)
        );
        assert_eq!(
            tracks[1].sample_byte_order,
            Some(SampleByteOrder::LittleEndian)
        );
        assert_eq!(
            layout(&descriptor, 450 * RAW - 1),
            Err(MdsLayoutError::PastEnd(3))
        );
    }

    #[test]
    fn says_why_an_image_is_not_laid_out() {
        let not = |name: &str| layout(&parse(&fixture(name)).unwrap(), u64::MAX).unwrap_err();
        assert_eq!(not("dvd"), MdsLayoutError::Dvd);
        assert_eq!(not("subchannel"), MdsLayoutError::Subchannel(1));
        assert_eq!(not("two-sessions"), MdsLayoutError::Sessions(2));
        let mut odd = parse(&fixture("mixed-mode")).unwrap();
        odd.tracks[0].pregap = 0;
        assert_eq!(layout(&odd, u64::MAX), Err(MdsLayoutError::FirstPregap(0)));
        let mut gap = parse(&fixture("mixed-mode")).unwrap();
        gap.tracks[2].start_offset += 1;
        assert_eq!(layout(&gap, u64::MAX), Err(MdsLayoutError::Order(3)));
        let mut size = parse(&fixture("mixed-mode")).unwrap();
        size.tracks[1].sector_size = 2048;
        assert!(matches!(
            layout(&size, u64::MAX),
            Err(MdsLayoutError::SectorSize { track: 2, .. })
        ));
    }

    #[test]
    fn refuses_what_is_not_a_version_one_descriptor() {
        assert_eq!(parse(b"").unwrap_err(), MdsError::NotMds);
        assert_eq!(parse(&[0; 200]).unwrap_err(), MdsError::NotMds);
        let mut v2 = fixture("single-data");
        v2[16] = 2;
        assert_eq!(parse(&v2).unwrap_err(), MdsError::Version(2, 5));
        let mut medium = fixture("single-data");
        medium[18] = 0x20;
        assert_eq!(parse(&medium).unwrap_err(), MdsError::Medium(0x20));
        assert_eq!(
            parse(&vec![0; MAX_DESCRIPTOR_BYTES + 1]).unwrap_err(),
            MdsError::TooLarge
        );
    }

    #[test]
    fn refuses_offsets_and_counts_outside_the_descriptor() {
        let good = fixture("mixed-mode");
        // Every truncation either reads the same or is refused, never panics.
        for cut in 0..good.len() {
            let _ = parse(&good[..cut]);
        }
        let mut sessions = good.clone();
        sessions[80..84].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            parse(&sessions).unwrap_err(),
            MdsError::OutOfBounds("session blocks")
        );
        let mut many = good.clone();
        many[20..22].copy_from_slice(&500_u16.to_le_bytes());
        assert_eq!(parse(&many).unwrap_err(), MdsError::TooMany("sessions"));
        let mut extra = good.clone();
        // The first real track block is the fourth, after A0, A1 and A2.
        let track = 88 + 24 + 3 * 80;
        extra[track + 12..track + 16].copy_from_slice(&9_999_999_u32.to_le_bytes());
        assert_eq!(
            parse(&extra).unwrap_err(),
            MdsError::OutOfBounds("extra block")
        );
    }

    #[test]
    fn refuses_a_data_file_name_that_is_not_a_plain_name() {
        for bad in [
            "../escape.mdf",
            "/abs.mdf",
            "dir/x.mdf",
            "c:x.mdf",
            "a\\b.mdf",
            "..",
        ] {
            let mut bytes = fixture("single-data");
            let name_at = bytes.len() - "*.mdf".len() - 1;
            bytes.truncate(name_at);
            bytes.extend_from_slice(bad.as_bytes());
            bytes.push(0);
            assert_eq!(
                parse(&bytes).unwrap_err(),
                MdsError::FileName { track: 1 },
                "{bad}"
            );
        }
    }
}
