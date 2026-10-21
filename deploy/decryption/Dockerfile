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

FROM ${TANGIBLE_IMAGE}

ARG LIBDVDCSS_VERSION=1.6.0

USER root

# dvdbackup brings libdvdread8; its recommendation of libdvdcss2 names
# Debian's libdvd-pkg downloader, which is not wanted: libdvdcss comes from
# the stage above.
RUN apt-get update \
 && apt-get install --no-install-recommends -y dvdbackup libaacs0 libbdplus0 \
 && rm -rf /var/lib/apt/lists/*

COPY --from=libdvdcss /out/usr/local/ /usr/local/
RUN ldconfig

# What the worker reports, written from what is actually installed. Versions
# only: nothing here names a key or a key file.
RUN set -eu; \
    pkg() { dpkg-query --showformat='${Version}' --show "$1"; }; \
    test -e /usr/local/lib/libdvdcss.so.2; \
    test -x /usr/bin/dvdbackup; \
    mkdir -p /usr/share/tangible; \
    printf '{"css":{"library":"libdvdcss","version":"%s","tool":"dvdbackup","tool_version":"%s"},"aacs":{"library":"libaacs","version":"%s","bdplus_version":"%s"}}\n' \
      "${LIBDVDCSS_VERSION}" "$(pkg dvdbackup)" "$(pkg libaacs0)" "$(pkg libbdplus0)" \
      > /usr/share/tangible/decryption.json

# libdvdcss caches title keys under the home directory unless told not to.
# The worker's root filesystem is read-only, and keys are not kept anywhere.
ENV DVDCSS_CACHE=off

USER tangible

LABEL org.opencontainers.image.title="Tangible burn worker with decryption (locally built)" \
      org.opencontainers.image.description="Built by its operator from deploy/decryption/Dockerfile; not a Tangible release"
