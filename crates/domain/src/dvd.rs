// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Facts about DVD-Video bytes that more than one part of Tangible reads:
//! the block reader, ripping a disc, and the image inspector, importing a
//! VIDEO_TS tree. Pure byte checks; nothing here reads a file.

/// Bytes in a DVD sector.
pub const SECTOR_BYTES: usize = 2048;

/// The most title sets DVD-Video allows.
pub const MAX_TITLE_SETS: u8 = 99;

/// The largest VOB file DVD-Video writes: 1 GiB.
pub const MAX_VOB_BYTES: u64 = 1024 * 1024 * 1024;

/// Whether a sector is CSS-scrambled DVD-Video.
///
/// A VOB sector is an MPEG-2 program stream pack: `00 00 01 BA`, a pack
/// header whose last byte says how much stuffing follows, then a PES packet
/// (`00 00 01`, a stream id, two length bytes) whose next byte carries the
/// two `PES_scrambling_control` bits. Non-zero means scrambled. Anything
/// that is not shaped like that is not a scrambled VOB sector.
#[must_use]
pub fn css_scrambled(sector: &[u8]) -> bool {
    if sector.len() < 32 || sector[..4] != [0x00, 0x00, 0x01, 0xBA] {
        return false;
    }
    // MPEG-2 packs start their fifth byte with the bits 01.
    if sector[4] & 0xC0 != 0x40 {
        return false;
    }
    let pes = 14 + usize::from(sector[13] & 0x07);
    let Some(header) = sector.get(pes..pes + 7) else {
        return false;
    };
    if header[..3] != [0x00, 0x00, 0x01] {
        return false;
    }
    let stream = header[3];
    // Video, MPEG audio, and private stream 1 (AC-3, DTS, LPCM, subpictures):
    // the streams CSS scrambles. Navigation packs (private stream 2) are not.
    let scrambleable = matches!(stream, 0xE0..=0xEF | 0xC0..=0xDF | 0xBD);
    scrambleable && header[6] & 0x30 != 0
}

/// One of the files DVD-Video defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DvdFile {
    /// `VIDEO_TS.IFO`, the video manager.
    VmgIfo,
    /// `VIDEO_TS.BUP`, its backup.
    VmgBup,
    /// `VIDEO_TS.VOB`, the first-play menu.
    VmgMenu,
    /// `VTS_nn_0.IFO`, a title set's information.
    VtsIfo(u8),
    /// `VTS_nn_0.BUP`, its backup.
    VtsBup(u8),
    /// `VTS_nn_0.VOB`, a title set's menus.
    VtsMenu(u8),
    /// `VTS_nn_m.VOB`, `m` from 1 to 9: a title set's video.
    VtsTitle(u8, u8),
}

impl DvdFile {
    /// Classify a file's name, exactly as DVD-Video spells it.
    #[must_use]
    pub fn classify(name: &str) -> Option<Self> {
        match name {
            "VIDEO_TS.IFO" => return Some(Self::VmgIfo),
            "VIDEO_TS.BUP" => return Some(Self::VmgBup),
            "VIDEO_TS.VOB" => return Some(Self::VmgMenu),
            _ => {}
        }
        // VTS_nn_m.EXT, twelve characters.
        let bytes = name.as_bytes();
        if bytes.len() != 12 || !name.starts_with("VTS_") || bytes[6] != b'_' || bytes[8] != b'.' {
            return None;
        }
        let digit = |byte: u8| byte.is_ascii_digit().then(|| byte - b'0');
        let set = digit(bytes[4])? * 10 + digit(bytes[5])?;
        if !(1..=MAX_TITLE_SETS).contains(&set) {
            return None;
        }
        let part = digit(bytes[7])?;
        match (&name[9..], part) {
            ("IFO", 0) => Some(Self::VtsIfo(set)),
            ("BUP", 0) => Some(Self::VtsBup(set)),
            ("VOB", 0) => Some(Self::VtsMenu(set)),
            ("VOB", 1..=9) => Some(Self::VtsTitle(set, part)),
            _ => None,
        }
    }
}
