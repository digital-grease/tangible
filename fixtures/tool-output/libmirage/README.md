# libmirage's reading of the MDS fixtures

`mds-layouts.txt` is how libmirage 3.2.10 (Debian trixie's `libmirage11` and
`gir1.2-mirage-3.2`) reads each descriptor in `fixtures/images/mds/`, with a
data file written beside it, one line per track. `crates/image/src/mds.rs`
compares its own reading with it, so the parser follows another reader and
not only the fixtures' author.

To produce it again, in a scratch container (libmirage is GPL; it is run,
never linked or shipped):

```bash
docker run --rm -it -v "$PWD":/repo:ro -v /tmp/mds:/images debian:trixie sh -c '
  apt-get update -qq && apt-get install -y -qq python3 python3-gi gir1.2-mirage-3.2 libmirage11 &&
  python3 -I /repo/fixtures/images/mds/make_fixtures.py /images --mdf &&
  cd /images && python3 /repo/fixtures/tool-output/libmirage/mirage_layout.py \
    single-data.mds mixed-mode.mds subchannel.mds dvd.mds wide-name.mds two-sessions.mds'
```

The start, length and pregap are libmirage's: a track starts where its
pregap does, and its length counts the pregap. The fragments are the
generated pregap (size 0) and the data, with its offset in the data file,
main size and subchannel size.
