// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Turning a burn plan into a cdrdao table of contents.
//!
//! Golden output, compared in full. A TOC is read by another program and a
//! near-miss produces a disc that is nearly right, so these assert the exact
//! document rather than that it contains the interesting parts.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::PathBuf;

use tangible_burn::cdrdao::{TocError, write_toc};
use tangible_burn::plan::{PlannedIndex, PlannedTrack};
use tangible_burn::{BurnPlan, DriveRef, PlannedInput, WriteMode};
use tangible_domain::cd::{SampleByteOrder, TrackFlag};
use tangible_domain::{BurnAttemptId, DriveId, Sha256Digest, WorkerId, cd};

const RAW: u64 = 2352;

fn plan(inputs: &[(&str, u64)], tracks: Vec<PlannedTrack>) -> BurnPlan {
    BurnPlan {
        attempt_id: BurnAttemptId::generate(),
        drive: DriveRef {
            worker_id: WorkerId::generate(),
            drive_id: DriveId::generate(),
            device_alias: "/dev/disc-block".to_owned(),
        },
        inputs: inputs
            .iter()
            .map(|(path, length)| PlannedInput {
                staged_path: PathBuf::from(path),
                sha256: Sha256Digest::from_bytes([0; 32]),
                length_bytes: *length,
            })
            .collect(),
        tracks,
        catalog: None,
        mode: WriteMode::TocDiscAtOnce,
        accepted_profiles: vec!["CD-R".to_owned()],
        speed: None,
        finalize: true,
        eject_on_success: true,
        total_bytes: inputs.iter().map(|(_, length)| *length).sum(),
    }
}

fn track(number: u32, mode: &str, sectors: u64) -> PlannedTrack {
    PlannedTrack {
        number,
        session: 1,
        mode: mode.to_owned(),
        input: 0,
        file_offset_bytes: 0,
        start_lba: 0,
        sector_count: sectors,
        pregap_sectors: 0,
        indexes: vec![PlannedIndex {
            number: 1,
            relative_lba: 0,
        }],
        isrc: None,
        // What a sheet's BINARY file is, and what nearly every rip is.
        sample_byte_order: Some(SampleByteOrder::LittleEndian),
        flags: Vec::new(),
    }
}

// --- the shapes a disc actually comes in ---------------------------------------

#[test]
fn an_audio_disc_is_cd_da() {
    let written = write_toc(&plan(
        &[("/staged/disc.bin", RAW * 100)],
        vec![track(1, "AUDIO", 75), {
            let mut second = track(2, "AUDIO", 25);
            second.file_offset_bytes = RAW * 75;
            second
        }],
    ))
    .expect("a table of contents");

    assert_eq!(
        written,
        "CD_DA\n\
         \n\
         TRACK AUDIO\n\
         FILE \"/staged/disc.bin\" SWAP 00:00:00 00:01:00\n\
         \n\
         TRACK AUDIO\n\
         FILE \"/staged/disc.bin\" SWAP 00:01:00 00:00:25\n"
    );
}

#[test]
fn a_mixed_mode_disc_puts_the_data_track_first_and_generates_the_gap() {
    // The shape almost every game CD arrives in, and the one the whole of E7
    // exists for.
    let written = write_toc(&plan(
        &[("/staged/disc.bin", RAW * 100)],
        vec![track(1, "MODE2/2352", 75), {
            let mut audio = track(2, "AUDIO", 25);
            audio.file_offset_bytes = RAW * 75;
            audio.pregap_sectors = 2;
            audio
        }],
    ))
    .expect("a table of contents");

    assert_eq!(
        written,
        "CD_ROM_XA\n\
         \n\
         TRACK MODE2_RAW\n\
         DATAFILE \"/staged/disc.bin\" 00:01:00\n\
         \n\
         TRACK AUDIO\n\
         PREGAP 00:00:02\n\
         FILE \"/staged/disc.bin\" SWAP 00:01:00 00:00:25\n"
    );
}

