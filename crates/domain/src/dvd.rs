// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Facts about DVD-Video bytes that more than one part of Tangible reads:
//! the block reader, ripping a disc, and the image inspector, importing a
//! VIDEO_TS tree. Pure byte checks; nothing here reads a file.

/// Bytes in a DVD sector.
pub const SECTOR_BYTES: usize = 2048;

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
