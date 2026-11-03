# chdman's reading of the CHD fixtures

The CHDs in `fixtures/images/chd/` were made by chdman 0.251, the one in
Tangible's own image, from the CUE/BINs and raw sectors
`fixtures/images/chd/make_inputs.py` writes. Each `<name>.txt` here is
`chdman info -v` of `<name>.chd`. `crates/image/src/chd.rs` compares its own
reading of the header and metadata with it, so the reader follows chdman
and not only the fixtures' author.

To make them again, with an image that has chdman (a locally built
`tangible` image does; the published one does too):

```bash
fixtures/tool-output/chdman/make.sh <image> /tmp/chd
```

It runs chdman in that image with no network, a read-only root and no
capabilities, over a scratch directory, and copies the CHDs and readings
here. chdman's output is deterministic for the same input and version, so
the CHDs come out the same.

- `single-data`: one raw mode 1 track.
- `mixed-mode`: data, then audio with a 150-frame pregap in the data
  (`PGTYPE:VAUDIO`), then audio. chdman pads each track to four frames.
- `generated-pregap`: data, then audio whose pregap is generated.
- `cooked`: one 2048-byte mode 1 track.
- `delta`: `single-data` again, as a child of `single-data.chd`.
- `dvd`: a 64-block ISO 9660 volume marked with a `DVD ` entry. chdman 0.251 has
  no `createdvd`, so it is a raw CHD given that entry, as createdvd gives
  one.
- `hd`: a blank 1 MiB hard disk, which is not a disc.
