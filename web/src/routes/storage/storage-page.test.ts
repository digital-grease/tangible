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
  vi.useRealTimers();
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

const PLANS = '/api/v1/storage/gc-plans?limit=20';

/** A dry-run plan every mark was read for, with a week's grace. */
const CERTAIN = {
  ...PLAN,
  grace_seconds: 7 * 86_400,
  manifests_unreadable: 0,
  certain: true,
  failed_import_staging: {
    areas: { count: 2, bytes: 12_288 },
    largest: [
      {
        import_id: 'i1',
        state: 'failed_terminal',
        error_code: 'IMPORT_DESCRIPTOR_INVALID',
        name: 'Hackers.zip',
        ended_at: '2026-11-14T12:00:00Z',
        bytes: 8192,
      },
    ],
  },
};

function keptPlan(state: string, plannedBy: string | null = 'alice') {
  return {
    id: 'p1',
    planned_at: '2026-11-17T12:00:00Z',
    planned_by: plannedBy,
    grace_seconds: 7 * 86_400,
    certain: true,
    candidates: { count: 2, bytes: 2048 },
    stale_partial_files: 0,
    state,
    expires_at: '2026-11-17T13:00:00Z',
    collect_requested_at: state === 'planned' ? null : '2026-11-17T12:01:00Z',
    collect_requested_by: state === 'planned' ? null : plannedBy,
    collected:
      state === 'collected'
        ? {
            collected_at: '2026-11-17T12:02:00Z',
            removed: { count: 1, bytes: 1024 },
            kept: { count: 1, bytes: 1024 },
            partial_files_removed: 0,
          }
        : null,
  };
}

function detail(state: string) {
  const collected = state === 'collected';
  return {
    plan: keptPlan(state),
    objects: [
      {
        sha256: 'ab'.repeat(32),
        size_bytes: 1024,
        modified_at: '2026-09-01T00:00:00Z',
        recorded: false,
        outcome: collected ? 'removed' : null,
        import: {
          import_id: 'i1',
          state: 'failed_terminal',
          error_code: 'IMPORT_DESCRIPTOR_INVALID',
          name: 'Hackers.zip',
          ended_at: '2026-11-14T12:00:00Z',
        },
      },
      {
        sha256: 'cd'.repeat(32),
        size_bytes: 1024,
        modified_at: '2026-09-01T00:00:00Z',
        recorded: true,
        outcome: collected ? 'kept_live' : null,
        import: null,
      },
    ],
  };
}

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
    expect(asked.some((key) => key.includes('/storage/gc-plan?'))).toBe(false);
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

  it('collects only after asking, listing every file and where it came from', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    let collected = false;
    const asked = server({
      [`GET ${CHECKS}`]: page([]),
      [`GET ${PLANS}`]: () => ({
        status: 200,
        body: { items: collected ? [keptPlan('collected')] : [] },
      }),
      'GET /api/v1/storage/gc-plan?grace_days=7&limit=100': { status: 200, body: CERTAIN },
      'POST /api/v1/storage/gc-plans': { status: 201, body: detail('planned') },
      'POST /api/v1/storage/gc-plans/p1/collect': () => ({
        status: 202,
        body: keptPlan('queued'),
      }),
      'GET /api/v1/storage/gc-plans/p1?limit=100': () => {
        collected = true;
        return { status: 200, body: detail('collected') };
      },
    });
    const { container } = render(Page);
    expect(await screen.findByText(/Nothing has been collected/)).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: 'Plan a collection' }));

    // What failed imports keep is counted, and never offered for removal.
    expect(await screen.findByText(/2 imports, 12\.0 KiB staged/)).toBeTruthy();
    expect(screen.getByText('Hackers.zip')).toBeTruthy();

    await fireEvent.click(screen.getByRole('button', { name: 'Collect 2 files (2.0 KiB)…' }));
    const question = await screen.findByRole('heading', { name: 'Collect 2 files (2.0 KiB)?' });
    expect(document.activeElement).toBe(question);
    expect(asked).toContain('POST /api/v1/storage/gc-plans');
    expect(asked).not.toContain('POST /api/v1/storage/gc-plans/p1/collect');
    const listed = screen.getByRole('table', { name: 'Every file that goes' });
    expect(
      within(listed).getByText('Hackers.zip (failed: IMPORT_DESCRIPTOR_INVALID)'),
    ).toBeTruthy();
    expect(within(listed).getByText('Not recorded by any import')).toBeTruthy();
    expect(screen.getByText(/written more than 7 days ago/)).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);

    // Keeping them asks nothing of the server.
    await fireEvent.click(screen.getByRole('button', { name: 'Keep them' }));
    expect(screen.queryByRole('heading', { name: 'Collect 2 files (2.0 KiB)?' })).toBeNull();

    await fireEvent.click(screen.getByRole('button', { name: 'Collect 2 files (2.0 KiB)…' }));
    await screen.findByRole('heading', { name: 'Collect 2 files (2.0 KiB)?' });
    await fireEvent.click(screen.getByRole('button', { name: 'Collect 2 files (2.0 KiB)' }));
    expect(await screen.findByText('Collection asked for. This page follows it.')).toBeTruthy();
    expect(screen.getByText('Waiting to be collected')).toBeTruthy();

    await vi.advanceTimersByTimeAsync(3000);
    expect(
      await screen.findByText('Collected: 1 file (1.0 KiB) removed, 1 file (1.0 KiB) kept.'),
    ).toBeTruthy();
    // The dry run no longer says what is there, so it goes.
    expect(screen.queryByText('Would be removed')).toBeNull();
    const outcomes = screen.getByRole('table', { name: 'What became of each file' });
    expect(within(outcomes).getByText('Removed')).toBeTruthy();
    expect(within(outcomes).getByText('Kept: something refers to it now')).toBeTruthy();
    const history = await screen.findByRole('table', { name: 'Collections, newest first' });
    expect(within(history).getByText('alice')).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);
  });

  it('offers no collection for an uncertain plan or one without a day of grace', async () => {
    server({
      [`GET ${CHECKS}`]: page([]),
      [`GET ${PLANS}`]: { status: 200, body: { items: [keptPlan('collected', null)] } },
      'GET /api/v1/storage/gc-plan?grace_days=0&limit=100': {
        status: 200,
        body: { ...CERTAIN, grace_seconds: 0 },
      },
      'GET /api/v1/storage/gc-plan?grace_days=7&limit=100': { status: 200, body: PLAN },
    });
    render(Page);
    const history = await screen.findByRole('table', { name: 'Collections, newest first' });
    expect(within(history).getByText('The schedule')).toBeTruthy();

    await fireEvent.click(screen.getByRole('button', { name: 'Plan a collection' }));
    expect(await screen.findByText('Uncertain.')).toBeTruthy();
    expect(screen.queryByRole('button', { name: /^Collect / })).toBeNull();

    await fireEvent.input(screen.getByLabelText('Grace period, in days'), {
      target: { value: '0' },
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Plan a collection' }));
    expect(await screen.findByText(/Collecting keeps a day's grace at least/)).toBeTruthy();
    expect(screen.queryByRole('button', { name: /^Collect / })).toBeNull();
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
