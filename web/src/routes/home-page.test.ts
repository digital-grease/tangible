// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

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

describe('Home page', () => {
  it('shows what needs somebody, and the burns needing attention by title', async () => {
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
    render(HomePage);
    expect(await screen.findByRole('heading', { name: 'Needs somebody' })).toBeTruthy();
    expect(screen.getByText('1 burn needing attention')).toBeTruthy();
    const links = screen.getAllByRole('link', { name: 'Example Quest, disc 2' });
    expect(links.map((link) => link.getAttribute('href'))).toContain('/burns/j1');
    expect(screen.getByText(/Needs attention \(VERIFY_MISMATCH\)/)).toBeTruthy();
    expect(screen.getByText(/Verification failed/)).toBeTruthy();
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
    expect(await screen.findByRole('heading', { name: /Ready/ })).toBeTruthy();
    expect(screen.queryByRole('heading', { name: 'Needs somebody' })).toBeNull();
    expect(screen.queryByRole('alert')).toBeNull();
  });
});
