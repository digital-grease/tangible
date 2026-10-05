#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 digitalgrease
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Write SOURCES.md for an image: where the Corresponding Source of everything
# in it is.
#
# Publishing the image conveys object code for every GPL program inside it,
# xorriso, cdrdao and glibc among them, not only Tangible's own. The runtime
# stage installs nothing but Debian packages, so each one's source is a
# Debian source package at an exact version, archived permanently by
# snapshot.debian.org. This lists them, from the image itself rather than
# from the Dockerfile, so it names what actually shipped.
#
# Usage: deploy/sources.sh IMAGE VERSION > SOURCES.md

set -euo pipefail

image=${1:?usage: deploy/sources.sh IMAGE VERSION}
version=${2:?usage: deploy/sources.sh IMAGE VERSION}

# The source package and version of every installed binary package. dpkg
# reports the source name only when it differs from the binary's, and the
# source version only when it differs too, hence the fallbacks.
packages=$(docker run --rm --network none --entrypoint dpkg-query "$image" \
    -W -f '${source:Package}\t${source:Version}\n' | sort -u)

cat <<EOF
# Corresponding Source for Tangible ${version}

The Tangible container image for this release contains Tangible and the
Debian packages below. This file says where the source code of each is.

## Tangible

Tangible is licensed under the GNU Affero General Public License, version 3
or later. Its complete source for this release, including the Dockerfile,
the Compose files, the database migrations and the web UI's build
configuration, is the source archive published with the release,
\`tangible-${version}-source.tar.gz\`, and the \`${version}\` tag of
https://github.com/digital-grease/tangible.

## Debian packages

Everything else in the image was installed from Debian's own packages. Each
row is a Debian source package at the exact version shipped; its source is
archived permanently at the linked address.

| Source package | Version | Source |
|---|---|---|
EOF

while IFS=$'\t' read -r name ver; do
    [ -n "$name" ] || continue
    printf '| %s | %s | https://snapshot.debian.org/package/%s/%s/ |\n' \
        "$name" "$ver" "$name" "$ver"
done <<<"$packages"
