# SPDX-FileCopyrightText: 2026 digitalgrease
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# The stable developer interface. Recipe names are contracts:
# CI calls the same ones you do, so a green local `just check` means something.

set shell := ["bash", "-euo", "pipefail", "-c"]

compose := "docker compose -f deploy/compose.yaml -f deploy/compose.dev.yaml"

# List available recipes.
default:
    @just --list

# --- setup -------------------------------------------------------------------

# Prepare a working development environment. Installs nothing system-wide.
setup: _check-tools
    @echo "==> frontend dependencies"
    pnpm --dir web install --frozen-lockfile
    @echo "==> development database"
    just dev-db
    @echo "==> configuration"
    @test -f deploy/.env || { cp deploy/.env.example deploy/.env && echo "created deploy/.env"; }
    @test -f config/local.toml || { cp config/example.toml config/local.toml && echo "created config/local.toml"; }
    @echo "==> migrations"
    just migrate
    @echo "==> openapi"
    just openapi
    @echo
    @echo "Setup complete. Start with: just dev"

# Fail early with a readable message rather than a confusing error later.
_check-tools:
    #!/usr/bin/env bash
    set -euo pipefail
    missing=()
    for tool in cargo pnpm docker; do
        command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
    done
    if [ ${#missing[@]} -gt 0 ]; then
        echo "missing required tools: ${missing[*]}" >&2
        echo "install them and re-run; see CONTRIBUTING.md" >&2
        exit 1
    fi

# --- run ---------------------------------------------------------------------

# Run the whole stack in containers.
dev:
    {{compose}} up --build

# Start only PostgreSQL, for running the server on the host.
dev-db:
    {{compose}} up -d postgres

# Run the server on the host against the containerised database.
dev-server:
    cargo run -p tangible-app -- serve

# Run the frontend dev server on the host.
dev-web:
    pnpm --dir web dev

# Run a burn worker on the host against the local server.
#
# The token is needed only on the first run: the credential it is exchanged for
# is stored under .dev-data/worker and reused after that.
dev-worker token="":
    cargo run -p tangible-app -- burn-worker \
        --server-url http://localhost:8080 \
        --worker-name dev-worker \
        {{ if token == "" { "" } else { "--enrollment-token " + token } }}

# --- quality -----------------------------------------------------------------

# Format everything.
fmt:
    cargo fmt --all
    cargo fmt --manifest-path fuzz/Cargo.toml
    pnpm --dir web format

# Lint everything. Warnings are errors, as in CI.
lint:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo fmt --manifest-path fuzz/Cargo.toml -- --check
    cargo clippy --manifest-path fuzz/Cargo.toml --lib --tests -- -D warnings
    pnpm --dir web lint
    pnpm --dir web check

# Unit and contract tests. No database, no hardware.
test:
    cargo test --workspace --all-features
    just test-fuzz-corpus
    pnpm --dir web test

# Every saved fuzz input through its target, on the stable toolchain: the seeds
# and every input that once crashed. No fuzzing happens here.
test-fuzz-corpus:
    cargo test --manifest-path fuzz/Cargo.toml --test replay

# Fuzz one parser: cue, toc, iso, manifest, logical_path, cdrdao_output,
# xorriso_output or romm_names. Needs a nightly toolchain and cargo-fuzz.
# What it learns accumulates in fuzz/corpus/, which is not committed. A crash
# is written to fuzz/artifacts/; once fixed, copy it into fuzz/regressions/.
fuzz target seconds="300":
    #!/usr/bin/env bash
    set -euo pipefail
    cd fuzz
    # Real tool output kept for the parser tests seeds these targets too
    # (FIXTURE_SEEDS in fuzz/src/lib.rs).
    case "{{target}}" in
        toc) fixtures=(../fixtures/tool-output/cdrdao/toc) ;;
        cdrdao_output) fixtures=(../fixtures/tool-output/cdrdao) ;;
        xorriso_output) fixtures=(../fixtures/tool-output/xorriso) ;;
        *) fixtures=() ;;
    esac
    mkdir -p corpus/{{target}} seeds/{{target}} regressions/{{target}}
    cargo +nightly fuzz run {{target}} corpus/{{target}} seeds/{{target}} \
        regressions/{{target}} "${fixtures[@]}" -- \
        -max_total_time={{seconds}} -rss_limit_mb=1024 -max_len=65536

# Tests that need a live PostgreSQL.
test-integration: dev-db
    TANGIBLE_TEST_DATABASE_URL=postgres://tangible:tangible@localhost:5432/tangible \
        cargo test --workspace --all-features -- --ignored

# Tests that run the real xorriso against a file target. No drive involved.
#
# Skipped rather than failed when xorriso is missing, so a developer without it
# still gets a green suite. Set TANGIBLE_XORRISO_BIN to point at a binary that
# is not on PATH.
test-xorriso:
    TANGIBLE_XORRISO_TESTS=1 cargo test -p tangible-burn --test xorriso_engine --all-features

# Tests that run the real cdrdao over the tables of contents the engine writes.
# No drive involved: cdrdao cannot write to a file, so these check what it
# reads a table of contents as, and how it refuses a drive that is not there.
#
# Skipped rather than failed when cdrdao is missing. TANGIBLE_CDRDAO_BIN may
# point at a wrapper that runs it in a container, if the wrapper mounts the
# temporary directory at the same path.
test-cdrdao:
    TANGIBLE_CDRDAO_TESTS=1 cargo test -p tangible-burn --test cdrdao_engine --all-features

