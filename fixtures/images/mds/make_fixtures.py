#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 digitalgrease
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Write the MDS descriptors these fixtures are, from the format as
docs/plans/mds-images.md describes it (libmirage 3.2.10's reading of Alcohol
120% version 1 images). Run with an output directory; with --mdf it also
writes each descriptor's data file, filled with each sector's number, for
reading the images back through libmirage.

    python3 -I make_fixtures.py OUT [--mdf]
"""

import struct
import sys
from pathlib import Path

SIGNATURE = b"MEDIA DESCRIPTOR"
HEADER, SESSION, TRACK, EXTRA, FOOTER = 88, 24, 80, 8, 16


def msf(lba):
    lba += 150
    return lba // 4500, (lba // 75) % 60, lba % 75


def descriptor(medium, sessions, filename, wide=False, version=(1, 5)):
    """sessions: [(number, start, end, [track...])]; a track is a dict with
    point, mode, subchannel, ctl, sector_size, start_sector, start_offset,
    pregap, length."""
    blocks_per_session = [3 + len(tracks) for (_, _, _, tracks) in sessions]
    sessions_at = HEADER
    tracks_at = sessions_at + SESSION * len(sessions)
    all_blocks = sum(blocks_per_session)
    extras_at = tracks_at + TRACK * all_blocks
    track_count = sum(len(t) for (_, _, _, t) in sessions)
    footers_at = extras_at + EXTRA * track_count
    name_at = footers_at + FOOTER
    name = filename.encode("utf-16-le") + b"\0\0" if wide else filename.encode() + b"\0"

    out = bytearray(name_at + len(name))
    struct.pack_into("<16sBBHH", out, 0, SIGNATURE, version[0], version[1], medium, len(sessions))
    struct.pack_into("<I", out, 80, sessions_at)
    struct.pack_into("<I", out, 84, 0)

    block = tracks_at
    extra = extras_at
    for index, (number, start, end, tracks) in enumerate(sessions):
        struct.pack_into(
            "<iiHBBHHII", out, sessions_at + SESSION * index,
            start, end, number, 3 + len(tracks), 3, tracks[0]["point"], tracks[-1]["point"], 0, block,
        )
        first, last = tracks[0], tracks[-1]
        leadout = end
        entries = [
            {"point": 0xA0, "pmsf": (first["point"], 0, 0), "ctl": first["ctl"]},
            {"point": 0xA1, "pmsf": (last["point"], 0, 0), "ctl": last["ctl"]},
            {"point": 0xA2, "pmsf": msf(leadout), "ctl": last["ctl"]},
        ]
        for entry in entries:
            struct.pack_into(
                "<BBBBBBBBBBBB", out, block,
                0, 0, 0x10 | entry["ctl"], 0, entry["point"],
                0, 0, 0, 0, *entry["pmsf"],
            )
            block += TRACK
        for track in tracks:
            pmin, psec, pframe = msf(track["start_sector"])
            struct.pack_into(
                "<BBBBBBBBBBBBIH", out, block,
                track["mode"], track["subchannel"], 0x10 | track["ctl"], 0, track["point"],
                0, 0, 0, 0, pmin, psec, pframe,
                track["length"] if medium >= 0x10 else extra, track["sector_size"],
            )
            struct.pack_into("<IQII", out, block + 36, track["start_sector"], track["start_offset"], 1, footers_at)
            if medium < 0x10:
                struct.pack_into("<II", out, extra, track["pregap"], track["length"])
                extra += EXTRA
            block += TRACK
    struct.pack_into("<II", out, footers_at, name_at, 1 if wide else 0)
    out[name_at:] = name
    return bytes(out)


def track(point, kind, start_sector, start_offset, pregap, length, sector_size=2352, subchannel=0):
    mode, ctl = {
        "audio": (0xA9, 0x0),
        "mode1": (0xAA, 0x4),
        "mode2": (0xAB, 0x4),
        "dvd": (0x02, 0x4),
    }[kind]
    return {
        "point": point, "mode": mode, "subchannel": subchannel, "ctl": ctl,
        "sector_size": sector_size, "start_sector": start_sector,
        "start_offset": start_offset, "pregap": pregap, "length": length,
    }


RAW = 2352
FIXTURES = {
    # One data track: the usual 150-sector pregap, not in the file.
    "single-data": (0x00, [(1, -150, 300, [track(1, "mode1", 0, 0, 150, 300)])], "*.mdf"),
    # Data, then two audio tracks; track 2's pregap is not in the file.
    "mixed-mode": (
        0x01,
        [(1, -150, 600, [
            track(1, "mode1", 0, 0, 150, 300),
            track(2, "audio", 450, 300 * RAW, 150, 100),
            track(3, "audio", 550, 400 * RAW, 0, 50),
        ])],
        "*.mdf",
    ),
    # 96 bytes of subchannel after every sector.
    "subchannel": (
        0x00,
        [(1, -150, 100, [track(1, "mode1", 0, 0, 150, 100, sector_size=RAW + 96, subchannel=0x08)])],
        "*.mdf",
    ),
    # A DVD: one track of 2048-byte sectors, its length where a CD's extra
    # block offset would be.
    "dvd": (0x10, [(1, 0, 64, [track(1, "dvd", 0, 0, 0, 64, sector_size=2048)])], "*.mdf"),
    # A data file named outright, in UTF-16.
    "wide-name": (0x00, [(1, -150, 50, [track(1, "mode1", 0, 0, 150, 50)])], "Wide Name.mdf"),
    # Two sessions, each one data track.
    "two-sessions": (
        0x01,
        [
            (1, -150, 100, [track(1, "mode1", 0, 0, 150, 100)]),
            (2, 11500, 11650, [track(2, "mode1", 11650, 100 * RAW, 150, 50)]),
        ],
        "*.mdf",
    ),
}


def main():
    out = Path(sys.argv[1])
    with_mdf = "--mdf" in sys.argv
    out.mkdir(parents=True, exist_ok=True)
    for name, (medium, sessions, filename) in FIXTURES.items():
        wide = name == "wide-name"
        (out / f"{name}.mds").write_bytes(descriptor(medium, sessions, filename, wide=wide))
        if with_mdf:
            data = bytearray()
            for (_, _, _, tracks) in sessions:
                for t in tracks:
                    for n in range(t["length"]):
                        sector = bytearray(t["sector_size"])
                        sector[:8] = (t["start_sector"] + n).to_bytes(8, "little", signed=True)
                        data += sector
            mdf = "Wide Name.mdf" if wide else f"{name}.mdf"
            (out / mdf).write_bytes(bytes(data))


if __name__ == "__main__":
    main()
