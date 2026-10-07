#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 digitalgrease
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Write the licence notices for every Rust crate in the tangible binary.

Walks `cargo metadata` from the tangible-app package through normal and build
dependencies only (development dependencies never reach the binary), for the
platform being built, and prints each crate with its licence expression and
the licence files it ships. Most crates are MIT or Apache-2.0, whose terms
require their notices to accompany the binary.

Resolving the graph needs every dependency's manifest, development ones
included, so cargo may download sources the build did not. Usage:

    python3 deploy/third-party-notices.py > THIRD_PARTY_NOTICES-rust.txt
"""

import json
import os
import re
import subprocess
import sys

ROOT_PACKAGE = "tangible-app"
LICENSE_FILE = re.compile(r"^(licen[cs]e|copying|notice|unlicense)([.\-_]|$)", re.IGNORECASE)


def host_triple():
    out = subprocess.run(["rustc", "-vV"], capture_output=True, text=True, check=True).stdout
    for line in out.splitlines():
        if line.startswith("host: "):
            return line.split(": ", 1)[1].strip()
    sys.exit("rustc did not report its host triple")


def metadata():
    out = subprocess.run(
        [
            "cargo", "metadata", "--format-version", "1", "--locked",
            "--filter-platform", host_triple(),
        ],
        stdout=subprocess.PIPE, text=True, check=True,
    ).stdout
    return json.loads(out)


def shipped(meta):
    """Package ids reachable from the binary through non-development edges."""
    nodes = {node["id"]: node for node in meta["resolve"]["nodes"]}
    root = next(p["id"] for p in meta["packages"] if p["name"] == ROOT_PACKAGE)
    seen, stack = set(), [root]
    while stack:
        current = stack.pop()
        if current in seen:
            continue
        seen.add(current)
        for dep in nodes[current]["deps"]:
            kinds = {kind["kind"] for kind in dep.get("dep_kinds", [])}
            if kinds - {"dev"}:
                stack.append(dep["pkg"])
    return seen


def licence_texts(package):
    directory = os.path.dirname(package["manifest_path"])
    texts = []
    for name in sorted(os.listdir(directory)):
        path = os.path.join(directory, name)
        if LICENSE_FILE.match(name) and os.path.isfile(path):
            with open(path, encoding="utf-8", errors="replace") as handle:
                texts.append(handle.read().strip())
    if package.get("license_file"):
        path = os.path.join(directory, package["license_file"])
        if os.path.isfile(path):
            with open(path, encoding="utf-8", errors="replace") as handle:
                text = handle.read().strip()
            if text not in texts:
                texts.append(text)
    return texts


def main():
    meta = metadata()
    workspace = set(meta["workspace_members"])
    ids = shipped(meta)
    packages = sorted(
        (p for p in meta["packages"] if p["id"] in ids and p["id"] not in workspace),
        key=lambda p: (p["name"], p["version"]),
    )
    missing = []
    sections = []
    for package in packages:
        texts = licence_texts(package)
        if not texts:
            missing.append(f"{package['name']} {package['version']}")
        sections.append("\n".join([
            f"{package['name']} {package['version']}",
            f"License: {package.get('license') or 'see below'}",
            f"Source: {package.get('repository') or 'crates.io'}",
            "",
            "\n\n".join(texts) if texts else
            "(the crate ships no licence file; its licence expression is above)",
        ]))
    rule = "\n\n" + "=" * 72 + "\n\n"
    print("Third-party Rust crates in the tangible binary\n")
    print("These crates are compiled into the tangible program. Each is listed with")
    print(f"the licence it is distributed under. {len(packages)} crates.\n")
    print(rule.join(sections))
    if missing:
        print(f"note: {len(missing)} crate(s) ship no licence file: {', '.join(missing)}",
              file=sys.stderr)


if __name__ == "__main__":
    main()
