// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

/**
 * Short forms of identifiers and digests, for where a page names something
 * that has no name of its own (docs/plans/ui-overhaul.md, U9).
 *
 * A public identifier is a UUIDv7 (AGENTS.md 4.4). Its first 48 bits are
 * the time it was made, in milliseconds, so its first eight characters are
 * the same for everything made within about 65 seconds: two burns queued a
 * minute apart were both "Burn 01938000". Its last twelve characters are
 * random, so the short form is taken from there.
 */

/** A UUID's last eight characters: random bits, so two short forms rarely match. */
export function shortId(id: string): string {
  const hex = id.replace(/-/g, '');
  // Anything else (a test's "j1", a future kind of identifier) is shown as
  // it is when short, or by its start when long.
  if (!/^[0-9a-f]{32}$/i.test(hex)) return id.length > 12 ? id.slice(0, 8) : id;
  return hex.slice(-8).toLowerCase();
}

/**
 * A digest's first twelve characters, and an ellipsis to say there is more.
 * Every bit of a digest is as random as every other, so its start serves.
 */
export function shortDigest(digest: string): string {
  return digest.length > 12 ? `${digest.slice(0, 12)}…` : digest;
}
