// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! cdrdao tables of contents, checked against cdrdao itself.
//!
//! Each TOC under `fixtures/tool-output/cdrdao/` has beside it what cdrdao
//! 1.2.4's `show-toc` printed for it: where every track starts and ends, its
//! pregap, its indexes, its codes and flags. The layout this crate computes
//! from the same text has to say the same thing, sector for sector, or a disc
//! imported from a TOC would be recorded, and burned, differently from how
//! cdrdao reads it.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::PathBuf;

use tangible_domain::LogicalPath;
use tangible_domain::cd::{SampleByteOrder, TrackFlag};
use tangible_image::toc::{self, TocError, TocLayoutError, TocWarning};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/tool-output/cdrdao")
}

fn read(name: &str) -> Vec<u8> {
    std::fs::read(fixtures().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// What `show-toc` said about one track.
#[derive(Debug, Default)]
struct Shown {
    start: u64,
    end: u64,
    pregap: u64,
    indexes: Vec<(u32, u64)>,
    isrc: Option<String>,
    copy: bool,
    pre_emphasis: bool,
    four_channel: bool,
}

/// The number in brackets at the end of a `show-toc` position line.
fn sectors(line: &str) -> u64 {
    line.rsplit_once('(')
        .and_then(|(_, rest)| rest.trim_end_matches(')').trim().parse().ok())
        .unwrap_or_else(|| panic!("no sector count in {line:?}"))
}

/// Read `show-toc` output into one entry per track, and the catalogue.
fn shown(name: &str) -> (Vec<Shown>, Option<String>) {
    let text = String::from_utf8(read(name)).expect("text");
    let mut tracks: Vec<Shown> = Vec::new();
    let mut catalog = None;
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("CATALOG NUMBER:") {
            catalog = Some(rest.trim().to_owned());
        } else if line.starts_with("TRACK ") {
            tracks.push(Shown::default());
        } else if let Some(track) = tracks.last_mut() {
            if line.starts_with("START") {
                track.start = sectors(line);
            } else if line.starts_with("END") {
                track.end = sectors(line);
            } else if line.starts_with("PREGAP") {
                track.pregap = sectors(line);
            } else if let Some(rest) = line.strip_prefix("INDEX") {
                let number = rest.split_whitespace().next().unwrap().parse().unwrap();
                track.indexes.push((number, sectors(line)));
            } else if let Some(rest) = line.strip_prefix("ISRC") {
                track.isrc = Some(rest.split_whitespace().collect());
            } else if line == "COPY PERMITTED" {
                track.copy = true;
            } else if line == "PRE-EMPHASIS" {
                track.pre_emphasis = true;
            } else if line == "FOUR CHANNEL AUDIO" {
                track.four_channel = true;
            }
        }
    }
    (tracks, catalog)
}

/// Lay out a fixture TOC with the given file sizes and compare it with what
/// cdrdao printed for it.
fn agrees_with_cdrdao(toc_name: &str, shown_name: &str, sizes: &[u64]) -> toc::TocLayout {
    let document = toc::parse(&read(toc_name)).expect("cdrdao read it, so must this");
    let laid = toc::layout(&document, sizes).expect("a layout");
    let (expected, catalog) = shown(shown_name);
    let tracks = &laid.layout.tracks;
    assert_eq!(tracks.len(), expected.len(), "{toc_name}: track count");
    assert_eq!(laid.layout.catalog, catalog, "{toc_name}: catalogue");

    for (track, cdrdao) in tracks.iter().zip(&expected) {
        let number = track.number;
        let index_one = track
            .indexes
            .iter()
            .find(|(n, _)| *n == 1)
            .map(|(_, at)| *at)
            .expect("an INDEX 1");
        assert_eq!(
            track.start_lba + index_one,
            cdrdao.start,
            "{toc_name} track {number}: start"
        );
        assert_eq!(
            track.start_lba + track.sector_count,
            cdrdao.end,
            "{toc_name} track {number}: end"
        );
        assert_eq!(
            track.pregap_sectors, cdrdao.pregap,
            "{toc_name} track {number}: pregap"
        );
        for (n, at) in &cdrdao.indexes {
            assert!(
                track
                    .indexes
                    .iter()
                    .any(|(ours, offset)| ours == n && track.start_lba + offset == *at),
                "{toc_name} track {number}: INDEX {n} at {at}: {:?}",
                track.indexes
            );
        }
        if let Some(isrc) = &cdrdao.isrc
            && isrc.bytes().any(|b| b != b'0')
        {
            assert_eq!(track.isrc.as_deref(), Some(isrc.as_str()), "{toc_name}");
        }
        assert_eq!(
            track.flags.contains(&TrackFlag::DigitalCopyPermitted),
            cdrdao.copy,
            "{toc_name} track {number}: copy"
        );
        assert_eq!(
            track.flags.contains(&TrackFlag::PreEmphasis),
            cdrdao.pre_emphasis,
            "{toc_name} track {number}: pre-emphasis"
        );
        assert_eq!(
            track.flags.contains(&TrackFlag::FourChannel),
            cdrdao.four_channel,
            "{toc_name} track {number}: channels"
        );
    }
    laid
}

const RAW: u64 = 2352;

#[test]
fn audio_tracks_from_two_files_agree_with_cdrdao() {
    let laid = agrees_with_cdrdao(
        "toc/audio.toc",
        "show-toc-audio.txt",
        &[3000 * RAW, 1500 * RAW],
    );
    let tracks = &laid.layout.tracks;
    assert_eq!(tracks[0].file, 0);
    assert_eq!(tracks[1].file, 1);
    // SWAP is how a TOC says the samples are little-endian.
    assert!(
        tracks
            .iter()
            .all(|t| t.sample_byte_order == Some(SampleByteOrder::LittleEndian))
    );
}

#[test]
fn codes_and_an_index_agree_with_cdrdao() {
    agrees_with_cdrdao("toc/codes.toc", "show-toc-codes.txt", &[3000 * RAW]);
}

#[test]
fn a_generated_pregap_agrees_with_cdrdao() {
    let laid = agrees_with_cdrdao("toc/mixed.toc", "show-toc-mixed.txt", &[4000 * RAW]);
    let audio = &laid.layout.tracks[1];
    assert_eq!(audio.mode, "AUDIO");
    assert_eq!(laid.layout.tracks[0].mode, "MODE2/2352");
    assert_eq!(audio.file_offset_bytes, 1000 * RAW);
    assert_eq!(
        audio.indexes,
        vec![(1, 0)],
        "the gap is generated, not in the file"
    );
}

#[test]
fn a_pregap_held_in_the_file_agrees_with_cdrdao() {
    // The shape this project's own TOC writer produces: two FILE statements
    // over adjacent bytes of one file, with START between them.
    let laid = agrees_with_cdrdao(
        "toc/mixed-start.toc",
        "show-toc-mixed-start.txt",
        &[4000 * RAW],
    );
    let audio = &laid.layout.tracks[1];
    assert_eq!(audio.indexes, vec![(0, 0), (1, 150)]);
    assert_eq!(audio.sector_count, 3000);
}

#[test]
fn flags_agree_with_cdrdao() {
    agrees_with_cdrdao("toc/flags.toc", "show-toc-flags.txt", &[4000 * RAW]);
}

#[test]
fn a_toc_cdrdao_wrote_when_reading_a_disc_agrees_with_cdrdao() {
    // From the first disc burned and read back on a real drive: a MODE1 data
    // track stored as 2048-byte blocks, then audio from the same file with a
    // generated two-second gap.
    let data = 1161 * 2048;
    let laid = agrees_with_cdrdao(
        "readback-mixed.toc",
        "show-toc-readback-mixed.txt",
        &[data + 2250 * RAW],
    );
    let tracks = &laid.layout.tracks;
    assert_eq!(tracks[0].mode, "MODE1/2048");
    assert_eq!(tracks[1].file_offset_bytes, data);
    // No SWAP: cdrdao's own files are big-endian.
    assert_eq!(
        tracks[1].sample_byte_order,
        Some(SampleByteOrder::BigEndian)
    );
    // The drive reported no ISRC, as zeros, which is not kept as one.
    assert_eq!(tracks[1].isrc, None);
    assert!(
        laid.warnings
            .contains(&TocWarning::ZeroCode { track: Some(2) })
    );
}

#[test]
fn the_toc_cdrdao_refused_is_refused_here_too() {
    assert!(matches!(
        toc::parse(&read("toc/syntax-error.toc")),
        Err(TocError::Unexpected { line: 5, .. })
    ));
}

#[test]
fn a_track_reaching_past_its_file_is_not_laid_out() {
    let document = toc::parse(&read("toc/too-long.toc")).expect("parses");
    assert!(matches!(
        toc::layout(&document, &[100 * RAW]),
        Err(TocLayoutError::OutsideFile { track: 1, .. })
    ));
}

#[test]
fn files_named_in_a_toc_go_through_the_same_resolver_as_a_cue_sheet() {
    // The TOC names /work/disc.bin; only disc.bin was staged. It is matched by
    // its last segment, with a warning, and nothing else can be reached.
    let document = toc::parse(&read("toc/mixed.toc")).expect("parses");
    let staged = vec![
        LogicalPath::parse("disc.bin").unwrap(),
        LogicalPath::parse("disc.toc").unwrap(),
    ];
    let resolution = tangible_image::resolve_names(&document.files, &staged);
    assert_eq!(
        resolution.paths(),
        Some(vec![LogicalPath::parse("disc.bin").unwrap()])
    );
    assert!(!resolution.warnings.is_empty());

    let escaping = toc::parse(b"TRACK MODE1\nDATAFILE \"../../etc/passwd\"\n").expect("parses");
    let resolution = tangible_image::resolve_names(&escaping.files, &staged);
    assert_eq!(
        resolution.paths(),
        None,
        "a traversal reaches nothing staged"
    );
}

#[test]
fn tracks_the_layout_cannot_record_are_refused_by_name() {
    for (text, sizes, why) in [
        (
            "TRACK AUDIO\nFILE \"a.bin\" 0 0:10:0\nFILE \"b.bin\" 0 0:10:0\n",
            vec![RAW * 750, RAW * 750],
            "more than one file",
        ),
        (
            "TRACK AUDIO\nFILE \"a.bin\" 0 0:10:0\nFILE \"a.bin\" 0:20:0 0:10:0\n",
            vec![RAW * 2250],
            "not one contiguous run",
        ),
        (
            "TRACK AUDIO\nFILE \"a.bin\" 0 0:10:0\nSILENCE 0:2:0\n",
            vec![RAW * 750],
            "silence follows",
        ),
        (
            "TRACK MODE1 RW\nDATAFILE \"a.bin\"\n",
            vec![2144 * 100],
            "subchannel",
        ),
        (
            "TRACK MODE2\nDATAFILE \"a.bin\"\n",
            vec![2336 * 100],
            "formless",
        ),
        (
            "TRACK AUDIO\nFILE \"song.wav\" 0\n",
            vec![RAW * 100],
            "decodes",
        ),
        (
            "TRACK AUDIO\nFILE \"a.bin\" 0 0:10:0\nEND\n",
            vec![RAW * 750],
            "post-gap",
        ),
    ] {
        let document = toc::parse(text.as_bytes()).expect("parses");
        match toc::layout(&document, &sizes) {
            Err(TocLayoutError::NotExpressible { reason, .. }) => {
                assert!(reason.contains(why), "{why}: {reason}");
            }
            other => panic!("{why}: {other:?}"),
        }
    }
}

#[test]
fn positions_outside_a_track_are_refused() {
    for (text, why) in [
        (
            "TRACK AUDIO\nFILE \"a.bin\" 0 0:10:0\nSTART 0:10:0\n",
            "at or past the end",
        ),
        (
            "TRACK AUDIO\nFILE \"a.bin\" 0 0:10:0\nINDEX 0:10:0\n",
            "past the end",
        ),
        (
            "TRACK AUDIO\nFILE \"a.bin\" 0 0:10:0\nINDEX 0:5:0\nINDEX 0:4:0\n",
            "do not advance",
        ),
        (
            "TRACK AUDIO\nFILE \"a.bin\" 0 0:10:0\nSTART 0:2:0\n",
            "first track",
        ),
    ] {
        let document = toc::parse(text.as_bytes()).expect("parses");
        match toc::layout(&document, &[RAW * 750]) {
            Err(TocLayoutError::BadPosition { reason, .. }) => {
                assert!(reason.contains(why), "{why}: {reason}");
            }
            other => panic!("{why}: {other:?}"),
        }
    }
}

#[test]
fn a_file_to_its_end_counts_whole_sectors_and_says_so() {
    let document = toc::parse(b"TRACK MODE1\nDATAFILE \"a.iso\"\n").expect("parses");
    let laid = toc::layout(&document, &[2048 * 300 + 100]).expect("a layout");
    assert_eq!(laid.layout.tracks[0].sector_count, 300);
    assert!(
        laid.warnings
            .contains(&TocWarning::PartialSector { track: 1 })
    );
}

#[test]
fn first_track_no_numbers_the_tracks() {
    let document = toc::parse(
        b"FIRST_TRACK_NO 5\nTRACK AUDIO\nFILE \"a.bin\" 0 0:10:0\nTRACK AUDIO\nFILE \"a.bin\" 0:10:0\n",
    )
    .expect("parses");
    let laid = toc::layout(&document, &[RAW * 1500]).expect("a layout");
    let numbers: Vec<u32> = laid.layout.tracks.iter().map(|t| t.number).collect();
    assert_eq!(numbers, vec![5, 6]);
}
