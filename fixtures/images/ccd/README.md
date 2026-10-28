# CloneCD descriptor fixtures

`.ccd` descriptors for `crates/image/src/ccd.rs`, written by hand for this
repository in the layout CloneCD writes (CRLF line ends, `[CloneCD]`,
`[Disc]`, `[Session N]`, one `[Entry N]` per raw TOC entry with `Point`,
`ADR` and `Control` in hex, then `[TRACK N]`). They are not taken from any
disc. No IMG or SUB is kept: the tests give the layout the sizes those
files would have and the first bytes of the sectors it asks for.

- `data-only.ccd`: one mode 1 data track of 300 sectors.
- `mixed-mode.ccd`: a 300-sector data track; an audio track whose pregap
  (`INDEX 0=300`, `INDEX 1=450`) is in the image, with an ISRC; an audio
  track with the copy-permitted control bit; the lead-out at 750; a
  catalogue number.
- `multi-session.ccd`: an audio track in session 1 and a data track in
  session 2, 11,250 sectors after its lead-out, which the layout declines.

They also seed the `ccd` fuzz target.

## How the format was read

There is no published specification. The parser follows CloneCD's format as
its open-source readers take it: libmirage's CCD parser (CDEmu,
`libmirage/images/image-ccd/parser.c`, GPL-2, read in October 2026 to learn
the format; no code is taken from it). From it:

- Entries with `Point` 1 to 99 start tracks at `PLBA`; `0xA0` opens a
  session, `0xA1` names its last track, `0xA2` is its lead-out; other points
  do not shape the layout.
- The IMG holds the tracks' sectors back to back, the 150-sector first
  pregap not among them. A track's pregap is `INDEX 1 - INDEX 0`, and its
  length runs to the next track's start less that track's pregap, or to the
  lead-out.
- `MODE` is not trusted over the sector itself.
- The IMG and SUB are named after the `.ccd`.

GNU ccd2cue 0.5 reads them independently: its CUE for each is in
`fixtures/tool-output/ccd2cue/`, and the single-session layouts agree with
it track by track (a test checks). A CloneCD image of a real disc has not
been checked.
