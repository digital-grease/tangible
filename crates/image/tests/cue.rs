// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! CUE sheets, from text to track extents.
//!
//! The sheets here are written out in full rather than built by a helper.
//! A CUE sheet is text somebody else wrote, and a test that constructs one
//! through a builder is testing the builder's idea of the format instead of
//! the format.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use tangible_domain::LogicalPath;
use tangible_image::cue::{
    self, CueError, CueWarning, LayoutError, MAX_CUE_BYTES, Msf, ReferenceFailure,
};

/// Bytes per sector for every mode used in these tests except MODE1/2048.
const RAW_SECTOR: u64 = 2352;

fn staged(names: &[&str]) -> Vec<LogicalPath> {
    names
        .iter()
        .map(|name| LogicalPath::parse(name).expect("a staged path"))
        .collect()
}

fn sheet(text: &str) -> cue::CueSheet {
    cue::parse(text.as_bytes()).expect("a readable sheet")
}

// --- parsing -------------------------------------------------------------------

#[test]
fn a_single_track_data_sheet_parses() {
    let parsed = sheet(
        r#"FILE "disc.bin" BINARY
  TRACK 01 MODE1/2352
    INDEX 01 00:00:00
"#,
    );

    assert_eq!(parsed.files.len(), 1);
    assert_eq!(parsed.files[0].name, "disc.bin");
    assert_eq!(parsed.files[0].format, "BINARY");
    assert_eq!(parsed.track_count(), 1);
    let track = &parsed.files[0].tracks[0];
    assert_eq!(track.number, 1);
    assert_eq!(track.mode.as_str(), "MODE1/2352");
    assert_eq!(track.mode.sector_bytes(), Some(2352));
    assert!(!track.mode.is_audio());
    assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
}

#[test]
fn a_mixed_mode_sheet_parses() {
    // The shape almost every game CD arrives in: one data track followed by
    // audio, all in one file.
    let parsed = sheet(
        r#"REM GENRE Game
CATALOG 1234567890123
FILE "Example Game (USA).bin" BINARY
  TRACK 01 MODE2/2352
    INDEX 01 00:00:00
  TRACK 02 AUDIO
    PREGAP 00:02:00
    INDEX 01 04:00:00
  TRACK 03 AUDIO
    INDEX 00 06:00:00
    INDEX 01 06:02:00
"#,
    );

    assert_eq!(parsed.catalog.as_deref(), Some("1234567890123"));
    assert_eq!(parsed.track_count(), 3);
    let tracks = &parsed.files[0].tracks;
    assert!(tracks[1].mode.is_audio());
    assert_eq!(
        tracks[1].pregap,
        Some(Msf {
            minutes: 0,
            seconds: 2,
            frames: 0
        })
    );
    assert_eq!(tracks[2].indexes.len(), 2);
    assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    assert!(parsed.confidence > 0.9);
}

#[test]
fn a_name_with_spaces_survives_quoting() {
    let parsed = sheet(
        r#"FILE "My Disc (Disc 1).bin" BINARY
  TRACK 01 AUDIO
    INDEX 01 00:00:00
"#,
    );

    assert_eq!(parsed.files[0].name, "My Disc (Disc 1).bin");
}

#[test]
fn a_windows_written_sheet_parses() {
    // CRLF line endings and a UTF-8 byte order mark, which is what a sheet
    // saved by a Windows text editor looks like.
    let text = "\u{feff}FILE \"disc.bin\" BINARY\r\n  TRACK 01 AUDIO\r\n    INDEX 01 00:00:00\r\n";
    let parsed = cue::parse(text.as_bytes()).expect("a readable sheet");

    assert_eq!(parsed.files[0].name, "disc.bin");
    assert_eq!(parsed.track_count(), 1);
    assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
}

#[test]
fn text_that_is_not_a_sheet_is_refused() {
    assert_eq!(cue::parse(b"").unwrap_err(), CueError::Empty);
    assert_eq!(
        cue::parse(b"REM just a comment\n").unwrap_err(),
        CueError::Empty
    );
    assert_eq!(
        cue::parse(b"CATALOG 1234567890123\n").unwrap_err(),
        CueError::NoFile
    );
}

#[test]
fn an_oversized_sheet_is_refused_without_being_read() {
    let huge = vec![b'\n'; MAX_CUE_BYTES + 1];

    assert_eq!(
        cue::parse(&huge).unwrap_err(),
        CueError::TooLarge {
            bytes: MAX_CUE_BYTES + 1
        }
    );
}

