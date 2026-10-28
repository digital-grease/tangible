# GNU ccd2cue output

What GNU ccd2cue 0.5 made of each descriptor in `fixtures/images/ccd/`, so
`crates/image/src/ccd.rs` is checked against an independent reader of the
format. A test reads each CUE back with Tangible's own CUE parser and
compares its layout with the CCD layout, track by track.

- `data-only.cue`, `mixed-mode.cue`: agree with the CCD layout in every
  track's start, pregap, length, mode, ISRC and the catalogue number.
  ccd2cue writes no `FLAGS` line, so the copy-permitted bit of
  `mixed-mode.ccd`'s third track is not in its sheet; the CCD layout keeps
  it from the entry's control bits.
- `multi-session.cue`: ccd2cue puts session 2's track at 00:00:00, on top of
  track 1, taking `[TRACK]` INDEX values as positions in the file whatever
  the session. It does not handle more than one session, which is one more
  reason the CCD layout declines to lay such an image out. Kept as a record,
  not compared.

## Provenance

Built 2026-10-27 from the GNU release, in a throwaway `debian:bookworm-slim`
container with gcc and make, and run on the descriptors there:

```bash
curl -sSfLO https://ftp.gnu.org/gnu/ccd2cue/ccd2cue-0.5.tar.gz
sha256sum ccd2cue-0.5.tar.gz
# 8725cfdbcb963259ff094a878aa0e49ea8284e1071ea36ac7ea47358c49b99c5
tar xzf ccd2cue-0.5.tar.gz && cd ccd2cue-0.5 && ./configure && make
src/ccd2cue -o mixed-mode.cue mixed-mode.ccd
```
