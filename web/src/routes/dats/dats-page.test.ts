// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { forgetReportSettings } from '$lib/report';
import { rememberSession } from '$lib/session';
import { axeViolations } from '$lib/testing/axe';

const { default: Page } = await import('./+page.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
  forgetReportSettings();
  rememberSession(null);
});

type Reply = { status: number; body: unknown };

/** The server, by `METHOD url`; records what was asked. */
function server(routes: Record<string, Reply | (() => Reply)>): string[] {
  const asked: string[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const key = `${init?.method ?? 'GET'} ${String(input)}`;
      asked.push(key);
      const route = routes[key];
      const reply = route ? (typeof route === 'function' ? route() : route) : null;
      const { status, body } = reply ?? { status: 404, body: {} };
      return {
        ok: status >= 200 && status < 300,
        status,
        json: async () => body,
        headers: new Headers(),
      } as Response;
    }),
  );
  return asked;
}

function dat(extra: Record<string, unknown> = {}) {
  return {
    id: 'd1',
    name: 'Sony - PlayStation',
    description: 'Sony - PlayStation - Datfile (10000)',
    version: '2026-10-01',
    author: 'redump.org',
    homepage: 'redump.org',
    source_sha256: '0'.repeat(64),
    game_count: 10000,
    rom_count: 30000,
    loaded_by: 'owner',
    loaded_at: '2026-10-28T12:00:00Z',
    matched_artifacts: 3,
    ...extra,
  };
}

function signIn(role: string) {
  rememberSession({ role } as Parameters<typeof rememberSession>[0]);
}

describe('DATs page', () => {
  it('loads a DAT an administrator chooses, and says what it identifies', async () => {
    signIn('administrator');
    let loaded = false;
    const asked = server({
      'GET /api/v1/dats': () => ({ status: 200, body: { items: loaded ? [dat()] : [] } }),
      'POST /api/v1/dats': () => {
        loaded = true;
        return {
          status: 201,
          body: {
            dat: dat(),
            replaced: false,
            unchanged: false,
            warnings: ['Game: odd.iso\'s SHA-1 "x" is not well formed, and was left out'],
            warning_count: 1,
          },
        };
      },
    });
    render(Page);
    expect(await screen.findByText(/No DATs loaded yet; load one above/)).toBeTruthy();
    const input = screen.getByLabelText('Logiqx XML DAT') as HTMLInputElement;
    const file = new File(['<datafile/>'], 'psx.dat', { type: 'text/xml' });
    await fireEvent.change(input, { target: { files: [file] } });
    await fireEvent.click(screen.getByRole('button', { name: 'Load' }));
    expect(
      await screen.findByText(
        'Sony - PlayStation was loaded: 10000 games; identifies 3 artifacts. 1 entry was left out.',
      ),
    ).toBeTruthy();
    expect(screen.getByText(/odd\.iso's SHA-1/)).toBeTruthy();
    expect(await screen.findByRole('rowheader', { name: /Sony - PlayStation/ })).toBeTruthy();
    expect(asked).toContain('POST /api/v1/dats');
  });

  it('says why a file that is not a DAT was refused', async () => {
    signIn('administrator');
    server({
      'GET /api/v1/dats': { status: 200, body: { items: [] } },
      'POST /api/v1/dats': {
        status: 422,
        body: {
          type: 'about:blank',
          title: 'Not a DAT this server reads',
          status: 422,
          code: 'DAT_INVALID',
          detail: 'the file is not a Logiqx DAT: its root element is not datafile',
        },
      },
    });
    render(Page);
    await screen.findByText(/No DATs loaded yet/);
    const input = screen.getByLabelText('Logiqx XML DAT') as HTMLInputElement;
    await fireEvent.change(input, {
      target: { files: [new File(['<html/>'], 'page.html')] },
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Load' }));
    expect(await screen.findByText(/its root element is not datafile/)).toBeTruthy();
  });

  it('removes a DAT only when asked twice, saying what goes with it', async () => {
    signIn('administrator');
    let removed = false;
    const asked = server({
      'GET /api/v1/dats': () => ({ status: 200, body: { items: removed ? [] : [dat()] } }),
      'DELETE /api/v1/dats/d1': () => {
        removed = true;
        return { status: 204, body: null };
      },
    });
    const { container } = render(Page);
    await fireEvent.click(await screen.findByRole('button', { name: 'Remove Sony - PlayStation' }));
    expect(screen.getByText(/the matches it made are deleted/)).toBeTruthy();
    // The keyboard is taken to the question.
    await vi.waitFor(() =>
      expect(document.activeElement).toBe(
        screen.getByRole('heading', { name: 'Remove Sony - PlayStation?' }),
      ),
    );
    expect(await axeViolations(container)).toEqual([]);
    expect(asked).not.toContain('DELETE /api/v1/dats/d1');
    await fireEvent.click(screen.getByRole('button', { name: 'Yes, remove Sony - PlayStation' }));
    expect(
      await screen.findByText('Removed Sony - PlayStation, and the matches it made.'),
    ).toBeTruthy();
    expect(asked).toContain('DELETE /api/v1/dats/d1');
  });

  it('shows an operator what is loaded, and nothing to change', async () => {
    signIn('operator');
    server({ 'GET /api/v1/dats': { status: 200, body: { items: [dat()] } } });
    render(Page);
    expect(await screen.findByText('10000 games; identifies 3 artifacts')).toBeTruthy();
    expect(screen.queryByLabelText('Logiqx XML DAT')).toBeNull();
    expect(screen.queryByRole('button')).toBeNull();
  });
});
