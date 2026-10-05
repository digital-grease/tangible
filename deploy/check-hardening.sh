#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 digitalgrease
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Fail if a rendered Compose stack loosens a container's confinement.
#
# Reads `docker compose ... config --format json` on stdin. Every service
# must:
#
#   - not be privileged, and not share the host's network, PID or IPC
#     namespace
#   - set no-new-privileges
#   - drop ALL capabilities, adding back only what its image provably needs
#     (PostgreSQL's entrypoint, below; nothing for Tangible)
#   - have a read-only root filesystem, unless --allow-writable names it
#   - set a process limit
#   - not mount the Docker socket
#   - map devices only if it is a burn worker, and then only an optical
#     drive's block or generic SCSI node
#
# Usage:
#   docker compose -f deploy/compose.yaml config --format json \
#     | deploy/check-hardening.sh [--allow-writable SERVICE]...

set -euo pipefail

allow_writable='[]'
while [ $# -gt 0 ]; do
    case "$1" in
        --allow-writable)
            allow_writable=$(jq -c --arg s "$2" '. + [$s]' <<<"$allow_writable")
            shift 2
            ;;
        *)
            echo "unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

rendered=$(cat)
problems=$(jq -r --argjson writable "$allow_writable" '
  def postgres_caps: ["CHOWN", "DAC_OVERRIDE", "FOWNER", "SETGID", "SETUID"];
  .services | to_entries[] | .key as $name | .value as $s |
  [
    (if $s.privileged == true then "is privileged" else empty end),
    (if ($s.network_mode // "") == "host" then "uses the host network" else empty end),
    (if ($s.pid // "") == "host" then "uses the host PID namespace" else empty end),
    (if ($s.ipc // "") == "host" then "uses the host IPC namespace" else empty end),
    (if (($s.security_opt // []) | index("no-new-privileges:true")) == null
       then "does not set no-new-privileges" else empty end),
    (if (($s.cap_drop // []) | index("ALL")) == null
       then "does not drop ALL capabilities" else empty end),
    (($s.cap_add // [])[] as $cap
       | if ($name == "postgres" and (postgres_caps | index($cap)) != null) then empty
         else "adds capability \($cap)" end),
    (if $s.read_only != true and ($writable | index($name)) == null
       then "has a writable root filesystem" else empty end),
    (if ($s.pids_limit // 0) <= 0 then "sets no process limit" else empty end),
    (($s.volumes // [])[] | select((.source // "") | test("docker\\.sock"))
       | "mounts the Docker socket"),
    (($s.devices // [])[] as $d
       | (if ($d | type) == "string" then $d else "\($d.source):\($d.target)" end) as $map
       | if ($name | startswith("burner-")) | not then "maps device \($map) but is not a burn worker"
         elif ($map | test(":/dev/s[rg][0-9]+(:|$)")) | not then "maps \($map), not an optical drive node"
         else empty end)
  ][] | "\($name): \(.)"
' <<<"$rendered")

if [ -n "$problems" ]; then
    echo "container hardening check failed:" >&2
    echo "$problems" | sed 's/^/  /' >&2
    exit 1
fi
jq -r '.services | keys | join(", ") | "hardened: \(.)"' <<<"$rendered"
