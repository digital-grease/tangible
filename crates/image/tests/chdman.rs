// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The chdman engine, running the real tool.
//!
//! Every case makes a CHD, has chdman verify it, extracts it again and lets
//! the engine compare the result with the parent, track by track. The discs
//! are generated: random track data (so a shifted or dropped sector cannot
//! hide among zeros), an audio track with a pregap stored in the file, and
//! the same disc as one file and as one file per track.
//!
//! These tests need chdman, so they are gated. Set `TANGIBLE_CHDMAN_TESTS=1`
//! to run them, and `TANGIBLE_CHDMAN_BIN` if the binary is not on `PATH`
//! (a wrapper that runs it in a container works, if the wrapper mounts the
//! temporary directory at the same path and keeps the working directory).
//! CI installs chdman and sets the flag.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::Path;

use tangible_domain::derivation::ChdOptions;
use tangible_domain::{LogicalPath, LossCharacter, Transformation};
use tangible_image::chdman::Chdman;
use tangible_image::derive::{DerivationEngine, DeriveError, DeriveRequest};
use tempfile::TempDir;

const RAW: usize = 2352;

async fn chdman() -> Option<Chdman> {
    if std::env::var("TANGIBLE_CHDMAN_TESTS").unwrap_or_default() != "1" {
        eprintln!("skipping: set TANGIBLE_CHDMAN_TESTS=1 to run the chdman engine tests");
        return None;
    }
    let program = std::env::var("TANGIBLE_CHDMAN_BIN").unwrap_or_else(|_| "chdman".to_owned());
    Some(
        Chdman::probe(program)
            .await
            .expect("chdman reports a version"),
    )
}

/// Deterministic noise.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            state.to_be_bytes()[2]
        })
        .collect()
}

/// A data track, an audio track whose two-second pregap is in the file, and
/// a second audio track.
struct Disc {
    data: Vec<u8>,
    gap: Vec<u8>,
    audio: Vec<u8>,
    more: Vec<u8>,
}

fn disc() -> Disc {
    Disc {
        data: noise(RAW * 300, 1),
        gap: vec![0; RAW * 150],
        audio: noise(RAW * 200, 2),
        more: noise(RAW * 175, 3),
    }
}

fn write(dir: &Path, name: &str, bytes: &[u8]) {
    std::fs::write(dir.join(name), bytes).unwrap();
}

async fn derive(
    engine: &Chdman,
    dir: &Path,
    input: &str,
    transformation: Transformation,
) -> Result<(LossCharacter, u64), DeriveError> {
    let output = dir.join("out").join("disc.chd");
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    let options = ChdOptions::defaults(transformation);
    let input = LogicalPath::parse(input).unwrap();
    let result = engine
        .derive(&DeriveRequest {
            transformation,
            options: Some(&options),
            input_dir: &dir.join("in"),
            input: &input,
            output: &output,
        })
        .await;
    match result {
        Ok(report) => Ok((
            report.loss_character,
            std::fs::metadata(&output).unwrap().len(),
        )),
        Err(error) => {
            assert!(!output.exists(), "a refused CHD must not be left behind");
            Err(error)
        }
    }
}

fn input_dir() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("in");
    std::fs::create_dir_all(&input).unwrap();
    (dir, input)
}

#[tokio::test]
async fn a_single_file_cue_bin_round_trips_track_for_track() {
    let Some(engine) = chdman().await else { return };
    let (dir, input) = input_dir();
    let d = disc();
    write(
        &input,
        "disc.bin",
        &[d.data, d.gap, d.audio, d.more].concat(),
    );
    write(
        &input,
        "disc.cue",
        b"FILE \"disc.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    INDEX 00 00:04:00\n    INDEX 01 00:06:00\n  TRACK 03 AUDIO\n    INDEX 01 00:08:50\n",
    );
    let (loss, size) = derive(&engine, dir.path(), "disc.cue", Transformation::ChdCreateCd)
        .await
        .expect("a CHD that round-trips");
    assert_eq!(loss, LossCharacter::StructurallyEquivalent);
    assert!(size > 0);
}