#[test]
fn a_gap_the_file_carries_is_written_as_start_between_two_files() {
    // The distinction that decides whether the writer generates silence or
    // writes what is in the file. cdrdao says which by where START falls.
    let mut audio = track(2, "AUDIO", 25);
    audio.file_offset_bytes = RAW * 75;
    audio.pregap_sectors = 2;
    audio.indexes = vec![
        PlannedIndex {
            number: 0,
            relative_lba: 0,
        },
        PlannedIndex {
            number: 1,
            relative_lba: 2,
        },
    ];

    let written = write_toc(&plan(
        &[("/staged/disc.bin", RAW * 100)],
        vec![track(1, "MODE1/2352", 75), audio],
    ))
    .expect("a table of contents");

    assert_eq!(
        written,
        "CD_ROM\n\
         \n\
         TRACK MODE1_RAW\n\
         DATAFILE \"/staged/disc.bin\" 00:01:00\n\
         \n\
         TRACK AUDIO\n\
         FILE \"/staged/disc.bin\" SWAP 00:01:00 00:00:02\n\
         START\n\
         FILE \"/staged/disc.bin\" SWAP 00:01:02 00:00:23\n",
        "the gap is in the file, so no PREGAP is generated"
    );
}

#[test]
fn one_file_per_track_addresses_each_from_its_own_beginning() {
    let mut second = track(2, "AUDIO", 50);
    second.input = 1;

    let written = write_toc(&plan(
        &[
            ("/staged/track01.bin", RAW * 100),
            ("/staged/track02.bin", RAW * 50),
        ],
        vec![track(1, "AUDIO", 100), second],
    ))
    .expect("a table of contents");

    assert_eq!(
        written,
        "CD_DA\n\
         \n\
         TRACK AUDIO\n\
         FILE \"/staged/track01.bin\" SWAP 00:00:00 00:01:25\n\
         \n\
         TRACK AUDIO\n\
         FILE \"/staged/track02.bin\" SWAP 00:00:00 00:00:50\n"
    );
}

#[test]
fn index_points_past_the_first_are_measured_from_the_track_proper() {
    let mut audio = track(1, "AUDIO", 300);
    audio.indexes = vec![
        PlannedIndex {
            number: 1,
            relative_lba: 0,
        },
        PlannedIndex {
            number: 2,
            relative_lba: 150,
        },
    ];

    let written = write_toc(&plan(&[("/staged/a.bin", RAW * 300)], vec![audio]))
        .expect("a table of contents");

    assert!(
        written.ends_with("INDEX 00:02:00\n"),
        "an index mark 150 sectors in is two seconds in: {written}"
    );
}

// --- what it refuses -----------------------------------------------------------

#[test]
fn a_data_track_that_does_not_start_its_file_is_refused() {
    // DATAFILE names a length and no offset, so there is no way to say "start
    // here". Emitting it anyway would write the wrong bytes.
    let mut data = track(2, "MODE2/2352", 25);
    data.file_offset_bytes = RAW * 75;

    let refused = write_toc(&plan(
        &[("/staged/disc.bin", RAW * 100)],
        vec![track(1, "AUDIO", 75), data],
    ))
    .unwrap_err();

    assert!(matches!(
        refused,
        TocError::DataTrackNotAtStartOfFile { track: 2, .. }
    ));
}

#[test]
fn a_data_track_whose_file_carries_its_pregap_is_refused() {
    let mut data = track(1, "MODE2/2352", 75);
    data.pregap_sectors = 2;
    data.indexes = vec![
        PlannedIndex {
            number: 0,
            relative_lba: 0,
        },
        PlannedIndex {
            number: 1,
            relative_lba: 2,
        },
    ];

    let refused = write_toc(&plan(&[("/staged/disc.bin", RAW * 100)], vec![data])).unwrap_err();

    assert_eq!(refused, TocError::DataTrackHasInFilePregap { track: 1 });
}

