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

# --- quality -----------------------------------------------------------------

# Format everything.
fmt:
    cargo fmt --all
    pnpm --dir web format

# Lint everything. Warnings are errors, as in CI.
lint:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    pnpm --dir web lint
    pnpm --dir web check

# Unit and contract tests. No database, no hardware.
test:
    cargo test --workspace --all-features
    pnpm --dir web test

# Tests that need a live PostgreSQL.
test-integration: dev-db
    TANGIBLE_TEST_DATABASE_URL=postgres://tangible:tangible@localhost:5432/tangible \
        cargo test --workspace --all-features -- --ignored

# Tests that touch a real optical drive. Never run in normal CI.
test-hardware:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "${TANGIBLE_HARDWARE_TESTS:-0}" != "1" ]; then
        echo "refusing to run hardware tests without TANGIBLE_HARDWARE_TESTS=1" >&2
        exit 1
    fi
    cargo test --workspace --all-features --features hardware-tests -- --ignored --test-threads=1

# Supply-chain checks.
audit:
    cargo deny check
    cargo audit

# The gate. If this passes, the change is ready to review.
check: lint test openapi-check compose-config

# --- generated artifacts -----------------------------------------------------

# Regenerate the OpenAPI document.
openapi:
    cargo run --quiet -p tangible-app -- openapi > openapi.json
    @echo "wrote openapi.json"

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
    echo "openapi.json is current"

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
    TANGIBLE_WORKER_NAME=validate docker compose -f deploy/compose.yaml -f deploy/compose.hardware.yaml --env-file deploy/.env.example config >/dev/null
    @echo "all compose combinations render"

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
