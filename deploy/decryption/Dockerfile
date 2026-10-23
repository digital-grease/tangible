# SPDX-FileCopyrightText: 2026 digitalgrease
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# OPT-IN. A burn-worker image that can rip encrypted discs (ADR 0013).
#
# The image Tangible publishes contains no decryption code. This file adds it,
# on top of a published image, on the operator's own machine, and nothing the
# project builds or releases ever runs it. Whether ripping a disc is lawful
# where you are is yours to determine.
#
# Build it explicitly, from the release you run, and read what it fetches
# first:
#
#   docker build -f deploy/decryption/Dockerfile \
#     --build-arg TANGIBLE_IMAGE=ghcr.io/digital-grease/tangible:v0.1.0 \
#     -t tangible-decryption:v0.1.0 deploy/decryption
#
# The result is yours: it is outside the release's signature, SBOM,
# provenance and vulnerability scan. Rebuild it whenever you move to a new
# release, so it carries that release's program and Debian's fixes.
#
# What it adds:
#
#   libdvdcss  built from VideoLAN's source release, pinned below by version
#              and SHA-256. Debian does not ship it. libdvdread, which
#              dvdbackup uses, loads it at run time when it is present.
#   dvdbackup  Debian's, to copy a DVD's video into a VIDEO_TS tree.
#   libaacs0   Debian's (main: no keys in it). Reads keys only from the
#   libbdplus0 KEYDB.cfg the operator mounts; for Blu-ray, later.
#   redumper   upstream's static Linux build, pinned below by build and
#              SHA-256, for CD rips with the drive's read offset corrected
#              (ADR 0015). It is here, not in the published image, because
#              it also carries CSS and AACS code and commands that crack DVD
#              keys; the worker only ever runs its drive::detect, dump and
#              split, on a CD.
#
# /usr/share/tangible/decryption.json records what was installed, for the
# worker to report. The worker believes it only when the library and the
# tool are also at the fixed places it looks for them itself.

# The published image to extend. Required: pin the release you run, or its
# digest. A locally built image works too, for testing.
ARG TANGIBLE_IMAGE

# libdvdcss is compiled in a throwaway stage, so the worker image gets the
# library and its licence and none of the compiler.
FROM debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587 AS libdvdcss

# To move to a newer release: take the version and its .sha256 file from
# https://download.videolan.org/pub/videolan/libdvdcss/ and check that file
# against the tarball yourself before changing both lines.
ARG LIBDVDCSS_VERSION=1.6.0
ARG LIBDVDCSS_SHA256=7ea556c846b7bfc32d47b41cae56d1863a6b6d5f706bb162778d6f298490977c

RUN apt-get update \
 && apt-get install --no-install-recommends -y \
      ca-certificates curl xz-utils gcc libc6-dev meson ninja-build pkg-config \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /build
# The checksum is checked before the archive is opened; a mismatch stops the
# build.
RUN curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
      -o libdvdcss.tar.xz \
      "https://download.videolan.org/pub/videolan/libdvdcss/${LIBDVDCSS_VERSION}/libdvdcss-${LIBDVDCSS_VERSION}.tar.xz" \
 && echo "${LIBDVDCSS_SHA256}  libdvdcss.tar.xz" | sha256sum --check --strict \
 && tar -xJf libdvdcss.tar.xz \
 && cd "libdvdcss-${LIBDVDCSS_VERSION}" \
 && meson setup build --prefix=/usr/local --libdir=lib \
      --buildtype=release -Ddefault_library=shared \
 && meson compile -C build \
 && meson install -C build --destdir /out \
 && install -D -m 0644 COPYING /out/usr/local/share/doc/libdvdcss/COPYING \
 && rm -rf /out/usr/local/include /out/usr/local/lib/pkgconfig

# redumper is fetched in a throwaway stage too, so the worker image gets the
# binary and its licence and nothing used to fetch them.
FROM debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587 AS redumper

