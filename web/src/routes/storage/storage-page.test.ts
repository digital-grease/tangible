// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen, within } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { forgetReportSettings } from '$lib/report';
import { axeViolations } from '$lib/testing/axe';
import Page from './+page.svelte';

afterEach(() => {
  vi.unstubAllGlobals();
  forgetReportSettings();
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

const CHECKS = '/api/v1/integrity-checks?limit=20';

function check(extra: Record<string, unknown> = {}) {
  return {
    id: 'c1',
    scope: 'all',
    artifact_id: null,
    state: 'complete',
    objects_total: 10,
    bytes_total: 10240,
    objects_checked: 10,
    bytes_checked: 10240,
    problems: [],
    problem_count: 0,
    created_by: 'scheduler',
    created_at: '2026-10-20T12:00:00Z',
    completed_at: '2026-10-20T13:00:00Z',
    ...extra,
  };
}

function page(items: unknown[]): Reply {
  return { status: 200, body: { items, next_cursor: null } };
}

const PLAN = {
  planned_at: '2026-10-25T12:00:00Z',
  grace_seconds: 0,
  stored: { count: 3, bytes: 3072 },
  live: { count: 1, bytes: 1024 },
  recent: { count: 0, bytes: 0 },
  collectable: { count: 2, bytes: 2048 },
  candidates: [
    {
      sha256: 'ab'.repeat(32),
      size_bytes: 1024,
      modified_at: '2026-09-01T00:00:00Z',
      recorded: false,
    },
  ],
  partial_files: { count: 1, bytes: 4 },
  stale_partial_files: 1,
  unrecognized: { count: 0, bytes: 0 },
  missing: { count: 0, bytes: 0 },
  missing_digests: [],
  manifests_unreadable: 1,
  checkpoints_unreadable: 0,
  certain: false,
};

describe('Storage page', () => {
  it('lists checks with what each found, and offers a full scrub', async () => {
    const asked = server({
      [`GET ${CHECKS}`]: page([
        check({
          id: 'c2',
          scope: 'artifact',
          artifact_id: 'a1',
          created_by: 'alice',
          problem_count: 2,
          problems: [{ kind: 'missing', sha256: 'cd'.repeat(32), detail: 'is not in the library' }],
        }),
        check(),
      ]),
      'POST /api/v1/integrity-checks': { status: 202, body: check({ id: 'c3', state: 'queued' }) },
    });
    const { container } = render(Page);
    const table = await screen.findByRole('table', { name: 'Integrity checks, newest first' });
    expect(await axeViolations(container)).toEqual([]);
    expect(within(table).getByRole('link', { name: 'One artifact' }).getAttribute('href')).toBe(
      '/library/a1',
    );
    expect(within(table).getByText('Done: 2 problems')).toBeTruthy();
    expect(within(table).getByText('And 1 more, counted but not kept.')).toBeTruthy();
    expect(screen.getByText(/The last ended .*: Done: nothing wrong\./)).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: 'Check the whole library' }));
    expect(await screen.findByText('A full scrub is queued. This page follows it.')).toBeTruthy();
    expect(asked).toContain('POST /api/v1/integrity-checks');
  });

  it('shows a scrub under way and withdraws it', async () => {
    let withdrawn = false;
    const running = check({
      state: 'running',
      objects_checked: 5,
      bytes_checked: 5120,
      completed_at: null,
    });
    const asked = server({
      [`GET ${CHECKS}`]: () => page([withdrawn ? check({ state: 'canceled' }) : running]),
      'POST /api/v1/integrity-checks/c1/cancel': () => {
        withdrawn = true;
        return { status: 200, body: check({ state: 'canceled' }) };
      },
    });
    render(Page);
    const scrub = await screen.findByRole('region', { name: 'Full scrub' });
    expect(await within(scrub).findByText(/5 of 10 files/)).toBeTruthy();
    expect(within(scrub).getByRole('progressbar').getAttribute('aria-valuenow')).toBe('50');
    await fireEvent.click(screen.getByRole('button', { name: 'Withdraw the scrub' }));
    expect(await screen.findByText(/Withdrawn\. What it had read is kept/)).toBeTruthy();
    expect(asked).toContain('POST /api/v1/integrity-checks/c1/cancel');
    expect(await screen.findByRole('button', { name: 'Check the whole library' })).toBeTruthy();
  });

  it('plans a collection on request and says when it is uncertain', async () => {
    const asked = server({
      [`GET ${CHECKS}`]: page([]),
      'GET /api/v1/storage/gc-plan?grace_days=0&limit=100': { status: 200, body: PLAN },
    });
    const { container } = render(Page);
    expect(await screen.findByText('No integrity check has been asked for.')).toBeTruthy();
    // The plan walks the store, so it waits to be asked for.
    expect(asked.some((key) => key.includes('gc-plan'))).toBe(false);
    await fireEvent.input(screen.getByLabelText('Grace period, in days'), {
      target: { value: '0' },
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Plan a collection' }));
    expect(await screen.findByText('Uncertain.')).toBeTruthy();
    const table = screen.getByRole('table', {
      name: 'The 1 largest of 2 files collection would remove',
    });
    expect(within(table).getByText('ab'.repeat(32))).toBeTruthy();
    expect(within(table).getByText('Never catalogued')).toBeTruthy();
    expect(screen.getByText(/1 file \(4 B\) left by interrupted imports, 1 older/)).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);
  });

  it('tells anyone else it is for administrators', async () => {
    server({
      [`GET ${CHECKS}`]: {
        status: 403,
        body: {
          type: 'about:blank',
          title: 'Forbidden',
          status: 403,
          code: 'FORBIDDEN',
          detail: 'no',
        },
      },
    });
    render(Page);
    expect(await screen.findByText('Only an administrator can see storage.')).toBeTruthy();
  });
});