#[tokio::test]
async fn a_file_per_track_cue_bin_round_trips_too() {
    let Some(engine) = chdman().await else { return };
    let (dir, input) = input_dir();
    let d = disc();
    write(&input, "t1.bin", &d.data);
    write(&input, "t2.bin", &[d.gap, d.audio].concat());
    write(&input, "t3.bin", &d.more);
    write(
        &input,
        "disc.cue",
        b"FILE \"t1.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\nFILE \"t2.bin\" BINARY\n  TRACK 02 AUDIO\n    INDEX 00 00:00:00\n    INDEX 01 00:02:00\nFILE \"t3.bin\" BINARY\n  TRACK 03 AUDIO\n    INDEX 01 00:00:00\n",
    );
    let (loss, _) = derive(&engine, dir.path(), "disc.cue", Transformation::ChdCreateCd)
        .await
        .expect("a CHD that round-trips");
    assert_eq!(loss, LossCharacter::StructurallyEquivalent);
}

#[tokio::test]
async fn a_toc_without_an_in_file_pregap_round_trips() {
    let Some(engine) = chdman().await else { return };
    let (dir, input) = input_dir();
    let d = disc();
    write(&input, "disc.bin", &[d.data, d.audio].concat());
    write(
        &input,
        "disc.toc",
        b"CD_ROM\n\nTRACK MODE1_RAW\nDATAFILE \"disc.bin\" 00:04:00\n\nTRACK AUDIO\nDATAFILE \"disc.bin\" #705600 00:02:50\n",
    );
    let (loss, _) = derive(&engine, dir.path(), "disc.toc", Transformation::ChdCreateCd)
        .await
        .expect("a CHD that round-trips");
    assert_eq!(loss, LossCharacter::StructurallyEquivalent);
}

#[tokio::test]
async fn a_toc_whose_pregap_chdman_misreads_is_refused() {
    let Some(engine) = chdman().await else { return };
    let (dir, input) = input_dir();
    let d = disc();
    write(
        &input,
        "disc.bin",
        &[d.data, d.gap, d.audio, d.more].concat(),
    );
    write(
        &input,
        "disc.toc",
        b"CD_ROM\n\nTRACK MODE1_RAW\nDATAFILE \"disc.bin\" 00:04:00\n\nTRACK AUDIO\nDATAFILE \"disc.bin\" #705600 00:04:50\nSTART 00:02:00\n\nTRACK AUDIO\nDATAFILE \"disc.bin\" #1528800 00:02:25\n",
    );
    let result = derive(&engine, dir.path(), "disc.toc", Transformation::ChdCreateCd).await;
    // chdman 0.251 and 0.276, Debian 12's and 13's, store this track's data
    // shifted by its pregap; the round trip is what catches it.
    let misreads = matches!(engine.tool_version().as_str(), "0.251" | "0.276");
    match result {
        Err(error) => {
            assert_eq!(error.code(), "DERIVATION_NOT_EQUIVALENT", "{error}");
            assert!(error.to_string().contains("track 2"), "{error}");
        }
        Ok((loss, _)) => {
            assert!(
                !misreads,
                "chdman {} misreads START; the CHD must be refused",
                engine.tool_version()
            );
            // A chdman that reads it correctly must truly match.
            assert_eq!(loss, LossCharacter::StructurallyEquivalent);
        }
    }
}

#[tokio::test]
async fn a_cd_iso_comes_back_byte_for_byte() {
    let Some(engine) = chdman().await else { return };
    let (dir, input) = input_dir();
    write(&input, "disc.iso", &noise(2048 * 400, 4));
    let (loss, _) = derive(&engine, dir.path(), "disc.iso", Transformation::ChdCreateCd)
        .await
        .expect("an ISO round-trips");
    assert_eq!(loss, LossCharacter::BitExactRepack);
}