#[test]
fn a_generated_gap_on_the_first_track_is_refused() {
    // cdrdao supplies track one's mandatory pregap itself. Whether a second
    // one would be ignored or added is not something to learn from a disc.
    let mut first = track(1, "AUDIO", 75);
    first.pregap_sectors = 150;

    let refused = write_toc(&plan(&[("/staged/disc.bin", RAW * 100)], vec![first])).unwrap_err();

    assert_eq!(
        refused,
        TocError::PregapOnFirstTrack {
            track: 1,
            sectors: 150
        }
    );
}

#[test]
fn a_mode_that_needs_the_subchannel_is_refused() {
    let refused = write_toc(&plan(
        &[("/staged/a.bin", 2448 * 10)],
        vec![track(1, "CDG", 10)],
    ))
    .unwrap_err();

    assert!(matches!(
        refused,
        TocError::ModeNotWritable { track: 1, .. }
    ));
}

#[test]
fn a_mode_nothing_recognises_is_refused() {
    let refused = write_toc(&plan(
        &[("/staged/a.bin", RAW * 10)],
        vec![track(1, "MODE9/9999", 10)],
    ))
    .unwrap_err();

    assert!(matches!(
        refused,
        TocError::UnknownTrackMode { track: 1, .. }
    ));
}

#[test]
fn a_path_that_could_close_the_quoted_string_is_refused() {
    // The staged name comes from a descriptor somebody else wrote. A quote in
    // it could end the string and continue with statements of its own.
    for hostile in [
        "/staged/evil\".bin",
        "/staged/back\\slash.bin",
        "/staged/new\nline.bin",
    ] {
        let refused = write_toc(&plan(&[(hostile, RAW * 10)], vec![track(1, "AUDIO", 10)]))
            .expect_err("must refuse");
        assert!(
            matches!(refused, TocError::UnquotablePath { .. }),
            "{hostile} was not refused: {refused:?}"
        );
    }
}

#[test]
fn an_offset_that_is_not_a_whole_number_of_sectors_is_refused() {
    let mut audio = track(1, "AUDIO", 10);
    audio.file_offset_bytes = 1;

    let refused = write_toc(&plan(&[("/staged/a.bin", RAW * 100)], vec![audio])).unwrap_err();

    assert!(matches!(
        refused,
        TocError::MisalignedOffset { track: 1, .. }
    ));
}

#[test]
fn a_plan_with_no_tracks_has_no_table_of_contents() {
    let mut block_image = plan(&[("/staged/disc.iso", 4096)], vec![]);
    block_image.mode = WriteMode::DataDiscAtOnce;

    assert_eq!(
        write_toc(&block_image).unwrap_err(),
        TocError::NotATrackLayout
    );
}

// --- the mapping that would be most expensive to get wrong ---------------------

#[test]
fn every_mode_maps_to_a_cdrdao_mode_holding_the_same_sized_sector() {
    // A mode mapped to one of a different block size scales every offset on
    // the disc. The sizes on the right are cdrdao's, from its manual; the
    // sizes on the left are this project's, from the descriptor. They have to
    // agree for each pairing or the disc comes out wrong.
    let cdrdao_block_bytes = |mode: &str| match mode {
        "AUDIO" | "MODE1_RAW" | "MODE2_RAW" => 2352,
        "MODE1" | "MODE2_FORM1" => 2048,
        "MODE2" | "MODE2_FORM_MIX" => 2336,
        "MODE2_FORM2" => 2324,
        other => panic!("{other} is not a cdrdao track mode"),
    };

    for mode in [
        "AUDIO",
        "MODE1/2048",
        "MODE1/2352",
        "MODE2/2048",
        "MODE2/2324",
        "MODE2/2336",
        "MODE2/2352",
        "CDI/2336",
        "CDI/2352",
    ] {
        let sector_bytes = u64::from(cd::sector_bytes(mode).expect("a known mode"));
        let written = write_toc(&plan(
            &[("/staged/a.bin", sector_bytes * 10)],
            vec![track(1, mode, 10)],
        ))
        .unwrap_or_else(|error| panic!("{mode} was refused: {error}"));

        let declared = written
            .lines()
            .find_map(|line| line.strip_prefix("TRACK "))
            .expect("a track line");
        assert_eq!(
            cdrdao_block_bytes(declared),
            sector_bytes,
            "{mode} was written as {declared}, whose sectors are a different size"
        );
    }
}

