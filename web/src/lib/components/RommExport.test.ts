// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { EditionRomm, RommSettings } from '$lib/romm';

const { default: RommExport } = await import('./RommExport.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
});

/** A fake server answering the two reads the section makes. */
function server(settings: RommSettings, edition: EditionRomm) {
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request) => {
      const url = String(input);
      const body = url === '/api/v1/romm' ? settings : edition;
      return {
        ok: true,
        status: 200,
        json: async () => body,
        headers: new Headers(),
      } as Response;
    }),
  );
}

const api = {
  integration_id: 'i1',
  name: 'RomM',
  enabled: true,
  status: 'ok',
  scan_after_export: true,
  scans_owed: [],
};

const settings = (overrides: Partial<RommSettings> = {}): RommSettings => ({
  configured: true,
  platforms: [
    { slug: 'psx', name: 'PlayStation', known_to_romm: true },
    { slug: 'pcfx', name: 'PC-FX', known_to_romm: false },
  ],
  api,
  ...overrides,
});

const edition = (overrides: Partial<EditionRomm> = {}): EditionRomm => ({
  platform: 'psx',
  export: true,
  state: 'current',
  folder: 'psx/Game (USA)',
  detail: null,
  file_count: 2,
  exported_at: '2026-10-27T05:00:00Z',
  checked_at: '2026-10-27T05:00:00Z',
  scan_owed: false,
  last_scan: {
    platforms: ['psx'],
    state: 'finished',
    scanned_roms: 3,
    new_roms: 1,
    updated_roms: 0,
    requested_at: '2026-10-27T05:00:01Z',
  },
  in_romm: {
    state: 'linked',
    rom_id: 41,
    url: 'https://romm.example.net/rom/41',
    detail: null,
    checked_at: '2026-10-27T05:00:05Z',
  },
  ...overrides,
});

describe('the RomM section', () => {
  it('says RomM has the game, links to its page, and how the last scan went', async () => {
    server(settings(), edition());
    render(RommExport, { editionId: 'e1' });
    expect(await screen.findByText('RomM has it')).toBeTruthy();
    const link = screen.getByRole('link', { name: 'Open it in RomM' });
    expect(link.getAttribute('href')).toBe('https://romm.example.net/rom/41');
    expect(link.getAttribute('rel')).toBe('noopener noreferrer');
    expect(screen.getByText(/Last scan of psx: Scan finished \(3 looked at, 1 new\)/)).toBeTruthy();
    expect(screen.getByRole('option', { name: 'PlayStation' })).toBeTruthy();
    expect(
      screen.getByRole('option', { name: 'PC-FX (not a platform the connected RomM knows)' }),
    ).toBeTruthy();
    expect(screen.queryByText(/Waiting for RomM to scan/)).toBeNull();
  });

  it('says what differs, and that a scan is awaited, without a link to nowhere', async () => {
    server(
      settings(),
      edition({
        scan_owed: true,
        in_romm: {
          state: 'unseen',
          rom_id: null,
          url: null,
          detail: 'RomM has no game in psx/Game (USA) yet; a scan of its platform finds it',
          checked_at: '2026-10-27T05:00:05Z',
        },
      }),
    );
    render(RommExport, { editionId: 'e1' });
    expect(await screen.findByText('RomM has not seen it yet')).toBeTruthy();
    expect(screen.getByText(/a scan of its platform finds it/)).toBeTruthy();
    expect(screen.getByText("Waiting for RomM to scan its platform's folder.")).toBeTruthy();
    expect(screen.queryByRole('link', { name: 'Open it in RomM' })).toBeNull();
  });

  it('warns of a platform RomM does not know', async () => {
    server(settings(), edition({ platform: 'pcfx', folder: 'pcfx/Game (USA)' }));
    render(RommExport, { editionId: 'e1' });
    expect(await screen.findByText(/The connected RomM does not know this platform/)).toBeTruthy();
  });

  it('says nothing of RomM itself without its API', async () => {
    server(settings({ api: null }), edition());
    render(RommExport, { editionId: 'e1' });
    expect(await screen.findByText('In RomM')).toBeTruthy();
    expect(screen.queryByText('RomM has it')).toBeNull();
    expect(screen.queryByText(/Last scan/)).toBeNull();
  });
});
