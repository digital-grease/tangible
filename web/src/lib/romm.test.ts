// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import { describe, expect, it } from 'vitest';
import {
  inRommLabel,
  platformOptionLabel,
  rommApiInUse,
  rommStateLabel,
  saveEditionRomm,
  scanLabel,
  type EditionRomm,
  type RommLink,
  type RommScan,
} from './romm';

const romm = (overrides: Partial<EditionRomm>): EditionRomm => ({
  platform: 'psx',
  export: true,
  state: null,
  folder: null,
  detail: null,
  file_count: 0,
  exported_at: null,
  checked_at: null,
  scan_owed: false,
  last_scan: null,
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

  it('says what RomM made of an export, once it has looked', () => {
    const link = (state: string): RommLink => ({
      state,
      checked_at: '2026-10-27T05:00:00Z',
      rom_id: state === 'unseen' ? null : 41,
      url: null,
      detail: null,
    });
    expect(inRommLabel(romm({ state: 'current' }))).toBeNull();
    expect(inRommLabel(romm({ state: 'current', in_romm: link('unseen') }))).toBe(
      'RomM has not seen it yet',
    );
    expect(inRommLabel(romm({ state: 'current', in_romm: link('linked') }))).toBe('RomM has it');
    expect(inRommLabel(romm({ state: 'current', in_romm: link('differs') }))).toBe(
      'RomM sees different files',
    );
    expect(inRommLabel(romm({ state: 'current', in_romm: link('missing') }))).toBe(
      'RomM marks it missing',
    );
    expect(inRommLabel(romm({ state: 'removed', export: false, in_romm: link('linked') }))).toBe(
      'RomM still lists it',
    );
    expect(inRommLabel(romm({ state: 'removed', export: false, in_romm: link('missing') }))).toBe(
      'RomM marks it missing, as expected',
    );
  });

  it('says how a scan went in words', () => {
    const scan = (overrides: Partial<RommScan>): RommScan => ({
      platforms: ['psx'],
      state: 'finished',
      requested_at: '2026-10-27T05:00:00Z',
      ...overrides,
    });
    expect(scanLabel(scan({ scanned_roms: 12, new_roms: 1, updated_roms: 0 }))).toBe(
      'Scan finished (12 looked at, 1 new)',
    );
    expect(scanLabel(scan({}))).toBe('Scan finished');
    expect(scanLabel(scan({ state: 'refused', detail: 'RomM is busy' }))).toBe(
      'RomM would not scan: RomM is busy',
    );
    expect(scanLabel(scan({ state: 'timed_out' }))).toBe(
      'Scan had not finished when Tangible stopped waiting',
    );
    expect(scanLabel(scan({ state: 'lost' }))).toBe('RomM lost track of the scan');
  });

  it('marks a platform the connected RomM does not know', () => {
    expect(platformOptionLabel({ slug: 'psx', name: 'PlayStation', known_to_romm: true })).toBe(
      'PlayStation',
    );
    expect(platformOptionLabel({ slug: 'psx', name: 'PlayStation' })).toBe('PlayStation');
    expect(platformOptionLabel({ slug: 'x', name: 'X', known_to_romm: false })).toBe(
      'X (not a platform the connected RomM knows)',
    );
    expect(rommApiInUse(null)).toBe(false);
    expect(rommApiInUse({ configured: true, platforms: [], api: null })).toBe(false);
  });
});
