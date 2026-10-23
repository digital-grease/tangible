# Changelog

Notable changes to Tangible, newest first. Versions follow semantic
versioning; during 0.x the API, the manifest schema and the database schema
may change between minor versions, and the release notes say how.

## Unreleased

### Added

- Groundwork for issue reports: text is scrubbed of credentials, paths,
  serial numbers, library file names and LAN addresses before anyone can
  send it, and two settings say where reports go
  (`TANGIBLE_REPORT_ISSUES_URL`, empty to turn them off) and whether
  addresses are scrubbed. The server names its release and commit in them.
- Drives are recorded with their vendor, model and firmware, from what the
  drive itself reports, and with how they are attached (USB or SATA), read
  from the host's device tree. The Workers page shows both.
- A burn attempt records what preflight found in the tray: the media
  profile, whether it was blank, its capacity and the disc's maker
  (`medium` on `GET /api/v1/burn-attempts/{id}`).
- A burn attempt can be prepared as an issue report:
  `GET /api/v1/burn-attempts/{id}/report` fills in the burn form for a
  problem, or a new "Drive and media that worked" form for a disc that
  burned and checked, scrubbed and ready to review.
- A burn's page offers both reports, and shows everything before the issue
  form opens in a new tab. It also now shows what the worker said when an
  attempt failed, each event's message, and the disc preflight found.
- Every error the web UI shows now carries its request id, so it can be
  found in the server's log. An error that means Tangible itself went wrong
  offers "Report this…", which prepares it as a scrubbed bug report for
  review (`POST /api/v1/error-reports`).
- `tangible doctor --report` prints a diagnostics checklist safe to share:
  database and schema, storage room and writability, tools, workers and
  drives, recent failure codes and the settings that matter for support,
  with a link to a prefilled bug report. `--json` for scripts. Plain
  `doctor` is still the quick healthcheck.
- An image Tangible did not recognise, or read with doubts or warnings,
  offers "Ask for support…" on its page: a format support request that
  describes its structure (component sizes and extensions, the first bytes
  of binary files, known signatures at known offsets) with every name
  replaced (`GET /api/v1/artifacts/{id}/format-report`, new issue form
  `format_support.yml`).
- CDs can be ripped into the library (ADR 0013, first part): an operator
  asks for a rip of a drive's disc (`POST /api/v1/drives/{id}/rips`), the
  drive's worker reads it with `cdrdao read-cd`, sends the table of
  contents and data, and they are imported as a TOC/BIN artifact whose
  origin says it was dumped from a disc. A rip can be withdrawn until its
  import starts. DVDs, Blu-rays and blank discs are refused for now. A
  drive now does one thing at a time across burns, erasures and rips. The
  web UI's Rips page asks for a rip and follows it to the library, with
  progress as the disc reads.
- DVD data discs can be ripped too (ADR 0013, second part): the worker
  reads the disc sector by sector from the drive itself into an image,
  retrying a sector that fails and stopping at one that will not read, and
  it is imported as an ISO. An encrypted (CSS) DVD is recognised while it
  reads and refused, keeping nothing, including on drives that will not
  read its video at all without authentication. A disc the system has not
  caught up with since a burn is refused with a request to reload it.
  Blu-rays are still refused.
- An opt-in worker image for ripping encrypted discs, built by the operator
  and never published (`deploy/decryption/Dockerfile`, with
  `deploy/compose.decryption.yaml` to run a worker from it): VideoLAN's
  libdvdcss from its source release, pinned by checksum, and Debian's
  `dvdbackup`, `libaacs0` and `libbdplus0`. A worker started from it says so
  in its log and reports the library and tool versions to the server.
- A rip can ask to decrypt an encrypted disc, and choose its original: the
  disc as it is with a decrypted copy beside it (the default), or only a
  decrypted image (`decrypt` and `keep` on `POST /api/v1/drives/{id}/rips`).
  Off unless an administrator sets `TANGIBLE_ALLOW_DECRYPTION=true`, and
  only on a drive whose worker can; otherwise the request is refused with
  `DECRYPTION_OFF` or `ENCRYPTED_NO_DECRYPTION`, and a queued rip that can
  no longer decrypt when its worker takes it is refused the same way. The
  drive list says which drives can (`can_decrypt_rips`), and diagnostics
  shows the setting.
- The server accepts what a decrypting rip sends: a decrypted `VIDEO_TS`
  tree as the original, or the encrypted disc image with a decrypted copy
  beside it, which is imported after the original as its derivative
  (`decrypt_css`, made from the disc). A ripped artifact's manifest now
  records how it was read (`origin.physical_dump`): the drive, the engine,
  read errors, the encryption found, and whether and how it was decrypted
  at acquisition, never a key.
  The release now checks its image contains none of these before signing
  it (`deploy/check-no-decryption.sh`).
- Workers decrypt CSS DVDs for a rip that asks. The drive is authenticated
  through dvdbackup and libdvdcss first; a disc that turns out not to be
  encrypted is read as it is. Keeping the encrypted original reads the disc
  whole, scrambled sectors as they are, then makes the decrypted copy while
  the disc is still in the drive; keeping a decrypted original makes only
  the copy. dvdbackup runs with fixed arguments, a cleared environment, a
  time limit and its key cache off, and nothing it prints that could carry
  key material is kept. A disc that will not decrypt fails as
  `DECRYPTION_FAILED`, keeping nothing.
- The opt-in worker image also installs redumper (ADR 0015), from its
  GitHub release pinned by build and SHA-256 for amd64 and arm64, for CD
  rips with the drive's read offset corrected. The worker does not use it
  yet. It is not in the published image, and the release's check refuses
  it there, because it carries its own CSS and AACS code.
