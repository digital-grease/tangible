// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Writing CUE sheets: what the CUE/BIN made from a CloneCD or Alcohol 120%
//! image is described by (docs/plans/ccd-images.md C3, mds-images.md M3). A
//! sheet written from a layout must read back, through the same parser
//! imports use, as the same layout.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use tangible_image::cue::{SheetError, layout, parse, write_sheet};

#[test]
fn a_sheet_written_from_a_layout_reads_back_as_the_same_tracks() {
    let text: &[u8] =
        b"CATALOG 0123456789012\nFILE \"Game.bin\" BINARY\n  TRACK 01 MODE1/2352\n    \
        INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    FLAGS DCP PRE\n    ISRC USRC17607839\n    \
        INDEX 00 00:04:00\n    INDEX 01 01:00:74\n";
    let laid = layout(&parse(text).unwrap(), &[6000 * 2352]).unwrap();
    let written = write_sheet(&laid, "Game.bin").unwrap();
    assert_eq!(written.as_bytes(), text, "{written}");
    let back = layout(&parse(written.as_bytes()).unwrap(), &[6000 * 2352]).unwrap();
    assert_eq!(back.tracks, laid.tracks);
    assert_eq!(back.catalog, laid.catalog);
}

#[test]
fn a_generated_pregap_is_written_as_pregap_and_reads_back_the_same() {
    // An Alcohol 120% image's tracks are laid out so (docs/plans/mds-images.md).
    let text: &[u8] = b"FILE \"Game.bin\" BINARY\n  TRACK 01 MODE1/2352\n    \
        INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    PREGAP 00:02:00\n    INDEX 01 00:04:00\n  \
        TRACK 03 AUDIO\n    INDEX 01 00:05:25\n";
    let laid = layout(&parse(text).unwrap(), &[450 * 2352]).unwrap();
    assert_eq!(
        (laid.tracks[1].start_lba, laid.tracks[1].pregap_sectors),
        (450, 150)
    );
    assert_eq!(laid.tracks[2].start_lba, 550, "the disc stays ahead");
    let written = write_sheet(&laid, "Game.bin").unwrap();
    assert_eq!(written.as_bytes(), text, "{written}");

    // A gap partly generated and partly in the file is not one a sheet says.
    let mut split = laid;
    split.tracks[1].pregap_sectors = 151;
    assert!(matches!(
        write_sheet(&split, "Game.bin"),
        Err(SheetError::NotExpressible { track: 2, .. })
    ));
}

#[test]
fn a_layout_a_sheet_cannot_say_exactly_is_not_written() {
    let laid = layout(
        &parse(b"FILE \"a.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n").unwrap(),
        &[300 * 2352],
    )
    .unwrap();
    assert!(write_sheet(&laid, "a.bin").is_ok());
    for name in ["", "a\"b.bin", "a\nb.bin"] {
        assert_eq!(
            write_sheet(&laid, name),
            Err(SheetError::UnquotableName),
            "{name:?}"
        );
    }
    let mut other = laid.clone();
    other.tracks[0].file = 1;
    assert!(matches!(
        write_sheet(&other, "a.bin"),
        Err(SheetError::NotExpressible { track: 1, .. })
    ));
    let mut cooked = laid.clone();
    cooked.tracks[0].mode = "MODE1/2048".to_owned();
    assert!(matches!(
        write_sheet(&cooked, "a.bin"),
        Err(SheetError::NotExpressible { .. })
    ));
    let mut generated = laid.clone();
    generated.tracks[0].start_lba = 2;
    assert!(matches!(
        write_sheet(&generated, "a.bin"),
        Err(SheetError::NotExpressible { .. })
    ));
    let mut empty = laid;
    empty.tracks.clear();
    assert_eq!(write_sheet(&empty, "a.bin"), Err(SheetError::NoTracks));
}
