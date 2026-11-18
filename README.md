# Tangible

[![CI](https://img.shields.io/github/actions/workflow/status/digital-grease/tangible/ci.yml?branch=main&logo=github&label=CI)](https://github.com/digital-grease/tangible/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/digital-grease/tangible?logo=github&label=release)](https://github.com/digital-grease/tangible/releases/latest)
[![License: AGPL-3.0-or-later](https://img.shields.io/badge/license-AGPL--3.0--or--later-blue.svg)](https://github.com/digital-grease/tangible/blob/main/LICENSE)
[![Runs on Linux + Docker Compose](https://img.shields.io/badge/runs%20on-Linux%20%2B%20Docker%20Compose-2496ED?logo=docker&logoColor=white)](https://github.com/digital-grease/tangible/blob/main/deploy/README.md)
[![GitHub Issues](https://img.shields.io/github/issues/digital-grease/tangible)](https://github.com/digital-grease/tangible/issues)

<a href="https://www.buymeacoffee.com/digitalgrease"><img src="https://cdn.buymeacoffee.com/buttons/v2/default-red.png" alt="Buy Me A Coffee" height="60" width="217"></a>

> A self-hosted disc-image preservation, management, and burning platform.

**Status:** early release, 0.4.0. Everything listed under "What it does" works
and is tested, including on a real optical drive. Expect the API and the
manifest schema to change during 0.x, with release notes.

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

- Imports by upload (in resumable parts for large files), from watched folders
  an administrator configures, from a URL, from qBittorrent, SABnzbd and
  NZBGet, and from S3-compatible storage; a folder holding a disc's files
  imports as one artifact, and ZIP, 7z and tar archives are unpacked. Every
  import is one a person asked for.
- Recognises ISO 9660 and UDF images, CUE/BIN, cdrdao TOC/BIN, CloneCD
  CCD/IMG/SUB (keeping its subchannel), Alcohol 120% MDS/MDF, CHD, and
  DVD-Video `VIDEO_TS` folders, by their structure rather than their file
  names, and reports what looks wrong.
- Stores every file once, by SHA-256, read-only, never renamed or rewritten,
  with a portable manifest per artifact from which the catalogue can be rebuilt.
  Integrity checks read it back, on request or on a schedule.
- Deletes an artifact only when an administrator asks, keeping its record,
  hashes and history as a tombstone; its files go with it, except those
  another artifact shares. Other files nothing refers to, such as what an
  import stored before it failed, go only from a plan that lists each one and
  where it came from: an administrator looks it over and collects it, or the
  server does on a schedule you set.
- Computes preservation hashes (CRC32, MD5, SHA-1) of every file and CD track,
  and matches them against the Logiqx DATs you load, such as Redump's.
- Catalogues titles, editions, disc sets and discs, and links images to discs.
- Shows cover art: a request's, from the provider it was found with; libretro's
  box art for a game a loaded DAT matches exactly; or one picked by hand. The
  server fetches each once and serves it itself.

**Requests**

- Anybody signed in may ask for an item, found among loaded DATs, MusicBrainz,
  TMDB (and IMDb IDs, which TMDB looks up) or IGDB, or described by hand,
  stating on every request that they legally own it and want a digital image
  of it.
- An operator or administrator approves or declines. For an approved request
  they may search the Prowlarr you run and pick a release, which Tangible
  fetches through Prowlarr and hands to one of your download clients; the
  import that follows fulfils the request. Nothing is searched for or grabbed
  on its own; see "Legal and content boundary" below.

**Ripping**

- Rips CDs (cdrdao, or redumper in the opt-in image) and DVDs in a worker's
  drive into the library, recording the drive, the tools and every read error.
- Rips encrypted DVDs from an opt-in worker image you build yourself; see
  "Legal and content boundary" below.

**Burning**

- One burn worker per optical drive, each in its own container with only that
  drive's block device and group: no privileged containers, no host devices
  for anything else. A worker runs on the server's machine or on another one,
  and its drive can be named by the `/dev/disk/by-id` name that stays with it
  when it is plugged in again.
- xorriso for ISO images (CD, DVD, Blu-ray), cdrdao for CD layouts with
  audio, mixed mode and pregaps.
- Checks the disc before writing, writes from a locally staged and re-hashed
  copy, and reads the disc back afterwards: data compared byte for byte, audio
  measured, and the record says which.
- Keeps an inventory of every physical copy and how it was verified, and
  erases rewritable discs only when an administrator asks for exactly that.

**Derivatives and RomM**

- Makes CUE/BIN, ISO and CHD derivatives (chdman, CD and DVD), each recording
  what produced it, and keeps a CHD only if it extracts back identical to the
  original.
- Writes chosen games into RomM's library folder the way RomM scans it, one
  folder per game, preferring a checked CHD, and never touches files it did
  not write. With RomM's API connected, asks RomM to scan what changed and
  links each game to its page there.

**Operations**

- A web UI, in a light and a dark theme, with a live view of every drive, and
  a documented REST API (`openapi.json`, generated from the code) with an
  event stream.
- Accounts with viewer, operator and administrator roles, cookie sessions and
  CSRF protection, sign-in through an OpenID Connect provider, and an audit
  log of who did what.
- Worker enrollment, draining and revocation, and credential rotation.
- Diagnostics safe to share, Prometheus metrics, and `restore-check` for after
  restoring a backup.
- Issue reports prepared for you: a failed burn, an error, a drive that worked
  or an image Tangible could not read becomes a prefilled issue with
  credentials, host paths and file names replaced, which your own browser
  opens only after you have read it. The server sends nothing.
- Hardened containers, fuzzed parsers, and release images that are signed and
  carry an SBOM and provenance.

## Not yet

Ripping Blu-ray discs, BDMV images, burning a `VIDEO_TS` folder, and
S3-compatible storage for the library itself (S3 is an import source). See
`CHANGELOG.md` for what each release adds.

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
10. Do not bundle content indexers, decryption keys, or decryption code.

## Installing

Tangible runs under Docker Compose on Linux; a drive's worker needs Compose
2.29.3 or later. In a directory of its own:

```bash
curl -fLO https://github.com/digital-grease/tangible/releases/latest/download/compose.yaml
docker compose config       # read what will run
docker compose up -d        # makes its secrets, copies and migrates the database, then starts
```

Then open `http://<this machine>:8080` and make the first administrator.
Nothing has to be set first. Settings go in `.env`, each with a default:
`curl -fL -o .env https://github.com/digital-grease/tangible/releases/latest/download/env.example`
gives the list. `TANGIBLE_PUBLIC_URL` matters once Tangible is reached over
HTTPS or signs in through an identity provider.

`docker compose pull` and `docker compose up -d` then take each patch release
as it comes; a new minor version is its own `compose.yaml`, downloaded the
same way. Every release file, `compose.yaml` included, is listed in the
release's signed `SHA256SUMS`, and a tagged clone or the signed deploy archive
installs the same way. [`deploy/README.md`](deploy/README.md) covers first
sign-in, adding a drive (on the server's machine, or on another one with
`compose.worker.yaml`), RomM, CHDs, backups, collecting files nothing needs,
upgrades, and verifying a release's signatures. No installation step pipes a
downloaded script into a shell, and none ever will.

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

Tangible is infrastructure for content the operator is authorized to acquire, preserve, transform, and reproduce. The project will not bundle release indexers or decryption keys, and will not fetch or derive keys.

Requests search only the Prowlarr an administrator configures, with the indexers the operator added to it; Tangible ships and configures none. A search is made by an operator or administrator, for a request they approved, and a release is grabbed only when they pick it. Each request carries the requester's statement that they legally own the item and want a digital image of it; Tangible requires it but cannot check it. Which indexers you search, and whether downloading a release is lawful where you are, is yours to determine.

Ripping discs in a worker's drive follows the arrangement VideoLAN and the Linux distributions use: the published images contain no decryption code. An operator who wants to rip encrypted DVDs builds a worker image locally from `deploy/decryption/Dockerfile`, which compiles VideoLAN's libdvdcss from its source release, and supplies any key files themselves. Decryption stays off until an administrator enables it. Whether ripping a particular disc is lawful where you are is yours to determine.

## License

`AGPL-3.0-or-later`. See [`LICENSE`](LICENSE).

Tangible is a network application, so AGPL section 13 applies: if you modify it and let other
people use it over a network, those users are entitled to your modified source.
