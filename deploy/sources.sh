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
# snapshot.debian.org. This lists them, with each upstream project's own
# address, from the image itself rather than from the Dockerfile, so it names
# what actually shipped.
#
# Usage: deploy/sources.sh IMAGE VERSION > SOURCES.md

set -euo pipefail

image=${1:?usage: deploy/sources.sh IMAGE VERSION}
version=${2:?usage: deploy/sources.sh IMAGE VERSION}

# The source package, its version and its upstream homepage, for every
# installed binary package. dpkg reports the source name only when it
# differs from the binary's, and the source version only when it differs
# too, hence the fallbacks. Several binary packages can share a source; the
# sort puts a non-empty homepage first, and awk keeps one row per source.
packages=$(docker run --rm --network none --entrypoint dpkg-query "$image" \
    -W -f '${source:Package}\t${source:Version}\t${Homepage}\n' \
    | sort -t $'\t' -k1,1 -k2,2 -k3,3r \
    | awk -F '\t' '!seen[$1 FS $2]++')

cat <<EOF
# Corresponding Source for Tangible ${version}

The Tangible container image for this release contains Tangible and the
Debian packages below. This file says where the source code of each is.

## Tangible

Tangible is licensed under the GNU Affero General Public License, version 3
or later (AGPL-3.0-or-later). Its complete source for this release,
including the Dockerfile, the Compose files, the database migrations and the
web UI's build configuration, is the source archive published with the
release, \`tangible-${version}-source.tar.gz\`, and the \`${version}\` tag of
https://github.com/digital-grease/tangible. The licence text is \`LICENSE\`
in both, and \`/usr/share/doc/tangible/LICENSE\` in the image.

## Debian packages

Everything else in the image was installed from Debian's own packages,
among them GPL programs such as xorriso, cdrdao, chdman (from MAME) and
glibc. Each row is a Debian source package at the exact version shipped.
The Source link is that version's complete source as Debian built it,
patches included, archived permanently. The Project link is the upstream
project's own home, or Debian's page for the package when it names none.
Each package's licence is in the image, in
\`/usr/share/doc/<package>/copyright\`.

Every Debian source package ever published can also be found through
https://snapshot.debian.org/ and browsed at https://sources.debian.org/.

| Source package | Version | Source | Project |
|---|---|---|---|
EOF

while IFS=$'\t' read -r name ver home; do
    [ -n "$name" ] || continue
    [ -n "$home" ] || home="https://tracker.debian.org/pkg/$name"
    printf '| %s | %s | https://snapshot.debian.org/package/%s/%s/ | %s |\n' \
        "$name" "$ver" "$name" "$ver" "$home"
done <<<"$packages"

cat <<EOF

## Rust crates and JavaScript packages

The Rust crates compiled into Tangible and the JavaScript bundled into its
web UI are under permissive licences such as MIT and Apache-2.0. Each is
listed, with its licence text and its source repository, in
\`THIRD_PARTY_NOTICES-rust.txt\` and \`THIRD_PARTY_NOTICES-web.txt\`,
published with the release and carried in the image under
\`/usr/share/doc/tangible/\`.
EOF
