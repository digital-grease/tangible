#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 digitalgrease
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Make the CHD fixtures in fixtures/images/chd from the inputs
# make_inputs.py writes, with the chdman in Tangible's own image (0.251),
# and keep `chdman info -v`'s reading of each here. See README.md.
#
#   fixtures/tool-output/chdman/make.sh <image> <work dir>
set -euo pipefail
image="$1"
work="$2"
root="$(cd "$(dirname "$0")/../../.." && pwd)"
python3 -I "$root/fixtures/images/chd/make_inputs.py" "$work"

chdman() {
  docker run --rm --network none --read-only --cap-drop ALL \
    --user "$(id -u):$(id -g)" -v "$work:/work" -w /work \
    --entrypoint chdman "$image" "$@"
}

for name in single-data mixed-mode generated-pregap cooked; do
  chdman createcd -f -i "$name.cue" -o "$name.chd"
done
# A delta: the same disc, written as a child of single-data.chd.
chdman createcd -f -i single-data.cue -o delta.chd -op single-data.chd
# chdman 0.251 has no createdvd: a raw CHD of 2048-byte units, marked as a
# DVD the way createdvd marks one. Only an uncompressed CHD takes new
# metadata, so it is made so and then copied compressed.
chdman createraw -f -i dvd.iso -o dvd-uncompressed.chd -hs 4096 -us 2048 -c none
chdman addmeta -i dvd-uncompressed.chd -t "DVD " -vt ""
chdman copy -f -i dvd-uncompressed.chd -o dvd.chd
# Not a disc.
chdman createhd -f -o hd.chd -s 1048576

for name in single-data mixed-mode generated-pregap cooked delta dvd hd; do
  cp "$work/$name.chd" "$root/fixtures/images/chd/$name.chd"
  chdman info -v -i "$name.chd" > "$root/fixtures/tool-output/chdman/$name.txt"
done
