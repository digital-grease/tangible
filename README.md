# Tangible

> A self-hosted disc-image preservation, management, and burning platform.

**Status:** Design complete enough to begin implementation; no production release exists yet.

Publishers keep moving toward digital-only distribution, where a purchase is a revocable license
and a delisted title simply stops existing. Discs do not work that way. Tangible exists to keep
the media you already own readable, verifiable, and reproducible, on hardware you control.

Tangible treats the disc image as the canonical object. It stores immutable originals, records provenance and hashes, models multi-disc releases, creates traceable derivatives, exports compatible game images to systems such as RomM, and operates optical burners through dedicated Docker containers.

## Why this exists

Existing media managers usually center on playable video files, episodes, songs, or ROMs. Disc images have different requirements:

- A disc may contain multiple tracks, sessions, pregaps, subchannel data, or several files.
- ISO cannot faithfully represent every optical-disc layout.
- A “successful burn” is different from a verified reproduction.
- Original images should remain immutable while conversions become derivatives.
- The system needs to reason about physical media capacity, drive capabilities, verification, and burned-copy history.
- Game, movie, software, audio, and archival discs share a common storage and burn lifecycle even when their metadata differs.

## Planned capabilities

- ISO, BIN/CUE, TOC/BIN, CCD/IMG/SUB, MDS/MDF, CHD, BDMV, and VIDEO_TS modeling.
- Watched-folder, upload, URL, and download-client imports.
- Streaming SHA-256 identity plus preservation-oriented hashes.
- Immutable content-addressed storage.
- Portable artifact manifests.
- Structural validation and format-specific warnings.
- Multi-disc and multi-file artifact support.
- Linux-native burning with xorriso and cdrdao.
- One least-privileged Docker burn worker per optical drive.
- Local staging, media preflight, progress events, and read-back verification.
- Physical-copy inventory and history.
- RomM filesystem export, followed by API integration.
- Optional metadata providers without making metadata a prerequisite.

## Architecture

```text
                    +----------------------+
Browser ------------> Tangible server      |
                    | REST API + SSE       |
                    | Domain services      |
                    | Durable job queue    |
                    +----+------------+----+
                         |            |
                         v            v
                 PostgreSQL      Object storage
                         |
           +-------------+--------------+
           |                            |
           v                            v
burn-worker /dev/sr0            burn-worker /dev/sr1
xorriso + cdrdao                xorriso + cdrdao
```

The normal deployment is Linux and Docker Compose. The API container does not receive optical-device access. Each burn-worker container receives only its configured `/dev/srN` block device, and the drive's group so an unprivileged process can open it.

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

The architecture is described above, and the ten principles below are the rules
everything else follows from. Beyond that, the code is the specification: the
domain crate carries the entity model and state machines, the migration carries
the storage constraints, and `openapi.json` is generated from the running API
rather than written by hand.

Structural invariants are enforced rather than documented. Prohibited constructs
fail the build through workspace lints, the database rejects states the domain
model forbids, and CI asserts that no default deployment can reach an optical
drive. Where a rule matters, there is a test named after it.

## Development baseline

Planned prerequisites:

- Rust stable, pinned by `rust-toolchain.toml`
- Node.js LTS
- pnpm
- Docker Engine with the Compose plugin
- PostgreSQL for local integration tests
- `just`
- Linux for real optical-drive tests

Expected interface once the initial scaffold exists:

```bash
git clone <repository-url>
cd tangible
just setup
just dev
just check
```

No official installation documentation should instruct users to pipe a downloaded script into a shell.

## Production deployment principles

A future release will provide:

- Tagged source releases.
- Signed release archives.
- Checksums and provenance attestations.
- Versioned container tags and published digests.
- A reviewable Compose bundle.
- Explicit upgrade and rollback instructions.

The intended operator flow is:

```bash
git clone --branch vX.Y.Z --depth 1 <repository-url>
cd tangible/deploy
cp .env.example .env
docker compose config
docker compose pull
docker compose up -d
```

Users should review the Compose file, environment file, volume mappings, device mappings, ports, and image references before starting the stack.

## Legal and content boundary

Tangible is neutral infrastructure for content the operator is authorized to acquire, preserve, transform, and reproduce. The core project will not bundle release indexers, decryption keys, DRM-removal logic, or instructions for defeating access controls.

## License

`AGPL-3.0-or-later`. See [`LICENSE`](LICENSE).

Tangible is a network application, so AGPL section 13 applies: if you modify it and let other
people use it over a network, those users are entitled to your modified source.
