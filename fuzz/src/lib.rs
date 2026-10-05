// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! What each fuzz target checks.
//!
//! Every function here takes arbitrary bytes, feeds them to one parser that
//! reads untrusted input, and panics if the parser panics or breaks one of
//! its promises. The fuzz targets in `fuzz_targets/` call these under
//! libFuzzer; `tests/replay.rs` calls them on every saved input with the
//! ordinary test runner, so a crash found once stays fixed.
//!
//! Not panicking is the floor. Where a parser makes a promise its callers
//! rely on, the harness checks that too: a path it accepts is safe to join
//! onto a root, a manifest it accepts survives being written and read back,
//! a reference it resolves is one of the files actually staged.

use std::collections::BTreeMap;

use tangible_burn::{cdrdao, xorriso};
use tangible_domain::LogicalPath;
use tangible_domain::logical_path::{MAX_PATH_BYTES, MAX_SEGMENT_BYTES};
use tangible_domain::manifest::ArtifactManifest;
use tangible_image::{cue, iso, toc};
use tangible_integrations::romm;

/// One target: what it is called, and what it runs.
pub type Target = (&'static str, fn(&[u8]));

/// The targets, by name, as `cargo fuzz` and the replay test know them.
pub const TARGETS: &[Target] = &[
    ("cue", cue_sheet),
    ("toc", toc_file),
    ("iso", iso_head),
    ("manifest", manifest),
    ("logical_path", logical_path),
    ("cdrdao_output", cdrdao_output),
    ("xorriso_output", xorriso_output),
    ("romm_names", romm_names),
];

/// Fixture directories, relative to the repository root, that also seed a
/// target: real tool output already kept for the parser tests, read in place
/// rather than copied into `seeds/`.
pub const FIXTURE_SEEDS: &[(&str, &str)] = &[
    ("toc", "fixtures/tool-output/cdrdao/toc"),
    ("cdrdao_output", "fixtures/tool-output/cdrdao"),
    ("xorriso_output", "fixtures/tool-output/xorriso"),
];

/// File sizes to lay a descriptor out against: plausible, empty, one sector
/// short, and as large as a `u64` goes, where an unchecked sum would wrap.
fn size_sets(files: usize) -> [Vec<u64>; 4] {
    let plausible = (0..files)
        .map(|i| (i as u64 + 1) * 2352 * 300)
        .collect::<Vec<_>>();
    [
        plausible.clone(),
        vec![0; files],
        plausible
            .iter()
            .map(|size| size.saturating_sub(1))
            .collect(),
        vec![u64::MAX; files],
    ]
}

/// The staged files a descriptor is resolved against: whatever names it
/// declares that are valid paths, in their own case and folded, plus a
/// bystander.
fn staged_for(names: &[String]) -> Vec<LogicalPath> {
    let mut staged = Vec::new();
    for name in names {
        for candidate in [name.clone(), name.to_lowercase(), name.to_uppercase()] {
            if let Ok(path) = LogicalPath::parse(&candidate)
                && !staged.contains(&path)
            {
                staged.push(path);
            }
        }
    }
    if let Ok(path) = LogicalPath::parse("unrelated.bin") {
        staged.push(path);
    }
    staged
}

fn assert_resolution_stays_staged(resolution: &cue::ReferenceResolution, staged: &[LogicalPath]) {
    for file in &resolution.files {
        if let Some(resolved) = &file.resolved {
            assert!(
                staged.contains(resolved),
                "{:?} resolved to {resolved}, which was never staged",
                file.declared
            );
        }
    }
}

/// A CUE sheet: parsed, laid out, its references resolved, and its FILE
/// lines rewritten as the RomM export rewrites them.
pub fn cue_sheet(data: &[u8]) {
    let Ok(sheet) = cue::parse(data) else {
        return;
    };
    assert!(
        (0.0..=1.0).contains(&sheet.confidence),
        "confidence {} is not a probability",
        sheet.confidence
    );

    let names: Vec<String> = sheet.files.iter().map(|file| file.name.clone()).collect();
    let staged = staged_for(&names);
    assert_resolution_stays_staged(&cue::resolve_references(&sheet, &staged), &staged);

    for sizes in size_sets(sheet.files.len()) {
        let _ = cue::layout(&sheet, &sizes);
    }

    // The export renames every file and rewrites the sheet to match. When it
    // says it managed, the result must still be a sheet naming exactly the
    // new names, in the same order: RomM's emulator opens what it names.
    let renames: BTreeMap<String, String> = names
        .iter()
        .enumerate()
        .map(|(i, name)| (name.clone(), format!("Game (Track {:02}).bin", i + 1)))
        .collect();
    if let Ok(rewritten) = cue::rewrite_file_names(data, |name| renames.get(name).cloned()) {
        let reparsed = cue::parse(&rewritten)
            .unwrap_or_else(|error| panic!("a rewritten sheet no longer parses: {error}"));
        let expected: Vec<&String> = names.iter().map(|name| &renames[name]).collect();
        let actual: Vec<&String> = reparsed.files.iter().map(|file| &file.name).collect();
        assert_eq!(actual, expected, "the rewritten sheet names other files");
    }
}

/// A cdrdao table of contents: parsed, laid out, references resolved.
pub fn toc_file(data: &[u8]) {
    let Ok(document) = toc::parse(data) else {
        return;
    };
    let staged = staged_for(&document.files);
    assert_resolution_stays_staged(&cue::resolve_names(&document.files, &staged), &staged);
    for sizes in size_sets(document.files.len()) {
        let _ = toc::layout(&document, &sizes);
    }
}

/// The head of a would-be ISO image.
///
/// The first eight bytes are the file's claimed total length and the ninth
/// picks an extension hint. The rest is placed after an all-zero system area,
/// where the volume descriptors live, so the fuzzer spends its effort on the
/// descriptors rather than on 32 KiB of padding. The raw bytes are inspected
/// as well, which covers files too short to hold a descriptor at all.
pub fn iso_head(data: &[u8]) {
    let check = |inspection: &iso::IsoInspection| {
        assert!(
            (0.0..=1.0).contains(&inspection.confidence),
            "confidence {} is not a probability",
            inspection.confidence
        );
    };
    check(&iso::inspect(data, data.len() as u64, None));

    let Some((header, descriptors)) = data.split_first_chunk::<9>() else {
        return;
    };
    let mut length = [0_u8; 8];
    length.copy_from_slice(&header[..8]);
    let total = u64::from_le_bytes(length);
    let hint = [None, Some("iso"), Some("bin"), Some("img")][usize::from(header[8] % 4)];

    let room = iso::REQUIRED_PREFIX_BYTES - iso::SYSTEM_AREA_BYTES;
    let mut prefix = vec![0_u8; iso::SYSTEM_AREA_BYTES];
    prefix.extend_from_slice(&descriptors[..descriptors.len().min(room)]);
    check(&iso::inspect(&prefix, total, hint));
}

/// A manifest document. One that is accepted must survive being written and
/// read back unchanged: manifests are rewritten, and the artifact record can
/// be rebuilt from them.
pub fn manifest(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(parsed) = ArtifactManifest::from_json(text) else {
        return;
    };
    let written = parsed
        .to_json()
        .unwrap_or_else(|error| panic!("an accepted manifest cannot be written: {error}"));
    let reread = ArtifactManifest::from_json(&written)
        .unwrap_or_else(|error| panic!("a written manifest is refused on reading: {error:?}"));
    assert_eq!(reread, parsed, "a manifest changed on a round trip");
}

/// A path from a descriptor, an archive or a manifest. One that is accepted
/// must be safe to join onto a root, and parse to itself.
pub fn logical_path(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(path) = LogicalPath::parse(text) else {
        return;
    };
    let accepted = path.as_str();
    assert!(!accepted.is_empty(), "accepted an empty path");
    assert!(
        accepted.len() <= MAX_PATH_BYTES,
        "accepted an over-long path"
    );
    assert!(
        !accepted.starts_with('/'),
        "accepted an absolute path: {accepted:?}"
    );
    assert!(
        !accepted.contains(['\0', '\\']) && !accepted.chars().any(char::is_control),
        "accepted a NUL, backslash or control character: {accepted:?}"
    );
    assert!(
        !(accepted
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
            && accepted.as_bytes().get(1) == Some(&b':')),
        "accepted a drive prefix: {accepted:?}"
    );
    for segment in accepted.split('/') {
        assert!(
            !segment.is_empty() && segment != "." && segment != "..",
            "accepted segment {segment:?} in {accepted:?}"
        );
        assert!(
            segment.len() <= MAX_SEGMENT_BYTES,
            "accepted an over-long segment"
        );
        assert!(
            !segment.ends_with([' ', '.']),
            "accepted a segment Windows would rename: {segment:?}"
        );
    }
    assert_eq!(
        LogicalPath::parse(accepted).ok().as_ref(),
        Some(&path),
        "an accepted path does not parse to itself"
    );
}

/// Whatever cdrdao printed, through every reader of its output.
pub fn cdrdao_output(data: &[u8]) {
    use cdrdao::parse;
    let output = String::from_utf8_lossy(data);
    for line in parse::lines(&output) {
        let _ = parse::message(line);
        let _ = parse::write_event(line);
    }
    let _ = parse::messages(&output);
    let _ = parse::version(&output);
    let _ = parse::device_unavailable(&output);
    let _ = parse::no_disc(&output);
    let _ = parse::toc_listing(&output);
    let _ = parse::readback_tracks(&output);
    let _ = parse::read_completed(&output);
    let _ = parse::disk_info(&output);
    let _ = parse::drive_info(&output, "/dev/sr0");
    let _ = parse::write_outcome(&output);
}

/// Whatever xorriso printed, through every reader of its output.
pub fn xorriso_output(data: &[u8]) {
    use xorriso::parse;
    let output = String::from_utf8_lossy(data);
    let _ = parse::version(&output);
    let _ = parse::devices(&output);
    let _ = parse::medium(&output);
    let _ = parse::blank_outcome(&output);
    let _ = parse::write_outcome(&output);
    let _ = parse::drive_refused(&output);
    let _ = parse::media_regions(&output);
}

/// A catalog title or region, made into a RomM folder or file name. The
/// result is written to disk on whatever share RomM reads, so it must hold
/// nothing any of them refuses or rewrites.
pub fn romm_names(data: &[u8]) {
    let raw = String::from_utf8_lossy(data);
    let name = romm::safe_name(&raw);
    assert!(!name.is_empty(), "an empty name for {raw:?}");
    assert!(
        name.len() <= romm::MAX_NAME_BYTES,
        "{} bytes, past the limit that leaves room under 255 for the suffixes",
        name.len()
    );
    assert!(
        !name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
            && !name.chars().any(char::is_control),
        "{name:?} holds a character a share refuses"
    );
    assert!(!name.starts_with('.'), "{name:?} would be hidden");
    assert!(
        !name.ends_with([' ', '.']),
        "{name:?} ends in a character Windows strips"
    );
    assert!(!name.contains("  "), "{name:?} has a run of spaces");
    assert_eq!(
        romm::safe_name(&name),
        name,
        "safe_name is not idempotent on {raw:?}"
    );
}