#[test]
fn a_line_longer_than_the_limit_is_refused() {
    let mut text = String::from("FILE \"disc.bin\" BINARY\n");
    text.push_str(&"A".repeat(5000));
    text.push('\n');

    assert_eq!(
        cue::parse(text.as_bytes()).unwrap_err(),
        CueError::LineTooLong { line: 2 }
    );
}

#[test]
fn more_tracks_than_a_cd_holds_is_refused() {
    use std::fmt::Write as _;

    let mut text = String::from("FILE \"disc.bin\" BINARY\n");
    for number in 1..=100 {
        let _ = writeln!(text, "  TRACK {number:02} AUDIO\n    INDEX 01 00:00:00");
    }

    assert!(matches!(
        cue::parse(text.as_bytes()).unwrap_err(),
        CueError::TooManyTracks { .. }
    ));
}

#[test]
fn a_repeated_track_number_is_refused() {
    // Two tracks with one number cannot both be burned, and quietly keeping
    // the second would silently drop the first.
    let text = "FILE \"disc.bin\" BINARY\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n  TRACK 01 AUDIO\n    INDEX 01 01:00:00\n";

    assert_eq!(
        cue::parse(text.as_bytes()).unwrap_err(),
        CueError::DuplicateTrack { number: 1 }
    );
}

#[test]
fn commands_out_of_order_are_refused() {
    assert_eq!(
        cue::parse(b"TRACK 01 AUDIO\n").unwrap_err(),
        CueError::TrackBeforeFile { line: 1 }
    );
    assert!(matches!(
        cue::parse(b"FILE \"d.bin\" BINARY\n  INDEX 01 00:00:00\n").unwrap_err(),
        CueError::CommandBeforeTrack { line: 2, .. }
    ));
}

#[test]
fn a_command_with_unreadable_arguments_is_refused() {
    assert!(matches!(
        cue::parse(b"FILE \"d.bin\" BINARY\n  TRACK 01 AUDIO\n    INDEX 01 not-a-timecode\n")
            .unwrap_err(),
        CueError::Malformed { line: 3, .. }
    ));
    assert!(matches!(
        cue::parse(b"FILE \"d.bin\" BINARY\n  TRACK ohno AUDIO\n").unwrap_err(),
        CueError::Malformed { line: 2, .. }
    ));
}

#[test]
fn an_unknown_command_is_kept_as_a_warning() {
    // Sheets carry vendor extensions. Refusing one would refuse the disc.
    let parsed = sheet(
        "FILE \"d.bin\" BINARY\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n  SOMETHINGELSE 4\n",
    );

    assert!(parsed.warnings.contains(&CueWarning::UnknownCommand {
        line: 4,
        command: "SOMETHINGELSE".to_owned()
    }));
}

#[test]
fn an_impossible_timecode_is_warned() {
    // 75 frames is one second, so there is no frame 75.
    let parsed = sheet("FILE \"d.bin\" BINARY\n  TRACK 01 AUDIO\n    INDEX 01 00:00:75\n");

    assert!(
        parsed
            .warnings
            .iter()
            .any(|warning| matches!(warning, CueWarning::ImpossibleTimecode { track: 1, .. }))
    );
}

#[test]
fn a_track_without_an_index_one_is_warned() {
    let parsed = sheet("FILE \"d.bin\" BINARY\n  TRACK 01 AUDIO\n    INDEX 00 00:00:00\n");

    assert!(
        parsed
            .warnings
            .contains(&CueWarning::MissingIndexOne { track: 1 })
    );
}

#[test]
fn track_numbering_that_skips_is_warned() {
    let parsed = sheet(
        "FILE \"d.bin\" BINARY\n  TRACK 02 AUDIO\n    INDEX 01 00:00:00\n  TRACK 05 AUDIO\n    INDEX 01 01:00:00\n",
    );

    assert!(
        parsed
            .warnings
            .contains(&CueWarning::FirstTrackIsNotOne { found: 2 })
    );
    assert!(
        parsed
            .warnings
            .contains(&CueWarning::TrackNumbersNotSequential {
                expected: 3,
                found: 5
            })
    );
}

#[test]
fn an_unrecognised_mode_is_warned_and_carries_no_sector_size() {
    let parsed = sheet("FILE \"d.bin\" BINARY\n  TRACK 01 MODE9/9999\n    INDEX 01 00:00:00\n");

    assert_eq!(parsed.files[0].tracks[0].mode.sector_bytes(), None);
    assert!(
        parsed
            .warnings
            .iter()
            .any(|warning| matches!(warning, CueWarning::UnknownTrackMode { track: 1, .. }))
    );
}

