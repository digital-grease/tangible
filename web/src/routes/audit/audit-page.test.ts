// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { axeViolations } from '$lib/testing/axe';

const { default: AuditPage } = await import('./+page.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
});

type Reply = { status: number; body: unknown };

function server(routes: Record<string, Reply>): string[] {
  const asked: string[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request) => {
      const url = String(input);
      asked.push(url);
      const reply = routes[url] ?? { status: 404, body: {} };
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

function event(n: number, action: string) {
  return {
    id: `e${n}`,
    occurred_at: '2026-10-28T12:00:00Z',
    actor_type: 'user',
    actor_id: 'alice',
    action,
    target_type: 'worker',
    target_id: `w${n}`,
    outcome: 'success',
    request_id: null,
    metadata: { from: 'online' },
  };
}

describe('Audit log page', () => {
  it('shows events in words and older ones when asked', async () => {
    const asked = server({
      '/api/v1/audit-events?limit=50': {
        status: 200,
        body: { items: [event(1, 'worker.revoked')], next_cursor: 'c1' },
      },
      '/api/v1/audit-events?limit=50&cursor=c1': {
        status: 200,
        body: { items: [event(2, 'worker.drained')], next_cursor: null },
      },
    });
    const { container } = render(AuditPage);
    expect(await screen.findByRole('rowheader', { name: 'Worker revoked' })).toBeTruthy();
    expect(screen.getByText('Done')).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);
    await fireEvent.click(screen.getByRole('button', { name: 'Show older events' }));
    expect(await screen.findByRole('rowheader', { name: 'Worker drained' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Show older events' })).toBeNull();
    expect(asked).toContain('/api/v1/audit-events?limit=50&cursor=c1');
  });

  it('filters by kind of action', async () => {
    const asked = server({
      '/api/v1/audit-events?limit=50': { status: 200, body: { items: [], next_cursor: null } },
      '/api/v1/audit-events?limit=50&action=worker.': {
        status: 200,
        body: { items: [event(1, 'worker.enabled')], next_cursor: null },
      },
    });
    render(AuditPage);
    expect(await screen.findByText('Nothing recorded matches.')).toBeTruthy();
    await fireEvent.change(screen.getByLabelText('What'), { target: { value: 'worker.' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Show' }));
    expect(await screen.findByRole('rowheader', { name: 'Worker enabled' })).toBeTruthy();
    expect(asked).toContain('/api/v1/audit-events?limit=50&action=worker.');
  });

  it('says who may read it', async () => {
    server({
      '/api/v1/audit-events?limit=50': {
        status: 403,
        body: {
          type: 'about:blank',
          title: 'Forbidden',
          status: 403,
          code: 'FORBIDDEN',
          detail: 'the operator role does not allow read_audit',
        },
      },
    });
    render(AuditPage);
    expect(await screen.findByText('Only an administrator can read the audit log.')).toBeTruthy();
  });
});
