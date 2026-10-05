// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Every saved input, through its target, on the stable toolchain.
//!
//! `seeds/<target>/` and the fixture directories in `FIXTURE_SEEDS` are the
//! starting corpus, `regressions/<target>/` every input that once crashed or
//! broke a promise. All of them run here with the normal test runner, so CI
//! checks them without a nightly compiler, and a fixed crash cannot quietly
//! come back.

use std::path::{Path, PathBuf};

/// Every file under `dir`, recursively, as libFuzzer reads a corpus.
fn files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
        if path.is_dir() {
            found.extend(files(&path));
        } else if path.is_file() {
            found.push(path);
        }
    }
    found.sort();
    found
}

fn replay(dir: &Path, target: &str, run: fn(&[u8])) -> usize {
    let paths = files(dir);
    for path in &paths {
        let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        eprintln!("{target}: {}", path.display());
        run(&bytes);
    }
    paths.len()
}

#[test]
fn every_saved_input_passes_its_target() {
    let fuzz = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repository = fuzz.parent().unwrap_or(fuzz);
    for (target, run) in tangible_fuzz::TARGETS {
        let mut seeds = replay(&fuzz.join("seeds").join(target), target, *run);
        for (seeded, dir) in tangible_fuzz::FIXTURE_SEEDS {
            if seeded == target {
                let count = replay(&repository.join(dir), target, *run);
                assert!(count > 0, "{dir} seeds {target} but holds nothing");
                seeds += count;
            }
        }
        replay(&fuzz.join("regressions").join(target), target, *run);
        assert!(seeds > 0, "{target} has no seeds");
    }
}

/// The manifest seeds ending in `.json` must be accepted, or the fuzzer
/// starts from documents the decoder refuses on the first field and never
/// reaches validation or the round trip.
#[test]
fn the_well_formed_manifest_seeds_are_accepted() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("seeds/manifest");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let path = entry.unwrap_or_else(|e| panic!("{e}")).path();
        if path.extension().is_some_and(|ext| ext == "json") {
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{e}"));
            if let Err(error) = tangible_domain::ArtifactManifest::from_json(&text) {
                panic!("{} is refused: {error:?}", path.display());
            }
            checked += 1;
        }
    }
    assert!(checked > 0, "no well-formed manifest seeds");
}

#[test]
fn every_target_has_a_binary() {
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
            .unwrap_or_else(|e| panic!("Cargo.toml: {e}"));
    for (target, _) in tangible_fuzz::TARGETS {
        assert!(
            manifest.contains(&format!("name = \"{target}\"")),
            "{target} is in TARGETS but has no [[bin]] in Cargo.toml"
        );
    }
}
