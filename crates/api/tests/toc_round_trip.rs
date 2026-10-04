// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The table of contents written to burn a disc reads back as the same disc.
//!
//! A CUE sheet is laid out, turned into the burn plan a worker would follow,
//! and written as the TOC cdrdao is given. Parsing that TOC with the import
//! side's parser has to give back every track where it was: start, length,
//! gaps, indexes, codes, flags and byte order. If it did not, a disc imported
//! from a TOC would burn differently from the same disc imported from a CUE
//! sheet, or one of the two sides would be reading cdrdao's format wrong.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::PathBuf;

use tangible_burn::cdrdao::write_toc;
use tangible_burn::plan::{PlannedIndex, PlannedTrack};
use tangible_burn::{BurnPlan, DriveRef, PlannedInput, WriteMode};
use tangible_domain::{BurnAttemptId, DriveId, Sha256Digest, WorkerId};
use tangible_image::{TrackLayout, cue, toc};

const RAW: u64 = 2352;

fn planned(track: &TrackLayout) -> PlannedTrack {
    PlannedTrack {
        number: track.number,
        session: track.session,
        mode: track.mode.clone(),
        input: track.file,
        file_offset_bytes: track.file_offset_bytes,
        start_lba: track.start_lba,
        sector_count: track.sector_count,
        pregap_sectors: track.pregap_sectors,
        indexes: track
            .indexes
            .iter()
            .map(|(number, relative_lba)| PlannedIndex {
                number: *number,
                relative_lba: *relative_lba,
            })
            .collect(),
        isrc: track.isrc.clone(),
        sample_byte_order: track.sample_byte_order,
        flags: track.flags.clone(),
    }
}

/// Lay out `sheet`, write the TOC a burn would use, read it back, and insist
/// it is the same disc.
fn round_trip(sheet: &str, sizes: &[u64]) {
    let parsed = cue::parse(sheet.as_bytes()).expect("a sheet");
    let from_cue = cue::layout(&parsed, sizes).expect("a layout");

    let inputs: Vec<PlannedInput> = sizes
        .iter()
        .enumerate()
        .map(|(n, size)| PlannedInput {
            staged_path: PathBuf::from(format!("/var/lib/tangible-worker/staging/track{n}.bin")),
            sha256: Sha256Digest::from_bytes([0; 32]),
            length_bytes: *size,
        })
        .collect();
    let plan = BurnPlan {
        attempt_id: BurnAttemptId::generate(),
        drive: DriveRef {
            worker_id: WorkerId::generate(),
            drive_id: DriveId::generate(),
            device_alias: "/dev/sr0".to_owned(),
        },
        total_bytes: sizes.iter().sum(),
        inputs,
        tracks: from_cue.tracks.iter().map(planned).collect(),
        catalog: from_cue.catalog.clone(),
        mode: WriteMode::TocDiscAtOnce,
        accepted_profiles: Vec::new(),
        speed: None,
        finalize: true,
        eject_on_success: false,
    };

    let written = write_toc(&plan).expect("the burn side writes it");
    let document = toc::parse(written.as_bytes())
        .unwrap_or_else(|error| panic!("the import side cannot read it: {error}\n{written}"));
    // The TOC names the staged inputs; their sizes, in the order it names
    // them.
    let toc_sizes: Vec<u64> = document
        .files
        .iter()
        .map(|name| {
            let n: usize = name
                .trim_start_matches("/var/lib/tangible-worker/staging/track")
                .trim_end_matches(".bin")
                .parse()
                .expect("a staged input");
            sizes[n]
        })
        .collect();
    let from_toc = toc::layout(&document, &toc_sizes)
        .unwrap_or_else(|error| panic!("{error}\n{written}"))
        .layout;

    assert_eq!(from_toc.catalog, from_cue.catalog, "{written}");
    assert_eq!(from_toc.tracks.len(), from_cue.tracks.len(), "{written}");
    for (back, original) in from_toc.tracks.iter().zip(&from_cue.tracks) {
        assert_eq!(back.number, original.number, "{written}");
        assert_eq!(back.mode, original.mode, "{written}");
        assert_eq!(
            back.start_lba, original.start_lba,
            "track {}: start\n{written}",
            original.number
        );
        assert_eq!(
            back.sector_count, original.sector_count,
            "track {}: length\n{written}",
            original.number
        );
        assert_eq!(
            back.pregap_sectors, original.pregap_sectors,
            "track {}: pregap\n{written}",
            original.number
        );
        assert_eq!(
            back.file_offset_bytes, original.file_offset_bytes,
            "track {}: offset\n{written}",
            original.number
        );
        assert_eq!(
            back.indexes, original.indexes,
            "track {}: indexes\n{written}",
            original.number
        );
        assert_eq!(
            back.isrc, original.isrc,
            "track {}: ISRC\n{written}",
            original.number
        );
        assert_eq!(
            back.flags, original.flags,
            "track {}: flags\n{written}",
            original.number
        );
        // Samples have a byte order and data sectors do not. A CUE sheet
        // records its file's for every track, a TOC only for audio, and the
        // burn side writes it only for audio, so only audio is compared.
        if original.mode == "AUDIO" {
            assert_eq!(
                back.sample_byte_order, original.sample_byte_order,
                "track {}: byte order\n{written}",
                original.number
            );
        }
    }
}

#[test]
fn a_mixed_mode_disc_with_a_generated_gap() {
    round_trip(
        "CATALOG 1234567890123\n\
         FILE \"disc.bin\" BINARY\n\
           TRACK 01 MODE1/2352\n\
             INDEX 01 00:00:00\n\
           TRACK 02 AUDIO\n\
             PREGAP 00:02:00\n\
             ISRC USRC17607839\n\
             FLAGS DCP PRE\n\
             INDEX 01 00:13:25\n",
        &[RAW * 4000],
    );
}

#[test]
fn a_gap_held_in_the_file_and_extra_index_points() {
    round_trip(
        "FILE \"disc.bin\" BINARY\n\
           TRACK 01 MODE2/2352\n\
             INDEX 01 00:00:00\n\
           TRACK 02 AUDIO\n\
             INDEX 00 00:13:25\n\
             INDEX 01 00:15:25\n\
             INDEX 02 00:20:00\n\
             INDEX 03 00:25:10\n\
           TRACK 03 AUDIO\n\
             FLAGS 4CH\n\
             INDEX 00 00:40:00\n\
             INDEX 01 00:41:00\n",
        &[RAW * 4500],
    );
}

#[test]
fn audio_from_a_file_per_track() {
    round_trip(
        "FILE \"one.bin\" BINARY\n\
           TRACK 01 AUDIO\n\
             INDEX 01 00:00:00\n\
         FILE \"two.bin\" BINARY\n\
           TRACK 02 AUDIO\n\
             INDEX 00 00:00:00\n\
             INDEX 01 00:02:00\n\
         FILE \"three.bin\" MOTOROLA\n\
           TRACK 03 AUDIO\n\
             PREGAP 00:01:00\n\
             INDEX 01 00:00:00\n",
        &[RAW * 3000, RAW * 1500, RAW * 800],
    );
}
