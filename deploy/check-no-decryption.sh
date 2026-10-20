#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 digitalgrease
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Fail if an image carries decryption code (ADR 0013).
#
# The image Tangible publishes never contains libdvdcss, libaacs or libbdplus,
# the tools built on them, or a key database; an operator who wants to rip
# encrypted discs builds deploy/decryption/Dockerfile themselves. This lists
# every file in the image's filesystem, without running anything in it, and
# refuses any that is one of those.
#
# Usage:
#   deploy/check-no-decryption.sh IMAGE

set -euo pipefail

if [ $# -ne 1 ]; then
    echo "usage: $0 IMAGE" >&2
    exit 2
fi

container=$(docker create "$1")
trap 'docker rm "$container" > /dev/null' EXIT

# Base names: the libraries under any soname, the tools, the record the
# opt-in image writes, and key databases under the names the libraries look
# for.
found=$(docker export "$container" | tar -t \
    | grep -E '(^|/)(libdvdcss[^/]*|libaacs[^/]*|libbdplus[^/]*|dvdbackup|tangible-bd-backup|decryption\.json|KEYDB\.cfg|keydb\.cfg)$' \
    || true)

if [ -n "$found" ]; then
    echo "decryption code in $1:" >&2
    echo "$found" | sed 's/^/  /' >&2
    exit 1
fi
echo "no decryption code: $1"