#[test]
fn a_second_session_is_read_from_the_only_place_it_is_written() {
    let parsed = sheet(
        r#"FILE "d.bin" BINARY
  TRACK 01 MODE2/2352
    INDEX 01 00:00:00
REM SESSION 02
  TRACK 02 MODE2/2352
    INDEX 01 04:00:00
"#,
    );

    assert_eq!(parsed.files[0].tracks[0].session, 1);
    assert_eq!(parsed.files[0].tracks[1].session, 2);
}

#[test]
fn text_that_is_not_utf8_is_read_lossily_and_said_so() {
    let mut text = b"FILE \"d.bin\" BINARY\n  TRACK 01 AUDIO\n    TITLE \"".to_vec();
    text.extend_from_slice(&[0xff, 0xfe]);
    text.extend_from_slice(b"\"\n    INDEX 01 00:00:00\n");

    let parsed = cue::parse(&text).expect("a readable sheet");

    assert!(parsed.warnings.contains(&CueWarning::NonUtf8Text));
    assert_eq!(parsed.track_count(), 1);
}

#[test]
fn a_gap_declared_twice_is_warned() {
    // PREGAP says the burner generates it; INDEX 00 says the file holds it.
    // A track claiming both is describing two different gaps.
    let parsed = sheet(
        "FILE \"d.bin\" BINARY\n  TRACK 01 AUDIO\n    PREGAP 00:02:00\n    INDEX 00 00:00:00\n    INDEX 01 00:02:00\n",
    );

    assert!(
        parsed
            .warnings
            .contains(&CueWarning::PregapAndIndexZero { track: 1 })
    );
}

#[test]
fn a_file_that_is_not_raw_sectors_is_warned() {
    let parsed = sheet("FILE \"track01.wav\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n");

    assert!(
        parsed
            .warnings
            .iter()
            .any(|warning| matches!(warning, CueWarning::NotRawSectors { .. }))
    );
}

#[test]
fn a_timecode_is_seventy_five_frames_to_the_second() {
    assert_eq!(
        Msf {
            minutes: 0,
            seconds: 0,
            frames: 0
        }
        .to_lba(),
        0
    );
    assert_eq!(
        Msf {
            minutes: 0,
            seconds: 2,
            frames: 0
        }
        .to_lba(),
        150
    );
    assert_eq!(
        Msf {
            minutes: 1,
            seconds: 0,
            frames: 0
        }
        .to_lba(),
        4500
    );
    assert_eq!(
        Msf {
            minutes: 74,
            seconds: 0,
            frames: 0
        }
        .to_lba(),
        333_000
    );
    assert_eq!(
        Msf {
            minutes: 0,
            seconds: 0,
            frames: 74
        }
        .to_lba(),
        74
    );
}

// --- resolving the files a sheet names -----------------------------------------

#[test]
fn a_name_that_matches_a_staged_file_resolves() {
    let parsed = sheet("FILE \"disc.bin\" BINARY\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n");
    let staged = staged(&["disc.cue", "disc.bin"]);

    let resolution = cue::resolve_references(&parsed, &staged);

    assert!(resolution.all_resolved());
    assert_eq!(
        resolution.files[0].resolved.as_ref().unwrap().as_str(),
        "disc.bin"
    );
    assert!(resolution.warnings.is_empty(), "{:?}", resolution.warnings);
}

#[test]
fn a_traversal_can_only_ever_reach_a_staged_file() {
    // The property that matters, stated as a test: whatever the sheet says,
    // the resolver returns a staged file or nothing. There is no third
    // outcome, and no path in it is built by joining the sheet's text onto
    // anything.
    let hostile = [
        "../../etc/passwd",
        "/etc/passwd",
        "..\\..\\windows\\system32\\config\\sam",
        "C:\\Windows\\System32\\drivers\\etc\\hosts",
        "\\\\server\\share\\secret.bin",
        "./disc.bin",
        "",
    ];
    let staged = staged(&["disc.bin"]);

    for name in hostile {
        let text = format!("FILE \"{name}\" BINARY\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n");
        let parsed = cue::parse(text.as_bytes()).expect("a readable sheet");
        let resolution = cue::resolve_references(&parsed, &staged);

        match resolution.files[0].resolved.as_ref() {
            Some(path) => assert!(
                staged.contains(path),
                "{name} resolved to {path:?}, which was never staged"
            ),
            None => assert!(resolution.files[0].failure.is_some()),
        }
    }
}