#[tokio::test]
async fn a_dvd_chd_is_refused_by_a_chdman_without_createdvd() {
    let Some(engine) = chdman().await else { return };
    if engine.tool_version() != "0.251" {
        return;
    }
    let refused = engine
        .check_supported(Transformation::ChdCreateDvd)
        .unwrap_err();
    assert!(refused.contains("createdvd"), "{refused}");
    let (dir, input) = input_dir();
    write(&input, "disc.iso", &noise(2048 * 10, 5));
    let error = derive(
        &engine,
        dir.path(),
        "disc.iso",
        Transformation::ChdCreateDvd,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code(), "DERIVATION_UNSUPPORTED");
}

#[tokio::test]
async fn a_dvd_iso_makes_a_dvd_chd_that_extracts_to_the_same_iso() {
    let Some(engine) = chdman().await else { return };
    if engine
        .check_supported(Transformation::ChdCreateDvd)
        .is_err()
    {
        // A chdman before createdvd; the test above covers it.
        return;
    }
    let (dir, input) = input_dir();
    let iso = noise(2048 * 400, 6);
    write(&input, "disc.iso", &iso);
    let (loss, _) = derive(
        &engine,
        dir.path(),
        "disc.iso",
        Transformation::ChdCreateDvd,
    )
    .await
    .expect("a DVD CHD that round-trips");
    assert_eq!(loss, LossCharacter::BitExactRepack);

    // The CHD createdvd made, imported as an original, extracts to the ISO
    // it was made of.
    let made = dir.path().join("out").join("disc.chd");
    std::fs::rename(&made, input.join("made.chd")).unwrap();
    let loss = extract(
        &engine,
        dir.path(),
        "made.chd",
        "back.iso",
        Transformation::IsoFromChd,
    )
    .await
    .expect("extracted and checked");
    assert_eq!(loss, LossCharacter::StructurallyEquivalent);
    assert_eq!(
        std::fs::read(dir.path().join("out").join("back.iso")).unwrap(),
        iso
    );
}

// --- CHDs imported as originals, extracted (docs/plans/chd-images.md) --------

fn chd_fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/../../fixtures/images/chd/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// Extract `input` in `dir/in` to `dir/out/<output>`, as a job would.
async fn extract(
    engine: &Chdman,
    dir: &Path,
    input: &str,
    output: &str,
    transformation: Transformation,
) -> Result<LossCharacter, DeriveError> {
    let output = dir.join("out").join(output);
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    let input = LogicalPath::parse(input).unwrap();
    let result = engine
        .derive(&DeriveRequest {
            transformation,
            options: None,
            input_dir: &dir.join("in"),
            input: &input,
            output: &output,
        })
        .await;
    if result.is_err() {
        assert!(
            !output.exists(),
            "a refused extraction must not be left behind"
        );
        assert!(!output.with_extension("bin").exists());
    }
    result.map(|report| report.loss_character)
}

#[tokio::test]
async fn a_cd_chd_extracts_to_a_cue_bin_that_makes_the_same_chd() {
    let Some(engine) = chdman().await else { return };
    let (dir, input) = input_dir();
    write(&input, "Game.chd", &chd_fixture("mixed-mode.chd"));
    let loss = extract(
        &engine,
        dir.path(),
        "Game.chd",
        "Game.cue",
        Transformation::CueBinFromChd,
    )
    .await
    .expect("extracted and checked");
    assert_eq!(loss, LossCharacter::StructurallyEquivalent);
    let out = dir.path().join("out");
    assert_eq!(
        std::fs::read_to_string(out.join("Game.cue")).unwrap(),
        "FILE \"Game.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    \
         INDEX 00 00:04:00\n    INDEX 01 00:06:00\n  TRACK 03 AUDIO\n    INDEX 01 00:08:00\n"
    );
    assert_eq!(
        std::fs::metadata(out.join("Game.bin")).unwrap().len(),
        750 * RAW as u64
    );
    assert!(!out.join("Game.check").exists(), "the check leaves nothing");
}

