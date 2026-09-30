# cdrdao output fixtures

Two kinds of file live here, and they are kept apart on purpose.

- **Captured** (this directory): real output from cdrdao, produced by running
  it. A parser tested against these agrees with the tool.
- **Assembled** (`assembled/`): output cdrdao can only produce with a drive
  attached, put together line by line from the format strings in cdrdao's own
  source. A parser tested against these agrees with the source, which is
  better than agreeing with a guess and worse than agreeing with a drive.

Replace every assembled file with a capture once a real drive is available,
and delete it from `assembled/` when you do.

## Provenance

cdrdao 1.2.4, package `1:1.2.4-3` from `debian:bookworm-slim`. That is the
base image and package the worker image installs (`deploy/Dockerfile`), so
these are the messages the shipped worker will see. Captured 2026-09-29 in a
container with no optical drive:

```bash
cat > Dockerfile <<'DOCKERFILE'
FROM debian:bookworm-slim
RUN apt-get update && apt-get install --no-install-recommends -y cdrdao
DOCKERFILE
docker build -t tangible-cdrdao-capture .
docker run --rm \
  -v "$PWD/capture.sh:/capture.sh:ro" \
  -v "$PWD/toc:/toc:ro" \
  -v "$PWD:/out" \
  tangible-cdrdao-capture /capture.sh
```

`capture.sh` is the exact script, and `toc/` holds the tables of contents it
ran against, in the shapes the TOC writer produces. `exit-codes.txt` records
each command's exit status, because several of the findings below are about
the exit status disagreeing with the output.

Two files are captured from a real drive: `drive-info.txt` and
`disk-info-no-disc.txt`, from a Slimtype DVD A DS8A8SH (firmware KS21) on USB,
passed to the same container as `--device /dev/sr0` with the tray empty,
captured 2026-09-29. They replaced assembled files of the same names. The
assembled empty-drive file had nine "still trying" lines where the drive
printed ten and lacked the drive banner, and the parser read both the same.
The same capture showed that cdrdao talks to the drive through `/dev/sr0`
with no `/dev/sg` node loaded.

The assembled files are built from cdrdao's source at tag `rel_1_2_4`
(`github.com/cdrdao/cdrdao`): `dao/main.cc` for the command flow, `showDiskInfo`
and `showDriveInfo`; `dao/dao.cc` for the progress lines; `trackdb/log.cc` for
the severity prefixes. The drive identity and medium numbers in them are
representative, not observed.

## What these established

Each of these changed the engine, and none of them is what a reasonable person
would guess:

1. **Everything goes to stderr, and progress lines end in `\r`.** `log.cc`
   adds no newline to a message that ends in a carriage return, so a whole
   write's progress arrives as one "line" to a reader that splits only on
   `\n`. `read-test-mixed.txt` shows it, from the same writer loop a real
   write uses.
2. **`show-toc` exits 0 for a table of contents it calls inconsistent.** A
   missing file, a file too short for its track and a track under four seconds
   all print `ERROR:` or `WARNING:` lines and still exit 0
   (`show-toc-missing-file.txt`, `show-toc-too-long.txt`,
   `show-toc-too-short.txt`). Only a syntax error exits non-zero. The check
   reads the lines.
3. **A write refuses a warning.** Anything the TOC check warns about, such as a
   track under four seconds, stops a write unless `--force` is given, which
   the engine never gives (`read-test-too-short.txt` shows the refusal).
4. **The completion line is not the last thing that can go wrong.** After
   `Writing finished successfully.` cdrdao still releases the medium lock, and
   a failure there exits 1 (`assembled/write-complete-then-unlock-failed.txt`).
   The same shape as xorriso's crash on shutdown, and handled the same way: a
   problem after the completion line is recorded, not treated as a failed
   write.
5. **`toc-size` does not count track one's pregap**, which cdrdao supplies
   itself, and does not count a gap the file carries twice.
6. **An empty drive retries for about thirty seconds** ("Unit not ready, still
   trying...", ten times at three-second intervals) before giving up.

## What each file is for

| File | What it is |
| --- | --- |
| `version.txt` | No arguments. The first line names the version; exit 1. |
| `show-toc-*.txt` | cdrdao's reading of each TOC in `toc/`, which the engine compares against the plan before a write. |
| `show-toc-missing-file.txt`, `-too-long`, `-too-short` | Inconsistent tables of contents that still exit 0. |
| `show-toc-syntax-error.txt` | The one case that exits non-zero. |
| `toc-size-*.txt` | Total blocks, for the pregap accounting in item 5. |
| `read-test-*.txt` | The writer loop without a drive: real `\r` progress, and the warning refusal. |
| `write-no-device.txt` | The engine's exact write arguments against a drive that is not there. |
| `disk-info-no-device.txt`, `drive-info-no-device.txt` | The same, for inspection and probing. |
| `drive-info.txt` | A real drive's answer. |
| `disk-info-no-disc.txt` | A real drive with an empty tray: ten retries, then it gives up. |
| `assembled/write-*.txt` | A successful write, an underrun, a failure after completion, and two refusals before writing. |
| `assembled/disk-info-*.txt` | A blank CD-R, a closed CD-RW and an appendable CD-R. |

## Refreshing these

Re-capture rather than edit. If a newer cdrdao changes its wording, the
fixtures change with it and the parser follows, so that what is tested stays
what the tool prints.