// --- the codes the disc carries ------------------------------------------------

#[test]
fn a_catalogue_number_and_an_isrc_are_written_out() {
    // The identifiers the ripped disc had, on the disc that replaces it.
    let mut audio = track(1, "AUDIO", 75);
    audio.isrc = Some("USRC17607839".to_owned());
    let mut with_codes = plan(&[("/staged/disc.bin", RAW * 100)], vec![audio]);
    with_codes.catalog = Some("1234567890123".to_owned());

    let written = write_toc(&with_codes).expect("a table of contents");

    assert_eq!(
        written,
        "CD_DA\n\
         CATALOG \"1234567890123\"\n\
         \n\
         TRACK AUDIO\n\
         ISRC \"USRC17607839\"\n\
         FILE \"/staged/disc.bin\" SWAP 00:00:00 00:01:00\n"
    );
}

#[test]
fn an_isrc_written_in_lower_case_is_the_same_code() {
    let mut audio = track(1, "AUDIO", 75);
    audio.isrc = Some("usrc17607839".to_owned());

    let written = write_toc(&plan(&[("/staged/a.bin", RAW * 100)], vec![audio]))
        .expect("a table of contents");

    assert!(written.contains("ISRC \"USRC17607839\""), "{written}");
}

#[test]
fn a_recording_code_against_a_data_track_is_not_written() {
    // cdrdao takes ISRC as an audio property. The manifest keeps it either
    // way; what is not done is asserting it in a place the format does not
    // have.
    let mut data = track(1, "MODE1/2352", 75);
    data.isrc = Some("USRC17607839".to_owned());

    let written =
        write_toc(&plan(&[("/staged/a.bin", RAW * 100)], vec![data])).expect("a table of contents");

    assert!(!written.contains("ISRC"), "{written}");
}

// --- byte order ------------------------------------------------------------------

#[test]
fn a_big_endian_file_is_what_cdrdao_expects_and_is_not_swapped() {
    let mut audio = track(1, "AUDIO", 75);
    audio.sample_byte_order = Some(SampleByteOrder::BigEndian);

    let written = write_toc(&plan(&[("/staged/disc.bin", RAW * 100)], vec![audio]))
        .expect("a table of contents");

    assert_eq!(
        written,
        "CD_DA\n\
         \n\
         TRACK AUDIO\n\
         FILE \"/staged/disc.bin\" 00:00:00 00:01:00\n"
    );
}

#[test]
fn each_file_says_its_own_byte_order() {
    // Why the swap is per file rather than cdrdao's global --swap: that option
    // would be wrong for one of these two whichever way it was set.
    let mut first = track(1, "AUDIO", 75);
    first.sample_byte_order = Some(SampleByteOrder::LittleEndian);
    let mut second = track(2, "AUDIO", 50);
    second.input = 1;
    second.sample_byte_order = Some(SampleByteOrder::BigEndian);

    let written = write_toc(&plan(
        &[("/staged/a.bin", RAW * 75), ("/staged/b.bin", RAW * 50)],
        vec![first, second],
    ))
    .expect("a table of contents");

    assert!(
        written.contains("FILE \"/staged/a.bin\" SWAP 00:00:00"),
        "{written}"
    );
    assert!(
        written.contains("FILE \"/staged/b.bin\" 00:00:00"),
        "{written}"
    );
}

