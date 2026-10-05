# Fuzzing

Fuzz targets for every parser that reads input Tangible did not write:
disc descriptors, disc images, manifests, paths, and the output of the burn
tools.

| Target | What it feeds | What it checks beyond not panicking |
|---|---|---|
| `cue` | CUE sheet bytes | references resolve only to staged files; layout survives huge and empty file sizes; a sheet the RomM export rewrites still parses and names exactly the new files |
| `toc` | cdrdao TOC bytes | references resolve only to staged files; layout survives huge and empty file sizes |
| `iso` | ISO volume descriptors, after a zeroed system area | confidence stays a probability |
| `manifest` | manifest JSON | an accepted manifest survives being written and read back unchanged |
| `logical_path` | a path string | an accepted path is relative, normalised, free of `..`, NUL, backslashes and control characters, within the length limits, and parses to itself |
| `cdrdao_output` | text cdrdao printed | every reader of cdrdao output |
| `xorriso_output` | text xorriso printed | every reader of xorriso output |
| `romm_names` | a title or region | the RomM file name holds nothing a share refuses, is not hidden, does not end in a space or dot, and is stable when cleaned twice |

The checks live in `src/lib.rs`. Each file in `fuzz_targets/` is one line
that hands libFuzzer's input to one of them.

## Running

Fuzzing needs a nightly toolchain and `cargo-fuzz`:

```bash
rustup toolchain install nightly
cargo install cargo-fuzz --locked
just fuzz cue          # five minutes
just fuzz toc 3600     # an hour
```

What the fuzzer learns accumulates in `corpus/<target>/`, which is not
committed. A crash is written to `artifacts/<target>/`, and
`cargo +nightly fuzz fmt <target> <file>` shows the input that caused it.

## Saved inputs

- `seeds/<target>/` is the starting corpus: hand-written descriptors,
  generated ISO descriptors, and manifests. The TOC and tool-output targets
  are also seeded from `fixtures/tool-output/` in place (`FIXTURE_SEEDS` in
  `src/lib.rs`), so real cdrdao and xorriso output is not copied here.
- `regressions/<target>/` holds every input that once crashed or broke a
  check, named for what it caught. Shrink one before the fix with
  `cargo +nightly fuzz tmin <target> <file>`; after the fix it no longer
  fails, so reduce it by hand to the smallest input that would have.

Both replay on the stable toolchain with the normal tests:

```bash
just test-fuzz-corpus
```

CI runs that replay, so a fixed crash cannot come back unnoticed. It does
not fuzz: fuzzing is open-ended and belongs on a developer's machine or a
scheduled job, not on every push.
