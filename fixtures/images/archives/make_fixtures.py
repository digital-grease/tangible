#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 digitalgrease
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Make the archive fixtures (docs/plans/archives.md).

Run from this directory with p7zip's `7z` and Info-ZIP's `zip` on PATH:

    python3 -I make_fixtures.py

Each good archive holds the same small CUE/BIN, `Game/Game.cue` and
`Game/Game.bin`, so a test can check what came out byte for byte. The bad
ones each break one rule: a name escaping the archive, a symbolic link, a
hard link, an encrypted entry. Nothing here is fetched; every byte is made
here.
"""

import io
import os
import shutil
import subprocess
import tarfile
import tempfile
import zipfile

SECTOR = 2352
BIN = bytes((i * 7 + 3) % 256 for i in range(SECTOR * 4))
CUE = b'FILE "Game.bin" BINARY\r\n  TRACK 01 MODE1/2352\r\n    INDEX 01 00:00:00\r\n'


def tree(root):
    os.makedirs(os.path.join(root, "Game"))
    with open(os.path.join(root, "Game", "Game.cue"), "wb") as f:
        f.write(CUE)
    with open(os.path.join(root, "Game", "Game.bin"), "wb") as f:
        f.write(BIN)


def run(*args, cwd):
    subprocess.run(list(args), cwd=cwd, check=True, stdout=subprocess.DEVNULL)


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    with tempfile.TemporaryDirectory() as scratch:
        tree(scratch)
        # Fixed times, so the bytes do not change from one run to the next.
        for name in ("Game", "Game/Game.cue", "Game/Game.bin"):
            os.utime(os.path.join(scratch, name), (1_700_000_000, 1_700_000_000))
        run("zip", "-X", "-r", "-9", os.path.join(here, "game.zip"), "Game", cwd=scratch)
        run("7z", "a", "-bd", "-m0=lzma2", "-mx=5", os.path.join(here, "game.7z"), "Game", cwd=scratch)
        run("7z", "a", "-bd", "-pfixture", "-mhe=off", os.path.join(here, "encrypted.7z"), "Game", cwd=scratch)
        run("zip", "-X", "-r", "-P", "fixture", os.path.join(here, "encrypted.zip"), "Game", cwd=scratch)

        def tarred(mode, out):
            with tarfile.open(os.path.join(here, out), mode, format=tarfile.PAX_FORMAT) as archive:
                for name in ("Game", "Game/Game.cue", "Game/Game.bin"):
                    info = archive.gettarinfo(os.path.join(scratch, name), arcname=name)
                    info.uid = info.gid = 0
                    info.uname = info.gname = ""
                    info.mtime = 1_700_000_000
                    if info.isfile():
                        with open(os.path.join(scratch, name), "rb") as f:
                            archive.addfile(info, f)
                    else:
                        archive.addfile(info)

        tarred("w", "game.tar")
        tarred("w:gz", "game.tar.gz")
        tarred("w:xz", "game.tar.xz")

    # Names and kinds the tools above would refuse to write, written by hand.
    with zipfile.ZipFile(os.path.join(here, "escape.zip"), "w") as archive:
        archive.writestr("../escape.bin", b"outside")
    with zipfile.ZipFile(os.path.join(here, "symlink.zip"), "w") as archive:
        info = zipfile.ZipInfo("Game/link")
        info.create_system = 3
        info.external_attr = (0o120777 << 16)
        archive.writestr(info, b"/etc/passwd")
    with zipfile.ZipFile(os.path.join(here, "twice.zip"), "w") as archive:
        archive.writestr("Game.bin", b"first")
        archive.writestr("Game.bin", b"second")
    with tarfile.open(os.path.join(here, "twice.tar"), "w") as archive:
        for body in (b"first", b"second"):
            info = tarfile.TarInfo("Game.bin")
            info.size = len(body)
            archive.addfile(info, io.BytesIO(body))
    with tarfile.open(os.path.join(here, "hardlink.tar"), "w") as archive:
        data = tarfile.TarInfo("Game.bin")
        data.size = 4
        archive.addfile(data, io.BytesIO(b"data"))
        link = tarfile.TarInfo("Game2.bin")
        link.type = tarfile.LNKTYPE
        link.linkname = "Game.bin"
        archive.addfile(link)
    # What `tar -C dir -cf x.tar .` writes: everything under ./
    with tarfile.open(os.path.join(here, "dotslash.tar"), "w") as archive:
        top = tarfile.TarInfo("./")
        top.type = tarfile.DIRTYPE
        archive.addfile(top)
        data = tarfile.TarInfo("./Game.bin")
        data.size = len(BIN)
        archive.addfile(data, io.BytesIO(BIN))
    # A PAX sparse file: an ordinary entry whose data is a sparse map.
    with tarfile.open(os.path.join(here, "sparse.tar"), "w", format=tarfile.PAX_FORMAT) as archive:
        data = tarfile.TarInfo("Game.bin")
        data.size = 4
        data.pax_headers = {
            "GNU.sparse.major": "1",
            "GNU.sparse.minor": "0",
            "GNU.sparse.name": "Game.bin",
            "GNU.sparse.realsize": "1048576",
        }
        archive.addfile(data, io.BytesIO(b"1\n0\n"))
    # A file, then a folder of the same name.
    with zipfile.ZipFile(os.path.join(here, "clash.zip"), "w") as archive:
        archive.writestr("Game", b"a file")
        archive.writestr("Game/Game.bin", b"inside a folder of that name")
    with zipfile.ZipFile(os.path.join(here, "macos.zip"), "w") as archive:
        archive.writestr("Game.bin", BIN)
        archive.writestr("__MACOSX/._Game.bin", b"resource fork")
        archive.writestr(".DS_Store", b"finder")
        archive.writestr("inner.zip", b"PK\x05\x06" + b"\x00" * 18)


if __name__ == "__main__":
    main()
