# Tangible

> A self-hosted disc-image preservation, management, and burning platform.

**Status:** pre-release. Everything listed under "What it does" works and is
tested, including on a real optical drive; the first tagged release, 0.1.0, is
being prepared. Expect the API and the manifest schema to change during 0.x,
with release notes.

Publishers keep moving toward digital-only distribution, where a purchase is a revocable license
and a delisted title simply stops existing. Discs do not work that way. Tangible exists to keep
the media you already own readable, verifiable, and reproducible, on hardware you control.

Tangible treats the disc image as the canonical object. It stores immutable originals, records provenance and hashes, models multi-disc releases, creates traceable derivatives, exports compatible game images to systems such as RomM, and operates optical burners through dedicated Docker containers.

## Why this exists

Existing media managers usually center on playable video files, episodes, songs, or ROMs. Disc images have different requirements:

- A disc may contain multiple tracks, sessions, pregaps, subchannel data, or several files.
- ISO cannot faithfully represent every optical-disc layout.
- A "successful burn" is different from a verified reproduction.
- Original images should remain immutable while conversions become derivatives.
- The system needs to reason about physical media capacity, drive capabilities, verification, and burned-copy history.
- Game, movie, software, audio, and archival discs share a common storage and burn lifecycle even when their metadata differs.

## What it does

**Library**

- Imports by upload, or from watched folders an administrator configures; a
  folder holding a disc's files imports as one artifact.
- Recognises ISO 9660 and UDF images, CUE/BIN and cdrdao TOC/BIN, by their
  structure rather than their file names, and reports what looks wrong.
- Stores every file once, by SHA-256, read-only, never renamed or rewritten,
  with a portable manifest per artifact from which the catalogue can be rebuilt.
- Catalogues titles, editions, disc sets and discs, and links images to discs.

**Burning**

- One burn worker per optical drive, each in its own container with only that
  drive's block device and group: no privileged containers, no host devices
  for anything else.
- xorriso for ISO images (CD, DVD, Blu-ray), cdrdao for CD layouts with
  audio, mixed mode and pregaps.
- Checks the disc before writing, writes from a locally staged and re-hashed
  copy, and reads the disc back afterwards: data compared byte for byte, audio
  measured, and the record says which.
- Keeps an inventory of every physical copy and how it was verified, and
  erases rewritable discs only when an administrator asks for exactly that.

**Derivatives and RomM**

- Makes a CHD of a CUE/BIN, TOC/BIN or CD ISO with chdman, and keeps it only
  if it extracts back identical to the original, track by track.
- Writes chosen games into RomM's library folder the way RomM scans it, one
  folder per game, preferring a checked CHD, and never touches files it did
  not write.

**Operations**

- A web UI and a documented REST API (`openapi.json`, generated from the code).
- Accounts with viewer, operator and administrator roles, cookie sessions and
  CSRF protection, and an audit trail of who did what.
- Hardened containers, fuzzed parsers, and release images that are signed and
  carry an SBOM and provenance.

## Not yet

URL and download-client imports, metadata providers, RomM's API, CHDs of DVDs,
CCD/IMG/SUB, MDS/MDF, BDMV and VIDEO_TS images, S3-compatible storage, and
single sign-on. See `CHANGELOG.md` for what each release adds.

## Architecture

```text
                    +----------------------+
Browser ------------> Tangible server      |
                    | REST API             |
                    | Domain services      |
                    | Durable job queues   |
                    +----+------------+----+
                         |            |
                         v            v
                 PostgreSQL      Content-addressed store
                         |
           +-------------+--------------+
           |                            |
           v                            v
burn-worker /dev/sr0            burn-worker /dev/sr1
xorriso + cdrdao                xorriso + cdrdao
```

The normal deployment is Linux and Docker Compose. The server container does not receive optical-device access. Each burn-worker container receives only its configured `/dev/srN` block device, and the drive's group so an unprivileged process can open it.

## Core principles

1. Preserve originals exactly.
2. Create derivatives; never normalize in place.
3. Treat imported content as hostile input.
4. Never mount untrusted images on the host.
5. Keep device privileges inside dedicated workers.
6. Stage and hash before writing.
7. Separate write success from verification.
8. Keep installation inspectable and reproducible.
9. Do not use remote-code shell pipelines.
10. Do not bundle content indexers or circumvention features.

## Where the design lives

The architecture is described above, and the ten principles are the rules
everything else follows from. Beyond that, the code is the specification: the
domain crate carries the entity model and state machines, the migrations carry
the storage constraints, and `openapi.json` is generated from the running API
rather than written by hand.

Structural invariants are enforced rather than documented. Prohibited constructs
fail the build through workspace lints, the database rejects states the domain
model forbids, and CI asserts that no default deployment can reach an optical
drive and that every container stays confined. Where a rule matters, there is a
test named after it.

## Installing

Tangible runs under Docker Compose on Linux. From a release:

```bash
git clone --branch vX.Y.Z --depth 1 https://github.com/digital-grease/tangible.git
cd tangible/deploy
cp .env.example .env
editor .env                 # at least POSTGRES_PASSWORD and TANGIBLE_PUBLIC_URL
docker compose config       # read what will run
docker compose up -d postgres
docker compose run --rm server migrate
docker compose up -d
```

Review the Compose file, environment file, volume mappings, device mappings,
ports and image references before starting the stack. [`deploy/README.md`](deploy/README.md)
covers first sign-in, adding a drive, RomM, CHDs, backups, upgrades, and
verifying a release's signatures. No installation step pipes a downloaded
script into a shell, and none ever will.

## Developing

You need Rust (the version pinned in `rust-toolchain.toml`), Node.js LTS with
pnpm, Docker with the Compose plugin, and optionally `just`. PostgreSQL runs
from the development Compose file.

```bash
git clone https://github.com/digital-grease/tangible.git
cd tangible
just setup        # web dependencies, the development database, migrations
just dev          # the whole stack in Docker, built from the tree
just dev-db       # or: only PostgreSQL, then `just dev-server` and `just dev-web`
just check        # what CI runs: formatting, lints, tests, OpenAPI drift, Compose checks
```

Without `just`, the `Justfile` shows the plain commands each recipe runs.
[`CONTRIBUTING.md`](CONTRIBUTING.md) describes the tests that need real tools
or a real drive, and fuzzing.

## Legal and content boundary

Tangible is neutral infrastructure for content the operator is authorized to acquire, preserve, transform, and reproduce. The core project will not bundle release indexers, decryption keys, DRM-removal logic, or instructions for defeating access controls.

## License

`AGPL-3.0-or-later`. See [`LICENSE`](LICENSE).

Tangible is a network application, so AGPL section 13 applies: if you modify it and let other
people use it over a network, those users are entitled to your modified source.