# To move to a newer build: take the build number and each zip's SHA-256
# from https://github.com/superg/redumper/releases (GitHub shows a digest
# for every asset), check them against the downloads yourself, and change
# all four lines together. Then rip a test CD before trusting it
# (docs/plans/redumper.md, phase F).
ARG REDUMPER_BUILD=b768
ARG REDUMPER_SHA256_AMD64=34409e24beb1acb11db975bdda12a7818096b6b92642266cfb0a12b4f700baf9
ARG REDUMPER_SHA256_ARM64=6e578b5e9c5ab577eac41b5a2b1dd2bcbf8a723bc1698a13797023203696577c
# GPL-3.0's text, from the same tag.
ARG REDUMPER_LICENSE_SHA256=3972dc9744f6499f0f9b2dbf76696f2ae7ad8af9b23dde66d6af86c9dfb36986

RUN apt-get update \
 && apt-get install --no-install-recommends -y ca-certificates curl unzip \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /build
# Both checksums are checked before anything is opened; a mismatch, or an
# architecture with no pinned build, stops the build. The architecture is
# this stage's own, which is the image's: asking dpkg works with the legacy
# builder too, which does not set TARGETARCH.
RUN set -eu; \
    arch="$(dpkg --print-architecture)"; \
    case "${arch}" in \
      amd64) asset=x64;   sha="${REDUMPER_SHA256_AMD64}" ;; \
      arm64) asset=arm64; sha="${REDUMPER_SHA256_ARM64}" ;; \
      *) echo "no pinned redumper build for ${arch}" >&2; exit 1 ;; \
    esac; \
    base="https://github.com/superg/redumper"; \
    fetch() { curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 -o "$1" "$2"; }; \
    fetch redumper.zip "${base}/releases/download/${REDUMPER_BUILD}/redumper-${REDUMPER_BUILD}-linux-${asset}.zip"; \
    echo "${sha}  redumper.zip" | sha256sum --check --strict; \
    fetch LICENSE "https://raw.githubusercontent.com/superg/redumper/${REDUMPER_BUILD}/LICENSE"; \
    echo "${REDUMPER_LICENSE_SHA256}  LICENSE" | sha256sum --check --strict; \
    unzip -q redumper.zip; \
    install -D -m 0755 "redumper-${REDUMPER_BUILD}-linux-${asset}/bin/redumper" /out/usr/local/bin/redumper; \
    install -D -m 0644 LICENSE /out/usr/local/share/doc/redumper/LICENSE

FROM ${TANGIBLE_IMAGE}

ARG LIBDVDCSS_VERSION=1.6.0
ARG REDUMPER_BUILD=b768

USER root

# dvdbackup brings libdvdread8; its recommendation of libdvdcss2 names
# Debian's libdvd-pkg downloader, which is not wanted: libdvdcss comes from
# the stage above.
RUN apt-get update \
 && apt-get install --no-install-recommends -y dvdbackup libaacs0 libbdplus0 \
 && rm -rf /var/lib/apt/lists/*

COPY --from=libdvdcss /out/usr/local/ /usr/local/
COPY --from=redumper /out/usr/local/ /usr/local/
RUN ldconfig

# What the worker reports, written from what is actually installed. Versions
# only: nothing here names a key or a key file.
RUN set -eu; \
    pkg() { dpkg-query --showformat='${Version}' --show "$1"; }; \
    test -e /usr/local/lib/libdvdcss.so.2; \
    test -x /usr/bin/dvdbackup; \
    test -x /usr/local/bin/redumper; \
    mkdir -p /usr/share/tangible; \
    printf '{"css":{"library":"libdvdcss","version":"%s","tool":"dvdbackup","tool_version":"%s"},"aacs":{"library":"libaacs","version":"%s","bdplus_version":"%s"},"redumper":{"build":"%s"}}\n' \
      "${LIBDVDCSS_VERSION}" "$(pkg dvdbackup)" "$(pkg libaacs0)" "$(pkg libbdplus0)" "${REDUMPER_BUILD}" \
      > /usr/share/tangible/decryption.json

# libdvdcss caches title keys under the home directory unless told not to.
# The worker's root filesystem is read-only, and keys are not kept anywhere.
ENV DVDCSS_CACHE=off

USER tangible

LABEL org.opencontainers.image.title="Tangible burn worker with decryption (locally built)" \
      org.opencontainers.image.description="Built by its operator from deploy/decryption/Dockerfile; not a Tangible release"
