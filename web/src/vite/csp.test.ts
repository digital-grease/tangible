// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
import { describe, expect, it } from 'vitest';
import config from '../../vite.config';

describe('the build, under the Content Security Policy', () => {
  it('never inlines an asset as a data: URI, which the policy would refuse', () => {
    // default-src 'self' allows only this origin; Vite inlines files under
    // 4 KiB unless told not to, and a refused font falls back silently.
    expect(config.build?.assetsInlineLimit).toBe(0);
  });
});
