// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import { describe, expect, it } from 'vitest';
import { rommStateLabel, saveEditionRomm, type EditionRomm } from './romm';

const romm = (overrides: Partial<EditionRomm>): EditionRomm => ({
  platform: 'psx',
  export: true,
  state: null,
  folder: null,
  detail: null,
  file_count: 0,
  exported_at: null,
  checked_at: null,
  ...overrides,
});

describe('RomM export status', () => {
  it('says where an export stands in words', () => {
    expect(rommStateLabel(romm({ export: false }))).toBe('Not in RomM');
    expect(rommStateLabel(romm({}))).toBe('Waiting for the exporter');
    expect(rommStateLabel(romm({ state: 'current' }))).toBe('In RomM');
    expect(rommStateLabel(romm({ state: 'current', export: false }))).toBe(
      'Being taken out of RomM',
    );
    expect(rommStateLabel(romm({ state: 'blocked' }))).toBe('Not exported');
    expect(rommStateLabel(romm({ state: 'failed' }))).toBe('Export failed');
    expect(rommStateLabel(romm({ state: 'removed', export: false }))).toBe('Not in RomM');
  });

  it('saves the platform and the switch together', async () => {
    const seen: { path: string; method: string; body: unknown }[] = [];
    const fetcher = (async (input: string | URL | Request, init?: RequestInit) => {
      seen.push({
        path: String(input),
        method: init?.method ?? 'GET',
        body: typeof init?.body === 'string' ? JSON.parse(init.body) : undefined,
      });
      return {
        ok: true,
        status: 200,
        headers: new Headers(),
        json: async () => romm({}),
      } as Response;
    }) as typeof fetch;
    await saveEditionRomm('e/1', 'ps2', true, fetcher);
    expect(seen[0]).toEqual({
      path: '/api/v1/editions/e%2F1/romm',
      method: 'PUT',
      body: { platform: 'ps2', export: true },
    });
  });
});