- The web UI asks about decryption on a drive that can decrypt: "Rip the
  disc in {drive}" opens a dialog with a "Decrypt an encrypted disc" box,
  the notice that whether ripping is lawful is the operator's to determine,
  and, once ticked, the choice of original. A rip's page shows the
  encryption found, which library and tool decrypted it, the choice made,
  and a link to the decrypted copy. An artifact's page says "Ripped from a
  disc" with the drive, the engine, read errors and the encryption, and
  states plainly when an image was decrypted when ripped and is not a
  faithful copy of the disc. For this, `GET /api/v1/artifacts/{id}` gains
  `ripped`, and a rip's `decryption` is now described in the OpenAPI
  document (`DecryptionView`) instead of as any object.
- DVD-Video `VIDEO_TS` folders are recognised: imported from a watched folder
  (the `VIDEO_TS` folder itself, or the disc's folder holding it), a folder
  whose `VIDEO_TS.IFO` really is a DVD-Video manager becomes a
  `video_ts_directory` artifact with a directory-tree topology. Missing title
  sets, backups or video, files that are not DVD-Video, odd sizes, names
  that only match ignoring case, and video still CSS-scrambled are reported
  as warnings. A VIDEO_TS folder cannot be burned, since it is a disc's
  files rather than an image of one, and a burn of one is refused at once.
- A CD burn that the drive refused while setting up, before writing
  anything, is tried once more if the disc is still blank; a drive on the
  bench did this once and burned on the next run.
- Administrators have a Diagnostics page with the same checklist, to copy,
  download, or prepare as an issue (`GET /api/v1/diagnostics` and
  `/diagnostics/report`, new permission `diagnose`).

### Fixed

- The block reader reads a DVD with direct I/O, around the page cache.
  Through the cache, the kernel read ahead of the sector asked for, so a
  refused sector a few dozen ahead made good ones fail and the error was
  blamed on the wrong sector; a damaged disc failed before it needed to. A
  whole disc no longer passes through the cache either. Where direct reads
  are refused, the reader falls back to cached ones.
- A completed import now removes its staging copy. Every import since 0.1.0
  kept a second full copy of its bytes in the staging directory, doubling
  the disk it used; those left by earlier versions can be deleted from
  `staging/imports/` for any import shown as complete.

## 0.1.0 (2026-10-07)

The first release.

### Library

- Import by upload, or from watched folders an administrator configures. A
  folder holding a disc's files imports as one artifact; symbolic links are
  refused, and hidden files are left out.
- ISO 9660 and UDF images, CUE/BIN and cdrdao TOC/BIN are recognised from
  their structure, with warnings for anything structurally odd. CUE and TOC
  references are resolved inside the import only.
- Content-addressed storage by SHA-256: every file stored once, read-only,
  never renamed or rewritten. Each artifact has a portable manifest
  (`org.tangible.artifact-manifest/v1alpha1`).
- A catalogue of titles, editions, disc sets and discs, with images linked to
  discs.

### Burning

- One burn worker per optical drive, in its own container with only that
  drive's block device and group. Workers enrol with one-use tokens, hold
  leases on their work, and recover safely after a restart without ever
  writing twice.
- xorriso writes ISO images to CD, DVD and Blu-ray media; cdrdao writes CD
  layouts with audio, mixed mode, pregaps, catalogue numbers and ISRCs.
- Preflight checks the disc before any write; the input is staged and
  re-hashed on the worker first.
- Read-back verification: ISO images compared in full; CD layouts track by
  track, data compared byte for byte and audio measured, recorded as passed
  only when everything was compared and as partial otherwise.
- An inventory of physical copies and how each was verified.
- Erasing a rewritable disc is its own operation, for administrators only,
  with an explicit confirmation. A burn never erases.

### Derivatives

- CHDs of CUE/BIN, TOC/BIN and CD ISO images, made by chdman and kept only if
  they extract back identical to the original, track by track. Lineage is
  recorded and shown, and asking twice makes one.

### RomM

- Chosen games are written into RomM's library folder as RomM scans it, one
  folder per game with its discs together, preferring a checked CHD. Only
  folders Tangible created are ever changed.

### Operations

- A web UI served by the server, and a REST API described by a generated
  OpenAPI document.
- Accounts with viewer, operator and administrator roles, cookie sessions,
  CSRF protection, sign-in rate limits, and an audit trail.
- Hardened containers checked in CI, parsers fuzzed, secrets kept out of logs
  and help text, and release images signed with SBOM and provenance.
- The licence notices of every bundled Rust crate and JavaScript package,
  generated at build time, in the image and with each release.
- Release images are scanned for vulnerabilities before they are signed, and
  one with a fixable high or critical vulnerability is not released.
- `SOURCES.md` with each release links every Debian package in the image to
  its archived source and its upstream project, and every page of the web UI
  links to Tangible's source.

### Known limitations

- Imports come only from uploads and watched folders; URL and download-client
  imports are not available yet.
- CCD/IMG/SUB, MDS/MDF, BDMV, VIDEO_TS and imported CHD images are not
  recognised.
- CHDs of DVDs need chdman 0.262 or later; the image carries 0.251.
- chdman 0.251 misreads a cdrdao TOC whose track has its pregap in the file,
  so such a TOC is refused rather than converted.
- Audio tracks are verified by length and readability, not compared, because
  without the drive's read offset a comparison fails on correct hardware.
- The RomM export writes files; it does not yet ask RomM to rescan.
- Storage is a local filesystem; S3-compatible storage is not available yet.
- Tested on one drive so far; see "Tested hardware" in `deploy/README.md`.
