# Changelog

Notable changes to Tangible, newest first. Versions follow semantic
versioning; during 0.x the API, the manifest schema and the database schema
may change between minor versions, and the release notes say how.

## 0.1.0 (unreleased)

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
