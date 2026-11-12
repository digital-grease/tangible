// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { render, screen, within } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { axeViolations } from '$lib/testing/axe';

const { default: HomePage } = await import('./+page.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
});

function server(dashboard: { status: number; body: unknown }) {
  const routes: Record<string, { status: number; body: unknown }> = {
    '/livez': { status: 200, body: { status: 'up', version: '0.1.0' } },
    '/readyz': { status: 200, body: { status: 'ready', checks: [] } },
    '/api/v1/dashboard': dashboard,
  };
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request) => {
      const reply = routes[String(input)] ?? { status: 404, body: {} };
      return {
        ok: reply.status >= 200 && reply.status < 300,
        status: reply.status,
        json: async () => reply.body,
        headers: new Headers(),
      } as Response;
    }),
  );
}

const burn = {
  burn_job_id: 'j1',
  disc_id: 'd1',
  title: 'Example Quest',
  disc_number: 2,
  state: 'needs_attention',
  error_code: 'VERIFY_MISMATCH',
  at: '2026-10-28T12:00:00Z',
};

describe('Overview page', () => {
  it('shows what needs somebody, the tiles, and the burns needing attention by title', async () => {
    server({
      status: 200,
      body: {
        imports: { active: 0, failed: 0 },
        burns: { queued: 0, waiting_for_media: 0, active: 0, needing_attention: 1 },
        workers: { online: 1, silent: 0, draining: 0, pending: 0, incompatible: 0 },
        integrity: { damaged: 0, unchecked: 0 },
        storage: { objects: 0, stored_bytes: 0, free_bytes: null, total_bytes: null },
        recent_burns: [{ ...burn, state: 'verification_failed' }],
        attention: [burn],
      },
    });
    const { container } = render(HomePage);
    expect(await screen.findByText('Needs somebody')).toBeTruthy();
    expect(screen.getByRole('heading', { level: 1, name: 'Overview' })).toBeTruthy();
    const glance = screen.getByRole('list', { name: 'At a glance' });
    expect(within(glance).getByRole('link', { name: 'Burns' }).getAttribute('href')).toBe('/burns');
    expect(within(glance).getByText('1 burn needing attention')).toBeTruthy();
    expect(within(glance).getByText('1 need attention')).toBeTruthy();

    const attention = screen.getByRole('table', { name: 'Burns needing attention' });
    expect(
      within(attention).getByRole('link', { name: 'Example Quest, disc 2' }).getAttribute('href'),
    ).toBe('/burns/j1');
    expect(within(attention).getByText('Needs attention')).toBeTruthy();
    expect(within(attention).getByText('VERIFY_MISMATCH')).toBeTruthy();

    const latest = screen.getByRole('table', { name: 'Latest burns' });
    expect(within(latest).getByText(/Verification failed/)).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);
  });

  it('says nothing needs you when nothing does', async () => {
    server({
      status: 200,
      body: {
        imports: { active: 0, failed: 0 },
        burns: { queued: 0, waiting_for_media: 0, active: 0, needing_attention: 0 },
        workers: { online: 1, silent: 0, draining: 0, pending: 0, incompatible: 0 },
        integrity: { damaged: 0, unchecked: 0 },
        storage: { objects: 0, stored_bytes: 0, free_bytes: null, total_bytes: null },
        recent_burns: [],
        attention: [],
      },
    });
    render(HomePage);
    expect(await screen.findByText('Nothing needs you')).toBeTruthy();
    expect(screen.getByText('Nothing needs attention.')).toBeTruthy();
    expect(screen.getByText('No burn has ended yet.')).toBeTruthy();
  });

  it('lists what the event stream says, in words, newest first', async () => {
    const listeners = new Map<string, ((event: MessageEvent) => void)[]>();
    class StandInSource {
      addEventListener(type: string, listener: (event: MessageEvent) => void) {
        listeners.set(type, [...(listeners.get(type) ?? []), listener]);
      }
      close() {}
    }
    vi.stubGlobal('EventSource', StandInSource);
    server({
      status: 200,
      body: {
        imports: { active: 0, failed: 0 },
        burns: { queued: 0, waiting_for_media: 0, active: 0, needing_attention: 1 },
        workers: { online: 1, silent: 0, draining: 0, pending: 0, incompatible: 0 },
        integrity: { damaged: 0, unchecked: 0 },
        storage: { objects: 0, stored_bytes: 0, free_bytes: null, total_bytes: null },
        recent_burns: [],
        attention: [burn],
      },
    });
    render(HomePage);
    expect(await screen.findByText('Nothing has happened since this page opened.')).toBeTruthy();
    const send = (type: string, data: unknown) => {
      for (const listener of listeners.get(type) ?? []) {
        listener({ data: JSON.stringify(data) } as MessageEvent);
      }
    };
    send('burn_job.changed', { topic: 'burns', id: 'j1', state: 'writing' });
    send('burn.progress', { topic: 'burns', burn_job_id: 'j1', progress: 0.5 });
    send('import.changed', { topic: 'imports', id: 'i1', state: 'complete' });
    const log = await screen.findByRole('list', { name: /Events since this page opened/ });
    const items = within(log).getAllByRole('listitem');
    // Progress is the drive bay's to show; the burn is named, as the
    // dashboard already lists it.
    expect(items).toHaveLength(2);
    expect(items[0]?.textContent).toContain('An import: Imported');
    expect(items[1]?.textContent).toContain('Example Quest, disc 2: Writing');
    expect(within(items[1]!).getByRole('link', { name: 'Open' }).getAttribute('href')).toBe(
      '/burns/j1',
    );
  });

  it('is the health check alone when signed out', async () => {
    server({
      status: 401,
      body: {
        type: 'about:blank',
        title: 'Not signed in',
        status: 401,
        code: 'UNAUTHENTICATED',
        detail: 'sign in first',
      },
    });
    render(HomePage);
    expect(await screen.findByText('Ready, version 0.1.0')).toBeTruthy();
    expect(screen.queryByText('Needs somebody')).toBeNull();
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('says so when the overview cannot be loaded', async () => {
    server({
      status: 503,
      body: {
        type: 'about:blank',
        title: 'Unavailable',
        status: 503,
        code: 'STORAGE_UNAVAILABLE',
        detail: 'the database is not answering',
      },
    });
    render(HomePage);
    const alert = await screen.findByRole('alert');
    expect(within(alert).getByText('the database is not answering')).toBeTruthy();
  });
});
