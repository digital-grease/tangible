#!/bin/sh
# SPDX-FileCopyrightText: 2026 digitalgrease
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Captures real cdrdao output for the parser tests. Run inside the same base
# image the worker ships (see README.md); writes each command's combined
# output to /out/<name>.txt and its exit status to /out/exit-codes.txt.
set -u
RAW=2352
mkdir -p /work && cd /work

# One mixed-mode image: 1000 raw data sectors then 3000 audio sectors.
head -c $((RAW * 4000)) /dev/urandom > disc.bin
head -c $((RAW * 3000)) /dev/urandom > track01.bin
head -c $((RAW * 1500)) /dev/urandom > track02.bin
# Shorter than the table of contents will claim.
head -c $((RAW * 100)) /dev/urandom > short.bin
cp /toc/*.toc /work/

: > /out/exit-codes.txt
run() {
  name=$1; shift
  "$@" > /out/$name.txt 2>&1
  echo "$name $?" >> /out/exit-codes.txt
}

run version cdrdao
for t in audio mixed mixed-start flags codes; do
  run show-toc-$t cdrdao show-toc $t.toc
done
run toc-size-mixed cdrdao toc-size mixed.toc
run toc-size-mixed-start cdrdao toc-size mixed-start.toc
run read-test-mixed cdrdao read-test mixed.toc
for t in missing-file too-long too-short syntax-error; do
  run show-toc-$t cdrdao show-toc $t.toc
done
run read-test-missing-file cdrdao read-test missing-file.toc
run read-test-too-short cdrdao read-test too-short.toc
# The engine's own write arguments, against a drive that is not there.
run write-no-device cdrdao write --device /dev/disc-block -n -v 2 /work/mixed.toc
run disk-info-no-device cdrdao disk-info --device /dev/disc-block
run drive-info-no-device cdrdao drive-info --device /dev/disc-block
