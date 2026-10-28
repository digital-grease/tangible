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
use tangible_domain::report::{self, ScrubContext};
use tangible_image::{ccd, cue, dat, iso, toc, video_ts};
use tangible_integrations::romm;

/// One target: what it is called, and what it runs.
pub type Target = (&'static str, fn(&[u8]));

/// The targets, by name, as `cargo fuzz` and the replay test know them.
pub const TARGETS: &[Target] = &[
    ("cue", cue_sheet),
    ("toc", toc_file),
    ("ccd", ccd_file),
    ("iso", iso_head),
    ("manifest", manifest),
    ("logical_path", logical_path),
    ("cdrdao_output", cdrdao_output),
    ("xorriso_output", xorriso_output),
    ("romm_names", romm_names),
    ("report_scrub", report_scrub),
    ("video_ts", video_ts_tree),
    ("dat", dat_file),
];

/// Fixture directories, relative to the repository root, that also seed a
/// target: real tool output already kept for the parser tests, read in place
/// rather than copied into `seeds/`.
pub const FIXTURE_SEEDS: &[(&str, &str)] = &[
    ("toc", "fixtures/tool-output/cdrdao/toc"),
    ("ccd", "fixtures/images/ccd"),
    ("cdrdao_output", "fixtures/tool-output/cdrdao"),
    ("xorriso_output", "fixtures/tool-output/xorriso"),
    ("report_scrub", "fixtures/tool-output/cdrdao"),
    ("report_scrub", "fixtures/tool-output/xorriso"),
];

/// A Logiqx DAT. What it accepts has a name, and every text and hash it
/// keeps is bounded and of its shape: lowercase hex of the algorithm's
/// length, nothing a terminal would act on.
pub fn dat_file(data: &[u8]) {
    let Ok(file) = dat::parse(data) else {
        return;
    };
    let clean = |text: &str| {
        assert!(text.chars().count() <= 2048, "text is bounded");
        assert!(!text.chars().any(char::is_control), "no control characters");
        assert_eq!(text, text.trim(), "text is trimmed");
    };
    let hash = |value: &Option<String>, digits: usize| {
        if let Some(value) = value {
            assert_eq!(value.len(), digits, "a hash is its algorithm's length");
            assert!(
                value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "a hash is lowercase hex"
            );
        }
    };
    assert!(!file.header.name.is_empty(), "an accepted DAT has a name");
    clean(&file.header.name);
    for text in [
        &file.header.description,
        &file.header.version,
        &file.header.author,
        &file.header.homepage,
        &file.header.url,
    ]
    .into_iter()
    .flatten()
    {
        clean(text);
    }
    for game in &file.games {
        assert!(!game.name.is_empty(), "a kept game has a name");
        clean(&game.name);
        for rom in &game.roms {
            assert!(!rom.name.is_empty(), "a kept file has a name");
            clean(&rom.name);
            hash(&rom.crc32, 8);
            hash(&rom.md5, 32);
            hash(&rom.sha1, 40);
            hash(&rom.sha256, 64);
        }
    }
    assert!(file.warnings.len() <= 1001, "warnings are bounded");
}

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

/// A CloneCD descriptor, laid out over images of several sizes, with and
/// without a subchannel, each sector header taken from the input itself.
/// Every track laid out must lie inside the image, in order, without
/// overlapping the one before.
pub fn ccd_file(data: &[u8]) {
    let Ok(document) = ccd::parse(data) else {
        return;
    };
    for sectors in [0_u64, 1, 150, 300, 750, 4500, u64::MAX / ccd::SECTOR_BYTES] {
        let img = sectors.saturating_mul(ccd::SECTOR_BYTES);
        for sub in [None, Some(sectors.saturating_mul(ccd::SUBCHANNEL_BYTES))] {
            let mut header = |at: u64| {
                let start = usize::try_from(at % 64).unwrap_or(0);
                let mut bytes = [0_u8; ccd::HEADER_BYTES];
                for (i, byte) in bytes.iter_mut().enumerate() {
                    *byte = data.get(start + i).copied().unwrap_or(0);
                }
                Some(bytes)
            };
            let Ok(laid) = ccd::layout(&document, img, sub, &mut header) else {
                continue;
            };
            assert_eq!(laid.subchannel, sub.is_some(), "the SUB is used as given");
            let mut end = 0_u64;
            for track in &laid.layout.tracks {
                assert!(track.start_lba >= end, "tracks are in order");
                assert!(track.sector_count > 0, "tracks hold sectors");
                end = track
                    .start_lba
                    .checked_add(track.sector_count)
                    .expect("track ends without overflow");
                assert!(
                    end <= sectors,
                    "track {} lies inside the image",
                    track.number
                );
                assert_eq!(track.file_offset_bytes, track.start_lba * ccd::SECTOR_BYTES);
                assert!(
                    track.pregap_sectors < track.sector_count,
                    "INDEX 1 is inside the track"
                );
            }
        }
    }
}

/// A would-be `VIDEO_TS` tree.
///
/// Up to the first blank line, one staged path per line; after it, the bytes
/// the import would have read: the video manager's header, each title set's
/// header, then sample sectors for each title VOB in turn. Every file the
/// layout classifies must be one that was staged, each exactly once, and the
/// paths it says are required must be among them.
pub fn video_ts_tree(data: &[u8]) {
    let (paths, rest) = match data.windows(2).position(|pair| pair == b"\n\n") {
        Some(at) => (&data[..at], &data[at + 2..]),
        None => (data, &[][..]),
    };
    let paths = String::from_utf8_lossy(paths);
    let staged: Vec<&str> = paths.lines().collect();
    let Some(layout) = video_ts::locate(&staged) else {
        return;
    };
    let mut seen: Vec<&str> = layout
        .files
        .keys()
        .map(String::as_str)
        .chain(layout.others.iter().map(String::as_str))
        .collect();
    seen.sort_unstable();
    let mut expected = staged.clone();
    expected.sort_unstable();
    expected.dedup();
    assert_eq!(seen, expected, "every staged path is classified once");

    let mut chunks = rest.chunks(video_ts::HEADER_BYTES);
    let mut reads = video_ts::Reads {
        video_manager: chunks.next().unwrap_or_default().to_vec(),
        ..video_ts::Reads::default()
    };
    for set in layout.title_sets() {
        reads
            .title_set_headers
            .insert(set, chunks.next().unwrap_or_default().to_vec());
    }
    let remaining: Vec<u8> = chunks.flatten().copied().collect();
    let mut sectors = remaining.chunks(tangible_domain::dvd::SECTOR_BYTES);
    for (index, name) in layout.files.keys().enumerate() {
        reads.sizes.insert(name.clone(), (index as u64) * 1000);
    }
    for vob in layout.title_vobs() {
        let samples: Vec<Vec<u8>> = sectors.by_ref().take(2).map(<[u8]>::to_vec).collect();
        reads.samples.insert(vob.to_owned(), samples);
    }

    let Ok(found) = video_ts::inspect(&layout, &reads) else {
        return;
    };
    assert!(
        (0.0..=1.0).contains(&found.confidence),
        "confidence {} is not a probability",
        found.confidence
    );
    for required in &found.required_paths {
        assert!(
            staged.contains(&required.as_str()),
            "{required} is required but was not staged"
        );
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

/// Text scrubbed for a public issue report. The input is a name to replace,
/// a NUL, then the text; without a NUL it is all text.
///
/// The scrubber's promises: the output is bounded, no Tangible credential
/// prefix survives, a name the caller gave never appears in the output, and
/// scrubbing scrubbed text changes nothing.
pub fn report_scrub(data: &[u8]) {
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };
    let (name, text) = input.split_once('\0').unwrap_or(("", input));
    let label = "component 1";
    let context = ScrubContext::new().name(name, label);
    let scrubbed = report::scrub(text, &context);

    assert!(
        scrubbed.text.len() <= report::MAX_OUTPUT_BYTES,
        "{} bytes, past the output bound",
        scrubbed.text.len()
    );
    for prefix in report::CREDENTIAL_PREFIXES {
        assert!(
            !scrubbed.text.contains(prefix),
            "{prefix} survived in {:?}",
            scrubbed.text
        );
    }

    // Only a name made of letters, digits and inner spaces can be checked
    // this way: the scrubber's own placeholders are made of punctuation and
    // a few words, so such a name cannot be assembled from them, unless it
    // is one of those words.
    let checkable = name.trim() == name
        && name.chars().count() >= report::MIN_NAME_CHARS
        && name.chars().all(|c| c.is_alphanumeric() || c == ' ')
        && !report::PLACEHOLDERS
            .iter()
            .chain(["<component-1>"].iter())
            .any(|placeholder| placeholder.contains(name));
    if checkable {
        assert!(
            !scrubbed.text.contains(name),
            "{name:?} survived in {:?}",
            scrubbed.text
        );
    }

    let plain = report::scrub(text, &ScrubContext::new());
    if !plain.truncated {
        let again = report::scrub(&plain.text, &ScrubContext::new());
        assert_eq!(
            again.text, plain.text,
            "scrubbing scrubbed text changed it, from {text:?}"
        );
    }
}
