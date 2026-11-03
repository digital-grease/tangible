#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 digitalgrease
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Write the CUE/BIN and raw inputs the CHD fixtures are made from.

    python3 make_inputs.py <dir>

Every sector is zero but for a CD sync pattern and a sector number on data
sectors, and a sector number on audio ones, so the CHDs chdman makes of
them are a few kilobytes. See fixtures/tool-output/chdman/README.md for the
chdman commands that make the CHDs and the reading kept of each.
"""

import sys
from pathlib import Path

RAW = 2352
SYNC = bytes([0] + [0xFF] * 10 + [0])


def data_sector(lba):
    sector = bytearray(RAW)
    sector[:12] = SYNC
    sector[15] = 1
    sector[16:20] = lba.to_bytes(4, "little")
    return bytes(sector)


def audio_sector(lba):
    sector = bytearray(RAW)
    sector[:4] = lba.to_bytes(4, "little")
    return bytes(sector)


def cooked_sector(lba):
    sector = bytearray(2048)
    sector[:4] = lba.to_bytes(4, "little")
    return bytes(sector)


def iso_volume(blocks):
    """A volume descriptor set and nothing else: enough to be read as an
    ISO 9660 volume of `blocks` blocks, every other block numbered."""
    sectors = [cooked_sector(n) for n in range(blocks)]
    pvd = bytearray(2048)
    pvd[0] = 1
    pvd[1:6] = b"CD001"
    pvd[6] = 1
    pvd[40:72] = b"TANGIBLE_DVD".ljust(32)
    pvd[80:84] = blocks.to_bytes(4, "little")
    pvd[84:88] = blocks.to_bytes(4, "big")
    pvd[128:130] = (2048).to_bytes(2, "little")
    pvd[130:132] = (2048).to_bytes(2, "big")
    terminator = bytearray(2048)
    terminator[0] = 255
    terminator[1:6] = b"CD001"
    terminator[6] = 1
    sectors[16] = bytes(pvd)
    sectors[17] = bytes(terminator)
    return b"".join(sectors)


def write(out, name, sheet, data):
    (out / f"{name}.cue").write_text(sheet)
    (out / f"{name}.bin").write_bytes(data)


def main():
    out = Path(sys.argv[1])
    out.mkdir(parents=True, exist_ok=True)

    # One raw data track.
    write(out, "single-data",
          'FILE "single-data.bin" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n',
          b"".join(data_sector(n) for n in range(300)))

    # Data, then audio with its pregap in the file, then audio flagged DCP.
    write(out, "mixed-mode",
          'FILE "mixed-mode.bin" BINARY\n'
          '  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n'
          '  TRACK 02 AUDIO\n    INDEX 00 00:04:00\n    INDEX 01 00:06:00\n'
          '  TRACK 03 AUDIO\n    FLAGS DCP\n    INDEX 01 00:08:00\n',
          b"".join(data_sector(n) for n in range(300))
          + b"".join(audio_sector(n) for n in range(300, 750)))

    # Audio whose pregap the burner generates.
    write(out, "generated-pregap",
          'FILE "generated-pregap.bin" BINARY\n'
          '  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n'
          '  TRACK 02 AUDIO\n    PREGAP 00:02:00\n    INDEX 01 00:04:00\n',
          b"".join(data_sector(n) for n in range(300))
          + b"".join(audio_sector(n) for n in range(300, 400)))

    # One cooked mode 1 track.
    write(out, "cooked",
          'FILE "cooked.bin" BINARY\n  TRACK 01 MODE1\n    INDEX 01 00:00:00\n',
          b"".join(cooked_sector(n) for n in range(200)))

    # A small ISO 9660 volume of 2048-byte sectors, for createraw.
    (out / "dvd.iso").write_bytes(iso_volume(64))


if __name__ == "__main__":
    main()