#[test]
fn a_windows_path_is_reduced_to_the_file_it_could_only_have_meant() {
    // Real sheets carry the absolute path of the machine they were dumped on.
    // The last segment is the only part that could ever name a staged file.
    let parsed = sheet(
        "FILE \"D:\\dumps\\old\\TRACK01.BIN\" BINARY\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n",
    );
    let staged = staged(&["TRACK01.BIN"]);

    let resolution = cue::resolve_references(&parsed, &staged);

    assert_eq!(
        resolution.files[0].resolved.as_ref().unwrap().as_str(),
        "TRACK01.BIN"
    );
    assert!(
        resolution
            .warnings
            .iter()
            .any(|warning| matches!(warning, CueWarning::MatchedByFileName { .. }))
    );
}

#[test]
fn a_backslash_separated_name_resolves_and_says_so() {
    let parsed =
        sheet("FILE \"data\\track01.bin\" BINARY\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n");
    let staged = staged(&["data/track01.bin"]);

    let resolution = cue::resolve_references(&parsed, &staged);

    assert_eq!(
        resolution.files[0].resolved.as_ref().unwrap().as_str(),
        "data/track01.bin"
    );
    assert!(
        resolution
            .warnings
            .iter()
            .any(|warning| matches!(warning, CueWarning::BackslashSeparators { .. }))
    );
}

#[test]
fn a_name_that_differs_only_in_case_resolves_and_says_so() {
    let parsed = sheet("FILE \"TRACK01.BIN\" BINARY\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n");
    let staged = staged(&["track01.bin"]);

    let resolution = cue::resolve_references(&parsed, &staged);

    assert_eq!(
        resolution.files[0].resolved.as_ref().unwrap().as_str(),
        "track01.bin"
    );
    assert!(
        resolution
            .warnings
            .iter()
            .any(|warning| matches!(warning, CueWarning::CaseInsensitiveMatch { .. }))
    );
}

#[test]
fn a_name_nothing_staged_matches_is_reported_as_missing() {
    let parsed = sheet("FILE \"absent.bin\" BINARY\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n");
    let staged = staged(&["disc.bin"]);

    let resolution = cue::resolve_references(&parsed, &staged);

    assert!(!resolution.all_resolved());
    assert_eq!(
        resolution.files[0].failure,
        Some(ReferenceFailure::NotStaged)
    );
    assert!(resolution.paths().is_none());
}

#[test]
fn an_ambiguous_name_is_refused_rather_than_guessed() {
    let parsed = sheet("FILE \"track01.bin\" BINARY\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n");
    let staged = staged(&["a/track01.bin", "b/track01.bin"]);

    let resolution = cue::resolve_references(&parsed, &staged);

    assert!(matches!(
        resolution.files[0].failure,
        Some(ReferenceFailure::Ambiguous { .. })
    ));
}

#[test]
fn two_references_to_one_file_are_refused() {
    // Two FILE entries pointing at one BIN would give two sets of tracks the
    // same bytes at offsets that cannot both be right.
    let parsed = sheet(
        r#"FILE "disc.bin" BINARY
  TRACK 01 AUDIO
    INDEX 01 00:00:00
FILE "DISC.BIN" BINARY
  TRACK 02 AUDIO
    INDEX 01 00:00:00
"#,
    );
    let staged = staged(&["disc.bin"]);

    let resolution = cue::resolve_references(&parsed, &staged);

    assert!(resolution.files[0].resolved.is_some());
    assert!(matches!(
        resolution.files[1].failure,
        Some(ReferenceFailure::AlreadyClaimed { .. })
    ));
}

// --- laying out the tracks -----------------------------------------------------

#[test]
fn a_single_track_disc_lays_out() {
    let parsed = sheet("FILE \"d.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n");

    let layout = cue::layout(&parsed, &[RAW_SECTOR * 1000]).expect("a layout");

    assert_eq!(layout.tracks.len(), 1);
    assert_eq!(layout.tracks[0].start_lba, 0);
    assert_eq!(layout.tracks[0].sector_count, 1000);
    assert_eq!(layout.tracks[0].file_offset_bytes, 0);
    assert_eq!(layout.session_count, 1);
    assert_eq!(layout.total_sectors(), 1000);
    assert!(layout.warnings.is_empty(), "{:?}", layout.warnings);
}