# Tests that run the real chdman: make a CHD, verify it, extract it again and
# compare every track with the original. Skipped rather than failed when
# chdman is missing. TANGIBLE_CHDMAN_BIN may point at a wrapper that runs it
# in a container, if the wrapper mounts the temporary directory at the same
# path and keeps the working directory.
test-chdman:
    TANGIBLE_CHDMAN_TESTS=1 cargo test -p tangible-image --test chdman --all-features

# Tests that touch a real optical drive. Never run in normal CI.
#
# TANGIBLE_HARDWARE_TESTS=1 runs the ones that only ask the drive questions.
# Adding TANGIBLE_HARDWARE_WRITE=1 runs the ones that burn, each of which uses
# up a disc, so name one: just test-hardware an_iso
# TANGIBLE_HARDWARE_DEVICE names the drive (default /dev/sr0).
test-hardware *filter:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "${TANGIBLE_HARDWARE_TESTS:-0}" != "1" ]; then
        echo "refusing to run hardware tests without TANGIBLE_HARDWARE_TESTS=1" >&2
        exit 1
    fi
    cargo test -p tangible-burn --all-features --test hardware {{filter}} -- --test-threads=1 --nocapture

# Supply-chain checks.
audit:
    cargo deny check
    cargo audit

# The gate. If this passes, the change is ready to review.
check: lint test openapi-check compose-config

# --- generated artifacts -----------------------------------------------------

# Regenerate the OpenAPI document and the frontend types derived from it.
# One recipe, because a document without matching types is a drift waiting to
# happen.
openapi:
    cargo run --quiet -p tangible-app -- openapi > openapi.json
    pnpm --dir web gen:api
    @echo "wrote openapi.json and web/src/lib/api-types.ts"

# Fail if the checked-in document has drifted.
openapi-check:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo run --quiet -p tangible-app -- openapi > /tmp/tangible-openapi.json
    if ! diff -u openapi.json /tmp/tangible-openapi.json; then
        echo >&2
        echo "openapi.json is out of date. Run: just openapi" >&2
        exit 1
    fi
    pnpm --dir web gen:api
    if ! git diff --quiet -- web/src/lib/api-types.ts; then
        echo >&2
        echo "web/src/lib/api-types.ts is out of date. Run: just openapi" >&2
        exit 1
    fi
    echo "openapi.json and the generated client are current"

# Build the test fixture corpus.
fixtures:
    cargo run --quiet -p tangible-app -- fixtures generate

# --- database ----------------------------------------------------------------

# Apply pending migrations.
migrate:
    cargo run --quiet -p tangible-app -- migrate

# Create a new migration: just migration-new name=create_artifacts
migration-new name:
    sqlx migrate add -r "{{name}}"

# Drop and recreate the development database. Development only.
db-reset:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "${TANGIBLE_ALLOW_DB_RESET:-0}" != "1" ]; then
        echo "refusing to reset without TANGIBLE_ALLOW_DB_RESET=1" >&2
        exit 1
    fi
    {{compose}} down -v postgres
    just dev-db
    just migrate

# --- deployment --------------------------------------------------------------

# Validate that every Compose combination renders.
compose-config:
    docker compose -f deploy/compose.yaml --env-file deploy/.env.example config >/dev/null
    docker compose -f deploy/compose.yaml -f deploy/compose.dev.yaml --env-file deploy/.env.example config >/dev/null
    TANGIBLE_WORKER_NAME=validate TANGIBLE_OPTICAL_GID=990 docker compose -f deploy/compose.yaml -f deploy/compose.hardware.yaml --env-file deploy/.env.example config >/dev/null
    TANGIBLE_WORKER_NAME=validate TANGIBLE_OPTICAL_GID=990 TANGIBLE_SCSI_DEVICE=/dev/sg4 docker compose -f deploy/compose.yaml -f deploy/compose.hardware.yaml -f deploy/compose.hardware-sg.yaml --env-file deploy/.env.example config >/dev/null
    @echo "all compose combinations render"
    just compose-hardening

# Fail if any Compose combination loosens a container's confinement
# (deploy/check-hardening.sh says what is required).
compose-hardening:
    docker compose -f deploy/compose.yaml --env-file deploy/.env.example config --format json | deploy/check-hardening.sh
    docker compose -f deploy/compose.yaml -f deploy/compose.dev.yaml --env-file deploy/.env.example config --format json | deploy/check-hardening.sh --allow-writable server
    TANGIBLE_WORKER_NAME=validate TANGIBLE_OPTICAL_GID=990 docker compose -f deploy/compose.yaml -f deploy/compose.hardware.yaml --env-file deploy/.env.example config --format json | deploy/check-hardening.sh
    TANGIBLE_WORKER_NAME=validate TANGIBLE_OPTICAL_GID=990 TANGIBLE_SCSI_DEVICE=/dev/sg4 docker compose -f deploy/compose.yaml -f deploy/compose.hardware.yaml -f deploy/compose.hardware-sg.yaml --env-file deploy/.env.example config --format json | deploy/check-hardening.sh

# Assert that no default stack can reach an optical drive.
verify-no-devices:
    #!/usr/bin/env bash
    set -euo pipefail
    for combo in "-f deploy/compose.yaml" "-f deploy/compose.yaml -f deploy/compose.dev.yaml"; do
        if docker compose $combo --env-file deploy/.env.example config | grep -qE "devices:|privileged: true"; then
            echo "device or privileged access found in a default stack: $combo" >&2
            exit 1
        fi
    done
    echo "no default stack exposes a device"

# Tear the development stack down.
down:
    {{compose}} down
