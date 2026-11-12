// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { rememberSession } from '$lib/session';
import { axeViolations } from '$lib/testing/axe';

const { default: Identification } = await import('./Identification.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
  rememberSession(null);
});

type Asked = { method: string; url: string };

function identification(extra: Record<string, unknown> = {}) {
  return {
    state: 'done',
    error: null,
    hashed_at: '2026-10-28T12:00:00Z',
    components: [
      {
        logical_path: 'disc.bin',
        length_bytes: 3000,
        crc32: '414fa339',
        md5: '9e107d9d372bb6826bd81d3542a419d6',
        sha1: '2fd4e1c67a2d28fced849ee1bb76e7391b93eb12',
      },
    ],
    tracks: [],
    matches: [],
    ...extra,
  };
}

const PARTIAL = {
  dat_id: 'd1',
  dat_name: 'Sony - PlayStation',
  dat_version: '2026-10-01',
  game_name: 'Game (USA)',
  description: null,
  category: 'Games',
  kind: 'partial',
  files_matched: 1,
  files_total: 2,
  files: [
    { name: 'Game (USA) (Track 1).bin', size: 1000, matched_by: 'track 1' },
    { name: 'Game (USA) (Track 2).bin', size: 2000, matched_by: null },
  ],
};

/** A fake server: the identification, and a rehash answering `after`. */
function server(view: unknown, after: unknown = view): Asked[] {
  const asked: Asked[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const method = init?.method ?? 'GET';
      asked.push({ method, url: String(input) });
      const body = method === 'POST' ? after : view;
      return {
        ok: true,
        status: method === 'POST' ? 202 : 200,
        json: async () => body,
        headers: new Headers(),
      } as Response;
    }),
  );
  return asked;
}

describe('the Identification section', () => {
  it('names the game a partial match is, and which track did not match', async () => {
    server(identification({ matches: [PARTIAL] }));
    const { container } = render(Identification, { artifactId: 'a1' });
    expect(await screen.findByText('Partly matches Game (USA): 1 of 2 files')).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);
    expect(screen.getByText(/In Sony - PlayStation \(2026-10-01\), Games/)).toBeTruthy();
    const files = screen.getAllByRole('listitem').map((item) => item.textContent?.trim() ?? '');
    // Each with its mark, drawn rather than written: the words say it.
    expect(files).toContain('Game (USA) (Track 1).bin (1000 B): matched by track 1');
    expect(files).toContain('Game (USA) (Track 2).bin (2.0 KiB): not matched');
    expect(screen.getByText(/offset correction/)).toBeTruthy();
  });

  it('says when nothing matched, and where DATs are loaded', async () => {
    server(identification());
    render(Identification, { artifactId: 'a1' });
    expect(await screen.findByText(/Matches no game in the loaded DATs/)).toBeTruthy();
    expect(screen.getByRole('link', { name: 'DATs' }).getAttribute('href')).toBe('/dats');
    expect(screen.getByText('Preservation hashes', { selector: 'summary' })).toBeTruthy();
    expect(screen.getByText('2fd4e1c67a2d28fced849ee1bb76e7391b93eb12')).toBeTruthy();
  });

  it('lets an operator hash a failed artifact again', async () => {
    rememberSession({ role: 'operator' } as Parameters<typeof rememberSession>[0]);
    const asked = server(
      identification({ state: 'failed', error: 'disc.bin is missing', hashed_at: null }),
      identification({ state: 'pending', hashed_at: null, components: [] }),
    );
    render(Identification, { artifactId: 'a1' });
    expect(await screen.findByText(/Could not be hashed: disc.bin is missing/)).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: 'Hash again' }));
    expect(await screen.findByText(/being computed in the background/)).toBeTruthy();
    expect(
      asked.some(
        (a) => a.method === 'POST' && a.url === '/api/v1/artifacts/a1/identification/rehash',
      ),
    ).toBe(true);
    expect(screen.queryByRole('button', { name: 'Hash again' })).toBeNull();
  });

  it('offers a viewer nothing to press', async () => {
    rememberSession({ role: 'viewer' } as Parameters<typeof rememberSession>[0]);
    server(identification({ matches: [{ ...PARTIAL, kind: 'exact', files_matched: 2 }] }));
    render(Identification, { artifactId: 'a1' });
    expect(await screen.findByText('Matches Game (USA), all 2 of its files')).toBeTruthy();
    expect(screen.queryByRole('button')).toBeNull();
  });
});