#[test]
fn a_mixed_mode_disc_lays_out() {
    // One file, a data track, then audio after a generated two second gap.
    let parsed = sheet(
        r#"FILE "game.bin" BINARY
  TRACK 01 MODE2/2352
    INDEX 01 00:00:00
  TRACK 02 AUDIO
    PREGAP 00:02:00
    INDEX 01 04:00:00
"#,
    );

    let layout = cue::layout(&parsed, &[RAW_SECTOR * 20_000]).expect("a layout");

    let data = &layout.tracks[0];
    assert_eq!(data.start_lba, 0);
    assert_eq!(data.sector_count, 18_000);
    assert_eq!(data.file_offset_bytes, 0);
    assert_eq!(data.pregap_sectors, 0);

    // The generated gap is on the disc but not in the file: the audio track's
    // bytes start where the data track's end, and its disc position is 150
    // sectors further on than that.
    let audio = &layout.tracks[1];
    assert_eq!(audio.file_offset_bytes, 18_000 * RAW_SECTOR);
    assert_eq!(audio.start_lba, 18_150);
    assert_eq!(audio.sector_count, 2_000);
    assert_eq!(audio.pregap_sectors, 150);
}

#[test]
fn a_pregap_that_is_in_the_file_is_counted_in_the_track() {
    // INDEX 00 says the gap is present in the file, which is the difference
    // that decides whether the burner generates silence or writes what is
    // there.
    let parsed = sheet(
        r#"FILE "game.bin" BINARY
  TRACK 01 MODE2/2352
    INDEX 01 00:00:00
  TRACK 02 AUDIO
    INDEX 00 04:00:00
    INDEX 01 04:02:00
"#,
    );

    let layout = cue::layout(&parsed, &[RAW_SECTOR * 20_000]).expect("a layout");

    let audio = &layout.tracks[1];
    assert_eq!(audio.file_offset_bytes, 18_000 * RAW_SECTOR);
    assert_eq!(audio.start_lba, 18_000, "an in-file gap moves nothing");
    assert_eq!(
        audio.sector_count, 2_000,
        "the gap is part of what is written"
    );
    assert_eq!(audio.pregap_sectors, 150);
    assert_eq!(audio.indexes, vec![(0, 0), (1, 150)]);
}

#[test]
fn one_file_per_track_lays_out_end_to_end() {
    let parsed = sheet(
        r#"FILE "track01.bin" BINARY
  TRACK 01 AUDIO
    INDEX 01 00:00:00
FILE "track02.bin" BINARY
  TRACK 02 AUDIO
    INDEX 01 00:00:00
"#,
    );

    let layout = cue::layout(&parsed, &[RAW_SECTOR * 100, RAW_SECTOR * 50]).expect("a layout");

    assert_eq!(layout.tracks[0].file, 0);
    assert_eq!(layout.tracks[0].start_lba, 0);
    assert_eq!(layout.tracks[0].sector_count, 100);
    assert_eq!(layout.tracks[1].file, 1);
    assert_eq!(layout.tracks[1].file_offset_bytes, 0);
    assert_eq!(layout.tracks[1].start_lba, 100, "the disc keeps counting");
    assert_eq!(layout.tracks[1].sector_count, 50);
}

#[test]
fn a_truncated_dump_gives_a_short_last_track_and_says_so() {
    // Nothing in a sheet states a track length, so a BIN that stops early
    // reads as a short final track. Saying so is the whole value.
    let parsed = sheet(
        "FILE \"d.bin\" BINARY\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    INDEX 01 00:01:00\n",
    );

    let layout = cue::layout(&parsed, &[RAW_SECTOR * 100 + 500]).expect("a layout");

    assert_eq!(layout.tracks[0].sector_count, 75);
    assert_eq!(layout.tracks[1].sector_count, 25);
    assert!(
        layout
            .warnings
            .iter()
            .any(|warning| matches!(warning, CueWarning::FileNotSectorAligned { .. }))
    );
}

