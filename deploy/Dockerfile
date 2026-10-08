# SPDX-FileCopyrightText: 2026 digitalgrease
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Multi-stage build. The runtime stage carries the burn engines because the
# same image runs both `serve` and `burn-worker`; only the worker container
# ever receives a device, so the server cannot use them.
#
# Every base image is pinned by digest, so a release rebuilt from its tag uses
# the same bases, and the SBOM and SOURCES.md name exactly what shipped. The
# tag beside each digest says what it is; to move to newer bases, pull the
# tag and replace the digest with `docker image inspect --format
# '{{index .RepoDigests 0}}' <tag>`.

# The web UI: a static build, served by the server from the same origin as
# the API. Node exists only in this stage; the runtime image has none.
FROM node:24-bookworm-slim@sha256:0e0ff40c39bc087845bfb27465a0df4ea419520094bc35842ff83dd8cbe6f9b6 AS web
WORKDIR /web
RUN npm install --global pnpm@11.26.0
# Dependencies first, from the lockfile alone, so they are cached across
# source changes. pnpm-workspace.yaml carries the install-script allowlist.
COPY web/package.json web/pnpm-lock.yaml web/pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile
COPY web ./
RUN pnpm build

FROM rust:1.98-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e AS build
WORKDIR /src

# Cache dependencies separately from source.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY migrations ./migrations
RUN cargo build --release --locked -p tangible-app
# The licence notices of every crate compiled in, from the sources the build
# just used, so they always match the binary. MIT and Apache-2.0 require them
# to travel with it.
COPY deploy/third-party-notices.py ./deploy/
RUN python3 deploy/third-party-notices.py > THIRD_PARTY_NOTICES-rust.txt

FROM debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587 AS runtime

# xorriso for ISO-oriented workflows, cdrdao for DAO/audio/mixed-mode CD,
# mame-tools for chdman, which makes CHD derivatives (it brings SDL2 and its
# libraries, about 75 MB; Debian builds chdman only in that package).
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
#
# The upgrade applies Debian security fixes published since the base image
# was cut: Debian refreshes its images about monthly, its security archive
# daily, and the release workflow refuses an image with a fixable high or
# critical vulnerability. SOURCES.md is read from the built image, so it
# names the upgraded versions.
RUN apt-get update \
 && apt-get upgrade -y \
 && apt-get install --no-install-recommends -y \
      xorriso \
      cdrdao \
      mame-tools \
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
COPY --from=web /web/build /usr/share/tangible/web
# Third-party licence notices: the Rust crates in the binary, and the
# JavaScript bundled into the web UI (also served beside it). Debian's own
# packages keep theirs under /usr/share/doc as usual.
COPY --from=build /src/THIRD_PARTY_NOTICES-rust.txt /usr/share/doc/tangible/
COPY --from=web /web/build/THIRD_PARTY_NOTICES.txt /usr/share/doc/tangible/THIRD_PARTY_NOTICES-web.txt
COPY LICENSE /usr/share/doc/tangible/LICENSE

# Serve the UI built above. Unset it to run the API alone.
ENV TANGIBLE_WEB_ROOT=/usr/share/tangible/web

# Set by the release workflow; a local build says what it is not. Last, so
# a new version or revision does not rebuild the layers above.
ARG VERSION=dev
ARG REVISION=unknown
ARG CREATED=unknown

# Where the Corresponding Source is: this image conveys object code for every
# GPL program in it, and for Tangible itself under the AGPL. SOURCES.md, with
# each release, lists the Debian source package of everything installed.
LABEL org.opencontainers.image.title="Tangible" \
      org.opencontainers.image.description="Self-hosted disc-image preservation, cataloguing and burning" \
      org.opencontainers.image.source="https://github.com/digital-grease/tangible" \
      org.opencontainers.image.url="https://github.com/digital-grease/tangible" \
      org.opencontainers.image.licenses="AGPL-3.0-or-later" \
      org.opencontainers.image.vendor="digitalgrease" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.created="${CREATED}"

ENTRYPOINT ["tangible"]
CMD ["serve"]
