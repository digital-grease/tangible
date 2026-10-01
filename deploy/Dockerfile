# SPDX-FileCopyrightText: 2026 digitalgrease
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Multi-stage build. The runtime stage carries the burn engines because the
# same image runs both `serve` and `burn-worker`; only the worker container
# ever receives a device, so the server cannot use them.

FROM rust:1.95-bookworm AS build
WORKDIR /src

# Cache dependencies separately from source.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY migrations ./migrations
RUN cargo build --release --locked -p tangible-app

FROM debian:bookworm-slim AS runtime

# xorriso for ISO-oriented workflows, cdrdao for DAO/audio/mixed-mode CD.
# ca-certificates for TLS to the database and integration endpoints.
#
# Installed from distro packages on purpose: publishing this image conveys
# object code for every GPL binary in it, and distro packaging means the
# corresponding source is already archived upstream.
#
# NEVER ADD, in any form:
#   cdrtools / cdrkit / wodim / genisoimage  CDDL+GPL mix, not distributable
#   cdrdao built --with-libscg               same CDDL problem
#   ffmpeg --enable-nonfree                  makes the whole image
#                                            unredistributable
#   unrar                                    binds RARLAB source; non-free
#   libdvdcss / libaacs / libbdplus          circumvention; also excluded by
#                                            the product boundary
# Adding any of these breaks distribution of the entire image, not just the
# one package. Adding to this list is a licence decision, not a build tweak.
RUN apt-get update \
 && apt-get install --no-install-recommends -y \
      xorriso \
      cdrdao \
      ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# Unprivileged by default. The worker gains device access through the device
# mapping in compose.hardware.yaml, not through user privilege.
RUN useradd --system --create-home --uid 10001 tangible

# The directories the Compose files mount named volumes on, created here and
# owned by the user the process runs as. Docker gives a new named volume the
# ownership of the directory it is mounted over, and a directory the image
# does not have is created owned by root: the first end-to-end run found the
# server refused its own library ("Permission denied") on a fresh deployment,
# and the worker's state volume would have failed the same way.
RUN mkdir -p /var/lib/tangible/library /var/lib/tangible/staging /var/lib/tangible-worker \
 && chown -R tangible:tangible /var/lib/tangible /var/lib/tangible-worker
USER tangible

COPY --from=build /src/target/release/tangible /usr/local/bin/tangible

ENTRYPOINT ["tangible"]
CMD ["serve"]
