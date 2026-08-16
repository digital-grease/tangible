// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! ISO 9660 detection and structural inspection.
//!
//! Images are built byte by byte here rather than loaded from fixtures. That
//! keeps the malformed cases honest (a truncated or corrupt image is
//! constructed exactly, not approximated), and it means the suite carries no
//! binary blobs whose provenance would need documenting.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use tangible_image::iso::{
    IsoWarning, LOGICAL_SECTOR_BYTES, PrimaryVolume, SYSTEM_AREA_BYTES, inspect,
};

/// Build a volume descriptor sector.
fn descriptor(kind: u8, identifier: [u8; 5], version: u8) -> Vec<u8> {
    let mut sector = vec![0_u8; LOGICAL_SECTOR_BYTES];
    sector[0] = kind;
    sector[1..6].copy_from_slice(&identifier);
    sector[6] = version;
    sector
}

/// Write a both-endian 32-bit field.
fn put_be32(sector: &mut [u8], offset: usize, value: u32) {
    sector[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    sector[offset + 4..offset + 8].copy_from_slice(&value.to_be_bytes());
}

/// Write a both-endian 16-bit field.
fn put_be16(sector: &mut [u8], offset: usize, value: u16) {
    sector[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    sector[offset + 2..offset + 4].copy_from_slice(&value.to_be_bytes());
}

fn put_text(sector: &mut [u8], range: std::ops::Range<usize>, text: &str) {
    let bytes = text.as_bytes();
    for slot in &mut sector[range.clone()] {
        *slot = b' ';
    }
    let take = bytes.len().min(range.len());
    sector[range.start..range.start + take].copy_from_slice(&bytes[..take]);
}

/// A primary volume descriptor declaring `block_count` blocks of 2048.
fn primary_volume(volume_id: &str, block_count: u32) -> Vec<u8> {
    let mut sector = descriptor(1, *b"CD001", 1);
    put_text(&mut sector, 8..40, "LINUX");
    put_text(&mut sector, 40..72, volume_id);
    put_be32(&mut sector, 80, block_count);
    put_be16(&mut sector, 128, 2048);
    put_text(&mut sector, 318..446, "EXAMPLE PUBLISHER");
    put_text(&mut sector, 446..574, "EXAMPLE PREPARER");
    put_text(&mut sector, 574..702, "XORRISO");
    put_text(&mut sector, 813..830, "2026081600000000");
    sector
}

/// Assemble an image prefix from a list of descriptor sectors.
fn image(descriptors: Vec<Vec<u8>>) -> Vec<u8> {
    let mut bytes = vec![0_u8; SYSTEM_AREA_BYTES];
    for sector in descriptors {
        bytes.extend_from_slice(&sector);
    }
    bytes
}

/// A well-formed single-volume image and its true byte length.
fn well_formed(block_count: u32) -> (Vec<u8>, u64) {
    let prefix = image(vec![
        primary_volume("EXAMPLE_DISC", block_count),
        descriptor(255, *b"CD001", 1),
    ]);
    let total = u64::from(block_count) * 2048;
    (prefix, total)
}

// --- the happy path ----------------------------------------------------------

#[test]
fn a_well_formed_image_is_recognised() {
    let (prefix, total) = well_formed(100);
    let result = inspect(&prefix, total, Some("iso"));

    assert!(result.is_iso9660);
    assert!(
        result.confidence > 0.9,
        "confidence was {}",
        result.confidence
    );
    assert!(
        result.warnings.is_empty(),
        "unexpected: {:?}",
        result.warnings
    );
}

#[test]
fn primary_volume_fields_are_read() {
    let (prefix, total) = well_formed(100);
    let volume = inspect(&prefix, total, None)
        .primary
        .expect("primary volume");

    assert_eq!(
        volume,
        PrimaryVolume {
            volume_id: "EXAMPLE_DISC".to_owned(),
            system_id: "LINUX".to_owned(),
            publisher: "EXAMPLE PUBLISHER".to_owned(),
            data_preparer: "EXAMPLE PREPARER".to_owned(),
            application: "XORRISO".to_owned(),
            created: "2026081600000000".to_owned(),
            block_count: 100,
            block_size: 2048,
        }
    );
}

#[test]
fn iso9660_is_reported_as_a_filesystem() {
    let (prefix, total) = well_formed(10);
    let result = inspect(&prefix, total, None);
    assert!(result.filesystems.iter().any(|f| f.name == "iso9660"));
}

#[test]
fn evidence_explains_the_verdict() {
    // A parser that returns only a guess cannot be argued with.
    let (prefix, total) = well_formed(10);
    let result = inspect(&prefix, total, None);
    assert!(
        result
            .evidence
            .iter()
            .any(|e| e.contains("primary volume descriptor")),
        "evidence was {:?}",
        result.evidence
    );
}

// --- structure beats the filename --------------------------------------------

#[test]
fn a_misnamed_image_is_still_detected_by_structure() {
    // Named .bin but structurally an ISO. Structure wins.
    let (prefix, total) = well_formed(10);
    let result = inspect(&prefix, total, Some("bin"));

    assert!(result.is_iso9660);
    assert!(result.confidence > 0.9);
    assert!(
        result
            .warnings
            .iter()
            .any(|w| matches!(w, IsoWarning::ExtensionDisagreesWithStructure { .. })),
        "the disagreement should be recorded"
    );
}

#[test]
fn an_iso_extension_cannot_rescue_a_file_that_is_not_one() {
    // The extension only nudges confidence; it cannot manufacture structure.
    let junk = vec![0x41_u8; SYSTEM_AREA_BYTES + LOGICAL_SECTOR_BYTES];
    let result = inspect(&junk, junk.len() as u64, Some("iso"));

    assert!(!result.is_iso9660);
    assert!(result.confidence.abs() < f32::EPSILON);
}

#[test]
fn the_extension_only_slightly_adjusts_confidence() {
    let (prefix, total) = well_formed(10);
    let with = inspect(&prefix, total, Some("iso")).confidence;
    let without = inspect(&prefix, total, None).confidence;
    assert!(
        (with - without) < 0.05,
        "extension moved confidence by {}",
        with - without
    );
}

// --- the checks that catch a bad dump ----------------------------------------

#[test]
fn a_truncated_image_is_detected() {
    // The strongest signal available: the volume states its own extent, so a
    // short file is provably incomplete.
    let (prefix, full) = well_formed(1000);
    let truncated = full / 2;

    let result = inspect(&prefix, truncated, Some("iso"));
    assert!(
        result.is_iso9660,
        "it is still an ISO, just an incomplete one"
    );
    assert!(
        result.warnings.contains(&IsoWarning::Truncated {
            declared_bytes: full,
            actual_bytes: truncated,
        }),
        "warnings were {:?}",
        result.warnings
    );
    assert!(result.has_integrity_warning());
}

#[test]
fn truncation_lowers_confidence_but_does_not_deny_the_format() {
    let (prefix, full) = well_formed(1000);
    let intact = inspect(&prefix, full, Some("iso")).confidence;
    let broken = inspect(&prefix, full / 2, Some("iso")).confidence;
    assert!(broken < intact, "{broken} should be below {intact}");
    assert!(broken > 0.0, "a truncated ISO is still recognisably an ISO");
}

#[test]
fn a_both_endian_disagreement_is_reported() {
    // Cannot occur in a correctly written image, so it is strong evidence of
    // corruption or a broken generator.
    let mut sector = primary_volume("CORRUPT", 100);
    // Leave the little-endian half alone and change only the big-endian half.
    sector[84..88].copy_from_slice(&999_u32.to_be_bytes());
    let prefix = image(vec![sector, descriptor(255, *b"CD001", 1)]);

    let result = inspect(&prefix, 100 * 2048, None);
    assert!(
        result.warnings.iter().any(|w| matches!(
            w,
            IsoWarning::EndianMismatch {
                field: "volume space size",
                little_endian: 100,
                big_endian: 999,
            }
        )),
        "warnings were {:?}",
        result.warnings
    );
    assert!(result.has_integrity_warning());
}

#[test]
fn trailing_data_is_noted_but_is_not_an_integrity_failure() {
    // Padding is common and usually benign, but concatenated images look the
    // same from here, so it is worth surfacing.
    let (prefix, declared) = well_formed(10);
    let result = inspect(&prefix, declared + 2048, None);

    assert!(
        result
            .warnings
            .iter()
            .any(|w| matches!(w, IsoWarning::TrailingData { .. }))
    );
    assert!(!result.has_integrity_warning());
}

#[test]
fn a_partial_trailing_block_is_reported() {
    let (prefix, declared) = well_formed(10);
    let result = inspect(&prefix, declared + 7, None);
    assert!(
        result
            .warnings
            .contains(&IsoWarning::PartialTrailingBlock { remainder: 7 })
    );
}

#[test]
fn an_unusual_block_size_is_reported() {
    let mut sector = primary_volume("ODD", 10);
    put_be16(&mut sector, 128, 512);
    let prefix = image(vec![sector, descriptor(255, *b"CD001", 1)]);

    let result = inspect(&prefix, 10 * 512, None);
    assert!(
        result
            .warnings
            .contains(&IsoWarning::UnusualBlockSize { block_size: 512 })
    );
}

#[test]
fn a_missing_terminator_is_reported() {
    let prefix = image(vec![primary_volume("NOTERM", 10)]);
    let result = inspect(&prefix, 10 * 2048, None);
    assert!(result.warnings.contains(&IsoWarning::NoTerminator));
}

#[test]
fn an_unexpected_descriptor_version_is_reported() {
    let prefix = image(vec![
        descriptor(1, *b"CD001", 9),
        descriptor(255, *b"CD001", 1),
    ]);
    let result = inspect(&prefix, 4096, None);
    assert!(result.warnings.iter().any(|w| matches!(
        w,
        IsoWarning::UnexpectedDescriptorVersion {
            descriptor_type: 1,
            version: 9
        }
    )));
}

// --- UDF and Joliet ----------------------------------------------------------

#[test]
fn udf_is_recognised_alongside_iso9660() {
    // A hybrid disc, which is what most DVD and Blu-ray media actually are.
    let prefix = image(vec![
        descriptor(1, *b"BEA01", 1),
        descriptor(0, *b"NSR03", 1),
        descriptor(0, *b"TEA01", 1),
        primary_volume("HYBRID", 10),
        descriptor(255, *b"CD001", 1),
    ]);
    let result = inspect(&prefix, 10 * 2048, None);

    assert!(result.is_iso9660);
    assert!(result.filesystems.iter().any(|f| f.name == "udf"));
    assert!(result.filesystems.iter().any(|f| f.name == "iso9660"));
}

#[test]
fn a_udf_only_image_is_recognised_without_iso9660() {
    let prefix = image(vec![
        descriptor(1, *b"BEA01", 1),
        descriptor(0, *b"NSR02", 1),
        descriptor(0, *b"TEA01", 1),
    ]);
    let result = inspect(&prefix, 10 * 2048, None);

    assert!(!result.is_iso9660);
    assert!(result.filesystems.iter().any(|f| f.name == "udf"));
    assert!(result.confidence > 0.0, "UDF alone is still a real image");
}

#[test]
fn joliet_is_detected_from_its_escape_sequence() {
    let mut supplementary = descriptor(2, *b"CD001", 1);
    supplementary[88..91].copy_from_slice(&[0x25, 0x2f, 0x45]);
    let prefix = image(vec![
        primary_volume("JOLIET", 10),
        supplementary,
        descriptor(255, *b"CD001", 1),
    ]);

    let result = inspect(&prefix, 10 * 2048, None);
    assert!(result.filesystems.iter().any(|f| f.name == "joliet"));
}

// --- malformed input ---------------------------------------------------------

#[test]
fn an_empty_file_is_handled() {
    let result = inspect(&[], 0, None);
    assert!(!result.is_iso9660);
    assert!(result.confidence.abs() < f32::EPSILON);
}

#[test]
fn a_file_shorter_than_the_system_area_is_handled() {
    for length in [1_usize, 100, SYSTEM_AREA_BYTES - 1, SYSTEM_AREA_BYTES] {
        let bytes = vec![0_u8; length];
        let result = inspect(&bytes, length as u64, Some("iso"));
        assert!(!result.is_iso9660, "length {length} must not parse");
    }
}

#[test]
fn a_descriptor_cut_off_mid_sector_does_not_panic() {
    // A read that stopped partway through a descriptor must be tolerated
    // rather than indexing past the end.
    let (full, total) = well_formed(10);
    for cut in 1..LOGICAL_SECTOR_BYTES {
        let truncated = &full[..SYSTEM_AREA_BYTES + cut];
        let _ = inspect(truncated, total, None);
    }
}

#[test]
fn arbitrary_bytes_never_panic() {
    // Standing in for a fuzz corpus: patterns that have historically broken
    // hand-written parsers.
    let cases: Vec<Vec<u8>> = vec![
        vec![0x00; SYSTEM_AREA_BYTES + LOGICAL_SECTOR_BYTES],
        vec![0xff; SYSTEM_AREA_BYTES + LOGICAL_SECTOR_BYTES],
        {
            let mut v = vec![0x00; SYSTEM_AREA_BYTES];
            v.extend_from_slice(&[0xff; LOGICAL_SECTOR_BYTES]);
            v
        },
        {
            // Claims to be a descriptor but is otherwise garbage.
            let mut v = vec![0xab; SYSTEM_AREA_BYTES];
            let mut sector = vec![0xcd_u8; LOGICAL_SECTOR_BYTES];
            sector[1..6].copy_from_slice(b"CD001");
            v.extend_from_slice(&sector);
            v
        },
    ];

    for (index, bytes) in cases.iter().enumerate() {
        for declared in [0_u64, 1, u64::MAX] {
            let _ = inspect(bytes, declared, Some("iso"));
            let _ = inspect(bytes, declared, None);
        }
        let _ = index;
    }
}

#[test]
fn an_enormous_declared_block_count_does_not_overflow() {
    // The declared extent is attacker-controlled, so the size arithmetic must
    // saturate rather than wrap.
    let mut sector = primary_volume("HUGE", u32::MAX);
    put_be16(&mut sector, 128, u16::MAX);
    let prefix = image(vec![sector, descriptor(255, *b"CD001", 1)]);

    let result = inspect(&prefix, 1024, None);
    assert!(
        result
            .warnings
            .iter()
            .any(|w| matches!(w, IsoWarning::Truncated { .. }))
    );
}

#[test]
fn a_descriptor_sequence_with_no_terminator_is_bounded() {
    // Without the bound this would walk the whole file.
    let mut descriptors = Vec::new();
    for _ in 0..500 {
        descriptors.push(descriptor(0, *b"CD001", 1));
    }
    let prefix = image(descriptors);

    let result = inspect(&prefix, prefix.len() as u64, None);
    assert!(
        result.descriptor_types.len() <= 64,
        "walked {} descriptors",
        result.descriptor_types.len()
    );
    assert!(result.warnings.contains(&IsoWarning::NoTerminator));
}

#[test]
fn control_characters_are_stripped_from_text_fields() {
    // Volume identifiers are attacker-influenced and reach logs and the UI.
    let mut sector = primary_volume("X", 10);
    put_text(&mut sector, 40..72, "EVIL");
    sector[44] = 0x1b; // an escape byte inside the identifier
    sector[45] = 0x07;
    let prefix = image(vec![sector, descriptor(255, *b"CD001", 1)]);

    let volume = inspect(&prefix, 10 * 2048, None).primary.expect("primary");
    assert!(
        !volume.volume_id.chars().any(char::is_control),
        "volume id retained a control character: {:?}",
        volume.volume_id
    );
}

#[test]
fn nothing_in_the_image_is_modified_by_inspection() {
    // The parser reads a shared slice; it must not be able to mutate it, and
    // it must not depend on doing so.
    let (prefix, total) = well_formed(10);
    let before = prefix.clone();
    let _ = inspect(&prefix, total, Some("iso"));
    assert_eq!(prefix, before);
}