#[tokio::test]
async fn a_chd_of_noise_extracts_to_exactly_the_bin_it_was_made_from() {
    let Some(engine) = chdman().await else { return };
    let (dir, input) = input_dir();
    let d = disc();
    let bin = [d.data, d.gap, d.audio, d.more].concat();
    write(&input, "disc.bin", &bin);
    write(
        &input,
        "disc.cue",
        b"FILE \"disc.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    INDEX 00 00:04:00\n    INDEX 01 00:06:00\n  TRACK 03 AUDIO\n    INDEX 01 00:08:50\n",
    );
    derive(&engine, dir.path(), "disc.cue", Transformation::ChdCreateCd)
        .await
        .expect("a CHD");

    let (again, input) = input_dir();
    std::fs::copy(
        dir.path().join("out").join("disc.chd"),
        input.join("disc.chd"),
    )
    .unwrap();
    extract(
        &engine,
        again.path(),
        "disc.chd",
        "disc.cue",
        Transformation::CueBinFromChd,
    )
    .await
    .expect("extracted and checked");
    let back = std::fs::read(again.path().join("out").join("disc.bin")).unwrap();
    assert!(back == bin, "every byte of every track comes back");
}

#[tokio::test]
async fn a_dvd_chd_extracts_to_its_iso() {
    let Some(engine) = chdman().await else { return };
    let (dir, input) = input_dir();
    write(&input, "Film.chd", &chd_fixture("dvd.chd"));
    let loss = extract(
        &engine,
        dir.path(),
        "Film.chd",
        "Film.iso",
        Transformation::IsoFromChd,
    )
    .await
    .expect("extracted and checked");
    assert_eq!(loss, LossCharacter::StructurallyEquivalent);
    let iso = std::fs::read(dir.path().join("out").join("Film.iso")).unwrap();
    assert_eq!(iso.len(), 64 * 2048);
    // make_inputs.py numbers every sector in its first four bytes.
    assert_eq!(&iso[2048 * 5..2048 * 5 + 4], &5_u32.to_le_bytes());
}

#[tokio::test]
async fn a_delta_chd_without_its_parent_is_refused() {
    let Some(engine) = chdman().await else { return };
    let (dir, input) = input_dir();
    write(&input, "delta.chd", &chd_fixture("delta.chd"));
    let error = extract(
        &engine,
        dir.path(),
        "delta.chd",
        "delta.cue",
        Transformation::CueBinFromChd,
    )
    .await
    .expect_err("chdman cannot read it");
    assert_eq!(error.code(), "CHDMAN_FAILED", "{error}");
}

#[tokio::test]
async fn verify_passes_a_whole_chd_and_fails_a_damaged_one() {
    // What the RomM export asks before sending an imported CHD as it is
    // (OWNER_QUEUE 14): chdman checks it against its own SHA-1s.
    let Some(engine) = chdman().await else { return };
    let (_dir, input) = input_dir();
    let good = chd_fixture("mixed-mode.chd");
    write(&input, "good.chd", &good);
    engine
        .verify(&input.join("good.chd"))
        .await
        .expect("a whole CHD verifies");

    let mut damaged = good.clone();
    let middle = damaged.len() / 2;
    damaged[middle] ^= 0xff;
    write(&input, "damaged.chd", &damaged);
    let error = engine
        .verify(&input.join("damaged.chd"))
        .await
        .expect_err("a damaged CHD does not");
    assert_eq!(error.code(), "CHDMAN_FAILED", "{error}");
    assert!(!error.is_retryable(), "chdman's own verdict: {error}");
    // It only read them.
    assert_eq!(std::fs::read(input.join("good.chd")).unwrap(), good);
}
