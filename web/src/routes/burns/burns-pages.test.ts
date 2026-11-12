// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen, within } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { forgetReportSettings } from '$lib/report';
import { axeViolations } from '$lib/testing/axe';

const where = vi.hoisted(() => ({ search: '' }));
vi.mock('$app/state', () => ({
  page: {
    params: { id: 'j1' },
    get url() {
      return new URL(`http://localhost/burns/new${where.search}`);
    },
  },
}));

const { default: QueuePage } = await import('./+page.svelte');
const { default: BurnPage } = await import('./[id]/+page.svelte');
const { default: NewBurnPage } = await import('./new/+page.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
  forgetReportSettings();
  where.search = '';
});

type Reply = { status: number; body: unknown };
function server(routes: Record<string, Reply>): string[] {
  const asked: string[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const url = String(input);
      const key = init?.method && init.method !== 'GET' ? `${init.method} ${url}` : url;
      asked.push(key);
      const reply = routes[key] ?? { status: 404, body: { code: 'NOT_FOUND', detail: key } };
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

const at = '2026-11-11T12:00:00Z';
function job(extra: Record<string, unknown> = {}) {
  return {
    id: 'j1',
    disc_id: 'd1',
    artifact_id: 'a1',
    state: 'writing',
    has_started_writing: true,
    priority: 0,
    verification_policy: ['full_sector_readback'],
    eject_policy: 'eject_on_success',
    requested_media_profile: null,
    created_by: 'admin',
    created_at: at,
    updated_at: at,
    completed_at: null,
    attempt_count: 1,
    progress: { stage: 'writing', fraction: 0.43 },
    ...extra,
  };
}
const attempt = {
  id: 't1',
  burn_job_id: 'j1',
  attempt_number: 1,
  state: 'writing',
  consumed_media: true,
  physical_copy_id: null,
  worker_id: 'w1',
  drive_id: 'd1',
  engine: 'xorriso',
  engine_version: '1.5.6',
  started_at: at,
  error_code: null,
  error_detail: null,
};
const SETTINGS = { status: 200, body: { enabled: false, version: '0.1.0', build: '0.1.0' } };

describe('Burn queue', () => {
  it('asks for a disc first, then lists every burn with its state and stage', async () => {
    server({
      '/api/v1/burn-jobs': {
        status: 200,
        body: {
          items: [
            job(),
            job({
              id: 'j2',
              state: 'waiting_for_media',
              progress: null,
              requested_media_profile: 'DVD+R',
            }),
          ],
          next_cursor: null,
        },
      },
    });
    const { container } = render(QueuePage);
    expect(await screen.findByText('A burn is waiting for a disc')).toBeTruthy();
    expect(screen.getByRole('link', { name: /Insert a disc for burn j2/ })).toBeTruthy();
    const table = screen.getByRole('table', { name: 'Burn jobs, newest first' });
    expect(within(table).getByText('Writing')).toBeTruthy();
    expect(within(table).getByRole('progressbar').getAttribute('aria-valuenow')).toBe('43');
    expect(screen.getByRole('link', { name: 'Create burn job' }).getAttribute('href')).toBe(
      '/burns/new',
    );
    expect(await axeViolations(container)).toEqual([]);
  });

  it('tells apart two burns queued in the same minute', async () => {
    // UUIDv7s made a second apart: their first eight characters are the
    // same timestamp bits, which once named both "Burn 0193a5b2".
    const first = '0193a5b2-7c10-7d4e-8f21-3a9b4c5d6e7f';
    const second = '0193a5b2-7fe0-7a11-9b02-cc0de1f2a3b4';
    server({
      '/api/v1/burn-jobs': {
        status: 200,
        body: { items: [job({ id: first }), job({ id: second })], next_cursor: null },
      },
    });
    render(QueuePage);
    const table = await screen.findByRole('table', { name: 'Burn jobs, newest first' });
    expect(within(table).getByText('4c5d6e7f').getAttribute('title')).toBe(first);
    expect(within(table).getByText('e1f2a3b4').getAttribute('title')).toBe(second);
  });

  it('says how to start when there are none', async () => {
    server({ '/api/v1/burn-jobs': { status: 200, body: { items: [], next_cursor: null } } });
    render(QueuePage);
    expect(await screen.findByRole('heading', { name: 'No burn jobs' })).toBeTruthy();
  });
});

describe('Burn page', () => {
  it('shows a burn writing: its state, progress, drawing, and that it cannot be cancelled', async () => {
    server({
      '/api/v1/burn-jobs/j1': { status: 200, body: { ...job(), attempts: [attempt] } },
      '/api/v1/burn-attempts/t1/events': { status: 200, body: { items: [] } },
      '/api/v1/burn-attempts/t1': { status: 200, body: { ...attempt, medium: null } },
      '/api/v1/artifacts/a1': {
        status: 200,
        body: { id: 'a1', disc_shape: { media_family: 'cd', sector_count: 300_000, tracks: [] } },
      },
      '/api/v1/report-settings': SETTINGS,
    });
    const { container } = render(BurnPage);
    expect(await screen.findByRole('heading', { level: 1, name: 'Burn j1' })).toBeTruthy();
    const status = screen.getByRole('heading', { name: 'Writing' }).closest('section')!;
    expect(within(status).getByRole('progressbar').getAttribute('aria-valuenow')).toBe('43');
    await vi.waitFor(() => expect(status.querySelector('circle.ring.written')).not.toBeNull());
    expect(
      screen.getByText(/This burn cannot be cancelled: a disc is being consumed/),
    ).toBeTruthy();
    expect(screen.queryByRole('button', { name: /Cancel this burn/ })).toBeNull();
    expect(screen.getByText('Target compatibility unknown')).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);
  });

  it('asks for a disc, drawn as an empty tray, and offers to cancel before writing', async () => {
    const asked = server({
      '/api/v1/burn-jobs/j1': {
        status: 200,
        body: {
          ...job({ state: 'waiting_for_media', has_started_writing: false, progress: null }),
          attempts: [{ ...attempt, state: 'preflighting', consumed_media: false }],
        },
      },
      '/api/v1/burn-attempts/t1/events': { status: 200, body: { items: [] } },
      '/api/v1/burn-attempts/t1': { status: 200, body: { ...attempt, medium: null } },
      'POST /api/v1/burn-jobs/j1/cancel': {
        status: 200,
        body: {
          ...job({ state: 'canceled', has_started_writing: false, progress: null }),
          attempts: [],
        },
      },
      '/api/v1/report-settings': SETTINGS,
    });
    const { container } = render(BurnPage);
    expect(await screen.findByText('Insert a disc.')).toBeTruthy();
    expect(container.querySelectorAll('circle.empty')).toHaveLength(2);
    await fireEvent.click(screen.getByRole('button', { name: /Cancel this burn/ }));
    await vi.waitFor(() => expect(asked).toContain('POST /api/v1/burn-jobs/j1/cancel'));
    expect(await screen.findByRole('heading', { name: 'Cancelled' })).toBeTruthy();
  });
});

describe('Create burn job', () => {
  it('shows the artifact it will write, and queues with the policy chosen', async () => {
    where.search = '?artifact=a1&disc=d1';
    const asked = server({
      '/api/v1/artifacts/a1': {
        status: 200,
        body: {
          id: 'a1',
          source_filename: 'debian.iso',
          format: 'iso',
          total_bytes: 754_000_000,
          validation_state: 'valid',
          disc_shape: { media_family: 'cd', sector_count: 368_000, tracks: [] },
        },
      },
      'POST /api/v1/burn-jobs': {
        status: 201,
        body: {
          ...job({ state: 'queued', progress: null }),
          warnings: ['Target compatibility unknown.'],
        },
      },
    });
    const { container } = render(NewBurnPage);
    expect(await screen.findByText('debian.iso')).toBeTruthy();
    expect((screen.getByLabelText('Disc identifier') as HTMLInputElement).value).toBe('d1');
    expect(container.querySelector('svg.disc')).not.toBeNull();
    expect(await axeViolations(container)).toEqual([]);
    await fireEvent.change(screen.getByLabelText('Verification'), {
      target: { value: 'filesystem_compare' },
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Queue this burn' }));
    expect(await screen.findByRole('heading', { name: 'Queued' })).toBeTruthy();
    expect(asked).toContain('POST /api/v1/burn-jobs');
    expect(screen.getByRole('link', { name: 'Follow this burn' }).getAttribute('href')).toBe(
      '/burns/j1',
    );
  });
});
