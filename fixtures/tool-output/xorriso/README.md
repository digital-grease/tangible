# xorriso output fixtures

Real output from xorriso, captured rather than written by hand. A parser tested
against invented output only proves the parser agrees with whoever invented it.

## Provenance

All captured on 2026-08-27 from `xorriso 1.5.6` (libburnia), as packaged in
`alpine:3.20`, running in a container with no optical drive:

```bash
docker run --rm -v "$PWD/out:/out" alpine:3.20 sh -c '
  apk add --no-cache xorriso
  xorriso -version
  xorriso -as mkisofs -V TESTVOL -o /tmp/source.iso /src
  xorriso -as cdrecord -v dev=stdio:/tmp/target.iso blank=as_needed padsize=300k /tmp/source.iso
  xorriso -outdev stdio:/tmp/target.iso -toc
  xorriso -devices
'
```

The target is `stdio:`, a file standing in for a medium. That is xorriso's own
mechanism, not a simulation of ours, so the messages are the ones a real drive
produces for everything except the parts that talk to hardware.

Two more were captured on 2026-09-29 from a real drive, a Slimtype DVD A
DS8A8SH (firmware KS21) on USB, using xorriso 1.5.6 from Ubuntu 24.04's package
with `--device /dev/sr0` and the tray empty:

```bash
xorriso -abort_on FATAL -devices
xorriso -abort_on FATAL -outdev /dev/sr0 -toc
```

Then, with a blank 80 minute CD-R in the tray, `-toc` again and the engine's
write arguments with `-dummy` added, which writes with the laser off; the disc
was checked blank afterwards.

The drive's serial number in the captures is replaced with `X`s. It
identifies one physical drive and has no business in a public repository.

## What each file is for

| File | What it is |
| --- | --- |
| `version.txt` | `-version`. Establishes the version and the libraries in use. |
| `devices-none.txt` | `-devices` on a machine with no drives. The empty case has to be recognised rather than mistaken for a parse failure. |
| `toc-blank.txt` | `-toc` against a blank medium. |
| `toc-written.txt` | `-toc` against a medium holding one session. |
| `write-success.txt` | A 115 MB write, with the progress lines a long write produces. |
| `write-insufficient-space.txt` | A write refused because the image is larger than the medium. Exit code 5. |
| `devices-one.txt` | `-devices` on a machine with one real drive. |
| `toc-blank-cdr.txt` | `-toc` on a real blank CD-R. |
| `write-success-cdr.txt` | The first real xorriso burn, 2026-09-30: a 4 MB ISO to a CD-R through the combined engine, read back identical over SCSI. Raw stderr; stdout was empty. Progress repeats once a second whether or not it moved, which is why the engine reports a megabyte only when it changes. |
| `write-dummy-cdr.txt` | A real dummy write. Note the `libburn : NOTE :` line: the library that drives the laser reports under its own prefix, which the parser did not recognise until this capture. |
| `toc-no-disc.txt` | `-toc` on a real drive with an empty tray. It says `Media current: is not recognizable`, which the parser used to read as a profile, so an empty drive looked like a disc that could not be written. |
| `write-success-then-abort.txt` | **The interesting one.** The write completed and xorriso then crashed on shutdown, exiting non-zero. |

That last file is why the parser reports what the output said and leaves the
verdict to the caller. A tool's exit code and its own account of the write can
disagree, and this is not a hypothetical: it happened on the first capture.
Trusting the exit code alone would have called a completed write a failure;
and, worse, the opposite mistake is available too, which is why a written disc
is never treated as a verified one.

## Refreshing these

Re-capture rather than edit. If a newer xorriso changes its wording, the
fixtures should change with it and the parser should be updated to match, so
that what is tested stays what the tool actually prints.
