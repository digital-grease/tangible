// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

const { default: WorkersPage } = await import('./+page.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
});

type Reply = { status: number; body: unknown };

/** Answer by method and path; record what was asked. */
function server(routes: Record<string, Reply>): string[] {
  const asked: string[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const key = `${init?.method ?? 'GET'} ${String(input)}`;
      asked.push(key);
      const reply = routes[key] ?? { status: 404, body: {} };
      return {
        ok: reply.status >= 200 && reply.status < 300,
        status: reply.status,
        json: async () => reply.body,
        headers: new Headers(),
      } as Response;
    }),
  );
  return asked;
}

function worker(status: string) {
  return {
    id: 'w1',
    name: 'bench',
    status,
    software_version: '0.1.0',
    protocol_version: '1alpha1',
    last_seen_at: '2026-10-28T12:00:00Z',
    created_at: '2026-10-01T12:00:00Z',
    revoked_at: null,
    drives: [{ id: 'd1', name: 'sr0' }],
  };
}

function page(status: string, extra: Record<string, Reply> = {}) {
  return server({
    'GET /api/v1/workers?limit=200': {
      status: 200,
      body: { items: [worker(status)], next_cursor: null },
    },
    'GET /api/v1/drives': { status: 200, body: { items: [] } },
    'GET /api/v1/erasures?limit=20': { status: 200, body: { items: [] } },
    ...extra,
  });
}

describe('Workers page', () => {
  it('lists workers in words and drains one when asked', async () => {
    const asked = page('online', {
      'POST /api/v1/workers/w1/drain': { status: 200, body: worker('draining') },
    });
    render(WorkersPage);
    expect(await screen.findByRole('rowheader', { name: 'bench' })).toBeTruthy();
    expect(screen.getByText('Taking work')).toBeTruthy();
    expect(screen.getByText('sr0')).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Enable bench' })).toBeNull();
    await fireEvent.click(screen.getByRole('button', { name: 'Drain bench' }));
    await vi.waitFor(() => expect(asked).toContain('POST /api/v1/workers/w1/drain'));
  });

  it('offers to enable a draining worker', async () => {
    page('draining');
    render(WorkersPage);
    expect(await screen.findByRole('button', { name: 'Enable bench' })).toBeTruthy();
    expect(screen.getByText(/taking nothing new/)).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Drain bench' })).toBeNull();
  });

  it('revokes only once it is confirmed, saying what it does', async () => {
    const asked = page('online', {
      'POST /api/v1/workers/w1/revoke': { status: 200, body: worker('revoked') },
    });
    render(WorkersPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Revoke bench…' }));
    expect(screen.getByText(/refused from its next request, for good/)).toBeTruthy();
    expect(asked).not.toContain('POST /api/v1/workers/w1/revoke');
    await fireEvent.click(screen.getByRole('button', { name: 'Keep bench' }));
    expect(screen.queryByText(/refused from its next request/)).toBeNull();

    await fireEvent.click(screen.getByRole('button', { name: 'Revoke bench…' }));
    await fireEvent.click(screen.getByRole('button', { name: 'Revoke bench' }));
    await vi.waitFor(() => expect(asked).toContain('POST /api/v1/workers/w1/revoke'));
  });

  it('says only an administrator can change workers', async () => {
    page('online', {
      'POST /api/v1/workers/w1/drain': {
        status: 403,
        body: {
          type: 'about:blank',
          title: 'Forbidden',
          status: 403,
          code: 'FORBIDDEN',
          detail: 'the operator role does not allow manage_workers',
        },
      },
    });
    render(WorkersPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Drain bench' }));
    expect(await screen.findByText('Only an administrator can change workers.')).toBeTruthy();
  });
});