#[test]
fn a_sheet_that_cannot_be_laid_out_is_refused_rather_than_guessed() {
    let unknown_mode =
        sheet("FILE \"d.bin\" BINARY\n  TRACK 01 MODE9/9999\n    INDEX 01 00:00:00\n");
    assert!(matches!(
        cue::layout(&unknown_mode, &[RAW_SECTOR * 10]).unwrap_err(),
        LayoutError::UnknownSectorSize { track: 1, .. }
    ));

    let no_index = sheet("FILE \"d.bin\" BINARY\n  TRACK 01 AUDIO\n    INDEX 02 00:00:00\n");
    assert_eq!(
        cue::layout(&no_index, &[RAW_SECTOR * 10]).unwrap_err(),
        LayoutError::MissingIndexOne { track: 1 }
    );

    let wave = sheet("FILE \"t.wav\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n");
    assert!(matches!(
        cue::layout(&wave, &[RAW_SECTOR * 10]).unwrap_err(),
        LayoutError::NotRawSectors { .. }
    ));

    let impossible = sheet("FILE \"d.bin\" BINARY\n  TRACK 01 AUDIO\n    INDEX 01 00:00:99\n");
    assert_eq!(
        cue::layout(&impossible, &[RAW_SECTOR * 10]).unwrap_err(),
        LayoutError::ImpossibleTimecode { track: 1 }
    );
}

#[test]
fn tracks_that_do_not_advance_are_refused() {
    let backwards = sheet(
        "FILE \"d.bin\" BINARY\n  TRACK 01 AUDIO\n    INDEX 01 00:02:00\n  TRACK 02 AUDIO\n    INDEX 01 00:01:00\n",
    );

    assert_eq!(
        cue::layout(&backwards, &[RAW_SECTOR * 1000]).unwrap_err(),
        LayoutError::TracksOutOfOrder { track: 1 }
    );
}

#[test]
fn a_track_starting_past_the_end_of_its_file_is_refused() {
    // A sheet paired with the wrong BIN, which is how a burn would otherwise
    // write whatever happened to be at that offset.
    let parsed = sheet("FILE \"d.bin\" BINARY\n  TRACK 01 AUDIO\n    INDEX 01 01:00:00\n");

    assert!(matches!(
        cue::layout(&parsed, &[RAW_SECTOR * 100]).unwrap_err(),
        LayoutError::StartsOutsideFile {
            track: 1,
            start: 4500,
            sectors: 100,
            ..
        }
    ));
}

#[test]
fn a_size_for_every_file_is_required() {
    let parsed = sheet("FILE \"d.bin\" BINARY\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n");

    assert_eq!(
        cue::layout(&parsed, &[]).unwrap_err(),
        LayoutError::SizeCountMismatch {
            given: 0,
            declared: 1
        }
    );
}

#[test]
fn a_second_session_survives_into_the_layout() {
    let parsed = sheet(
        r#"FILE "d.bin" BINARY
  TRACK 01 MODE2/2352
    INDEX 01 00:00:00
REM SESSION 02
  TRACK 02 MODE2/2352
    INDEX 01 00:01:00
"#,
    );

    let layout = cue::layout(&parsed, &[RAW_SECTOR * 200]).expect("a layout");

    assert_eq!(layout.session_count, 2);
    assert_eq!(layout.tracks[1].session, 2);
}

// --- the codes a sheet carries -------------------------------------------------

#[test]
fn a_catalogue_number_and_an_isrc_reach_the_layout() {
    let parsed = sheet(
        r#"CATALOG 1234567890123
FILE "d.bin" BINARY
  TRACK 01 AUDIO
    ISRC USRC17607839
    INDEX 01 00:00:00
"#,
    );

    let layout = cue::layout(&parsed, &[RAW_SECTOR * 100]).expect("a layout");

    assert_eq!(layout.catalog.as_deref(), Some("1234567890123"));
    assert_eq!(layout.tracks[0].isrc.as_deref(), Some("USRC17607839"));
    assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
}

#[test]
fn a_code_that_is_not_one_is_warned_about_and_left_out_of_the_layout() {
    // The sheet is preserved as it was received; what is not preserved is a
    // claim that these are a catalogue number and an ISRC.
    let parsed = sheet(
        r#"CATALOG not-a-catalogue
FILE "d.bin" BINARY
  TRACK 01 AUDIO
    ISRC nonsense
    INDEX 01 00:00:00
"#,
    );

    assert!(
        parsed
            .warnings
            .iter()
            .any(|warning| matches!(warning, CueWarning::MalformedCatalog { .. }))
    );
    assert!(
        parsed
            .warnings
            .iter()
            .any(|warning| matches!(warning, CueWarning::MalformedIsrc { track: 1, .. }))
    );
    assert_eq!(parsed.catalog.as_deref(), Some("not-a-catalogue"));

    let layout = cue::layout(&parsed, &[RAW_SECTOR * 100]).expect("a layout");
    assert_eq!(layout.catalog, None);
    assert_eq!(layout.tracks[0].isrc, None);
}