#[test]
fn an_audio_track_whose_byte_order_is_unknown_is_refused() {
    // One answer is a disc and the other is static.
    let mut audio = track(1, "AUDIO", 75);
    audio.sample_byte_order = None;

    assert_eq!(
        write_toc(&plan(&[("/staged/disc.bin", RAW * 100)], vec![audio])),
        Err(TocError::ByteOrderUnknown { track: 1 })
    );
}

#[test]
fn a_data_track_is_never_swapped() {
    // Data sectors are bytes, not samples, whatever the file's audio would be.
    let mut data = track(1, "MODE1/2352", 75);
    data.sample_byte_order = None;

    let written = write_toc(&plan(&[("/staged/disc.bin", RAW * 100)], vec![data]))
        .expect("a table of contents");

    assert!(!written.contains("SWAP"), "{written}");
}

// --- flags -----------------------------------------------------------------------

#[test]
fn the_flags_an_audio_track_carries_are_written_before_its_data() {
    let mut audio = track(1, "AUDIO", 75);
    audio.isrc = Some("USRC17607839".to_owned());
    audio.flags = vec![
        TrackFlag::PreEmphasis,
        TrackFlag::DigitalCopyPermitted,
        TrackFlag::FourChannel,
    ];

    let written = write_toc(&plan(&[("/staged/disc.bin", RAW * 100)], vec![audio]))
        .expect("a table of contents");

    assert_eq!(
        written,
        "CD_DA\n\
         \n\
         TRACK AUDIO\n\
         ISRC \"USRC17607839\"\n\
         COPY\n\
         PRE_EMPHASIS\n\
         FOUR_CHANNEL_AUDIO\n\
         FILE \"/staged/disc.bin\" SWAP 00:00:00 00:01:00\n"
    );
}

#[test]
fn a_flag_goes_before_a_generated_gap() {
    // cdrdao reads the flag block and then the gap; the other order is a
    // syntax error rather than a different disc.
    let mut audio = track(2, "AUDIO", 25);
    audio.file_offset_bytes = RAW * 75;
    audio.pregap_sectors = 2;
    audio.flags = vec![TrackFlag::PreEmphasis];

    let written = write_toc(&plan(
        &[("/staged/disc.bin", RAW * 100)],
        vec![track(1, "MODE1/2352", 75), audio],
    ))
    .expect("a table of contents");

    assert!(
        written.contains("TRACK AUDIO\nPRE_EMPHASIS\nPREGAP 00:00:02\n"),
        "{written}"
    );
}

#[test]
fn audio_flags_are_not_asserted_of_a_data_track() {
    // Like an ISRC: the manifest keeps them, and the disc is not told
    // something about audio it does not hold. Copy permission is a property of
    // any track and is written.
    let mut data = track(1, "MODE1/2352", 75);
    data.flags = vec![
        TrackFlag::PreEmphasis,
        TrackFlag::FourChannel,
        TrackFlag::DigitalCopyPermitted,
    ];

    let written = write_toc(&plan(&[("/staged/disc.bin", RAW * 100)], vec![data]))
        .expect("a table of contents");

    assert!(written.contains("TRACK MODE1_RAW\nCOPY\n"), "{written}");
    assert!(!written.contains("PRE_EMPHASIS"), "{written}");
    assert!(!written.contains("FOUR_CHANNEL"), "{written}");
}

#[test]
fn serial_copy_management_has_no_statement_and_is_refused() {
    let mut audio = track(1, "AUDIO", 75);
    audio.flags = vec![TrackFlag::SerialCopyManagement];

    assert_eq!(
        write_toc(&plan(&[("/staged/disc.bin", RAW * 100)], vec![audio])),
        Err(TocError::FlagNotWritable {
            track: 1,
            flag: "SCMS"
        })
    );
}
