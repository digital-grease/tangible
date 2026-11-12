// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import { describe, expect, it } from 'vitest';
import { shortDigest, shortId } from './ids';

describe('short identifiers', () => {
  it('takes a UUIDv7 by its random tail, not its timestamp', () => {
    // Made in the same millisecond: their first eight characters agree.
    const first = '0193a5b2-7c10-7d4e-8f21-3a9b4c5d6e7f';
    const second = '0193a5b2-7c10-7a11-9b02-cc0de1f2a3b4';
    expect(first.slice(0, 8)).toBe(second.slice(0, 8));
    expect(shortId(first)).toBe('4c5d6e7f');
    expect(shortId(second)).toBe('e1f2a3b4');
  });

  it('says it in lower case, whatever case it came in', () => {
    expect(shortId('0193A5B2-7C10-7D4E-8F21-3A9B4C5D6E7F')).toBe('4c5d6e7f');
  });

  it('leaves anything that is not a UUID as it is, or by its start when long', () => {
    expect(shortId('j1')).toBe('j1');
    expect(shortId('not-a-uuid-but-long')).toBe('not-a-uu');
  });
});

describe('short digests', () => {
  it('shows a digest by its start, saying there is more', () => {
    expect(shortDigest('ab'.repeat(32))).toBe('abababababab…');
    expect(shortDigest('abc')).toBe('abc');
  });
});
