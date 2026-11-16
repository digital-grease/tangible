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
      '/api/v1/burn-attempts/t1/events?order=newest': { status: 200, body: { items: [] } },
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

  it('shows the newest events first, and earlier ones on request', async () => {
    const event = (sequence: number, code: string, fraction: number | null = null) => ({
      sequence,
      stage: 'writing',
      code,
      fraction,
      data: {},
      observed_at: at,
    });
    const asked = server({
      '/api/v1/burn-jobs/j1': { status: 200, body: { ...job(), attempts: [attempt] } },
      '/api/v1/burn-attempts/t1/events?order=newest': {
        status: 200,
        body: {
          items: [event(52, 'WRITE_PROGRESS', 0.43), event(51, 'WRITE_PROGRESS', 0.42)],
          next_after: null,
          next_before: 51,
        },
      },
      '/api/v1/burn-attempts/t1/events?order=newest&before=51': {
        status: 200,
        body: {
          items: [event(2, 'STAGING_COMPLETE'), event(1, 'STAGING_STARTED')],
          next_before: null,
        },
      },
      '/api/v1/burn-attempts/t1': { status: 200, body: { ...attempt, medium: null } },
      '/api/v1/report-settings': SETTINGS,
    });
    render(BurnPage);
    const panel = (await screen.findByRole('heading', { name: 'Latest attempt timeline' })).closest(
      'section',
    )!;
    const codes = () =>
      within(panel)
        .getAllByRole('listitem')
        .map((item) => item.querySelector('code')?.textContent);
    await vi.waitFor(() => expect(codes()).toEqual(['WRITE_PROGRESS', 'WRITE_PROGRESS']));
    expect(within(panel).getByText('43%', { exact: false })).toBeTruthy();
    await fireEvent.click(within(panel).getByRole('button', { name: 'Show earlier events' }));
    await vi.waitFor(() =>
      expect(codes()).toEqual([
        'WRITE_PROGRESS',
        'WRITE_PROGRESS',
        'STAGING_COMPLETE',
        'STAGING_STARTED',
      ]),
    );
    expect(asked).toContain('/api/v1/burn-attempts/t1/events?order=newest&before=51');
    // Nothing earlier than the first event.
    expect(within(panel).queryByRole('button', { name: 'Show earlier events' })).toBeNull();
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
      '/api/v1/burn-attempts/t1/events?order=newest': { status: 200, body: { items: [] } },
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

/** A catalog disc, as the picker is answered. */
function place(extra: Record<string, unknown> = {}) {
  return {
    disc: {
      id: 'd1',
      disc_set_id: 's1',
      sequence_number: 1,
      display_name: null,
      media_family: 'dvd',
      compatibility_claim: 'unknown',
      artifact_count: 0,
      copy_count: 0,
      created_at: at,
      version: 1,
    },
    title_id: 't1',
    title: 'Hackers',
    title_kind: 'movie',
    release_year: 1995,
    edition_id: 'e1',
    edition: 'NTSC DVD',
    disc_set: 'Single disc',
    disc_count_expected: 1,
    linked: false,
    ...extra,
  };
}

const HACKERS = {
  id: 'a1',
  source_filename: 'hackers.img',
  format: 'iso',
  total_bytes: 4_381_129_111,
  validation_state: 'valid',
  request: { id: 'r1', title: 'Hackers', kind: 'movie', year: '1995' },
  disc_shape: { media_family: null, sector_count: 2_139_218, tracks: [] },
};

const QUEUED = {
  status: 201,
  body: { ...job({ state: 'queued', progress: null }), warnings: [] },
};

/** What the form posted to queue the burn. */
function posted(sent: ReturnType<typeof vi.fn>, url: string): unknown {
  const call = sent.mock.calls.find(
    ([input, init]) =>
      String(input) === url && (init as RequestInit | undefined)?.method === 'POST',
  );
  return call ? JSON.parse(String((call[1] as RequestInit).body)) : undefined;
}

describe('Create burn job', () => {
  it('shows the artifact and the disc it was opened with, and queues with the policy chosen', async () => {
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
      '/api/v1/discs?artifact=a1&id=d1': {
        status: 200,
        body: {
          items: [place({ title: 'Debian', release_year: 2025, edition: 'netinst' })],
          more: false,
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
    expect(await screen.findByText('Debian (2025) · netinst · Disc 1')).toBeTruthy();
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

  it('chooses the one disc already linked to the artifact', async () => {
    where.search = '?artifact=a1';
    server({
      '/api/v1/artifacts/a1': { status: 200, body: HACKERS },
      '/api/v1/discs?artifact=a1&limit=5': {
        status: 200,
        body: { items: [place({ linked: true })], more: false },
      },
      'POST /api/v1/burn-jobs': QUEUED,
    });
    render(NewBurnPage);
    expect(await screen.findByText('Hackers (1995) · NTSC DVD · Disc 1')).toBeTruthy();
    expect(screen.getByText('Linked to this artifact')).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Choose another disc' })).toBeTruthy();
  });

  it('finds the disc by what the artifact is, and queues a burn of it', async () => {
    where.search = '?artifact=a1';
    const two = place({
      disc: { ...place().disc, id: 'd2', sequence_number: 2 },
      disc_count_expected: 2,
      disc_set: 'Two-disc set',
    });
    server({
      '/api/v1/artifacts/a1': { status: 200, body: HACKERS },
      '/api/v1/discs?artifact=a1&limit=5': { status: 200, body: { items: [], more: false } },
      '/api/v1/discs?search=Hackers&artifact=a1&limit=20': {
        status: 200,
        body: { items: [place(), two], more: true },
      },
      'POST /api/v1/burn-jobs': QUEUED,
    });
    const sent = vi.mocked(fetch);
    const { container } = render(NewBurnPage);
    const list = await screen.findByRole('list', { name: 'Discs found' });
    // Searched by the request's title, not the file's name.
    expect(
      (screen.getByRole('searchbox', { name: /Find it in the catalog/ }) as HTMLInputElement).value,
    ).toBe('Hackers');
    expect(within(list).getAllByRole('listitem').length).toBe(2);
    expect(
      within(list).getByText('Hackers (1995) · NTSC DVD · Two-disc set · Disc 2'),
    ).toBeTruthy();
    expect(screen.getByText(/More discs match/)).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);
    // Nothing to queue until a disc is chosen.
    const queue = screen.getByRole('button', { name: 'Queue this burn' }) as HTMLButtonElement;
    expect(queue.disabled).toBe(true);
    await fireEvent.click(
      screen.getByRole('button', { name: 'Use Hackers (1995) · NTSC DVD · Two-disc set · Disc 2' }),
    );
    expect(queue.disabled).toBe(false);
    await fireEvent.click(queue);
    expect(await screen.findByRole('heading', { name: 'Queued' })).toBeTruthy();
    expect(posted(sent, '/api/v1/burn-jobs')).toMatchObject({ artifact_id: 'a1', disc_id: 'd2' });
  });

  it('searches on Enter without queueing anything', async () => {
    where.search = '?artifact=a1';
    const asked = server({
      '/api/v1/artifacts/a1': { status: 200, body: HACKERS },
      '/api/v1/discs?artifact=a1&limit=5': { status: 200, body: { items: [], more: false } },
      '/api/v1/discs?search=Hackers&artifact=a1&limit=20': {
        status: 200,
        body: { items: [], more: false },
      },
      '/api/v1/discs?search=Hackers+1995&artifact=a1&limit=20': {
        status: 200,
        body: { items: [place()], more: false },
      },
    });
    render(NewBurnPage);
    expect(await screen.findByText('No disc in the catalog matches.')).toBeTruthy();
    const box = screen.getByRole('searchbox', { name: /Find it in the catalog/ });
    await fireEvent.input(box, { target: { value: 'Hackers 1995' } });
    await fireEvent.keyDown(box, { key: 'Enter' });
    expect(await screen.findByRole('list', { name: 'Discs found' })).toBeTruthy();
    expect(asked.some((key) => key.startsWith('POST'))).toBe(false);
  });

  it('catalogues a new disc from the request, linked to the artifact, and uses it', async () => {
    where.search = '?artifact=a1';
    server({
      '/api/v1/artifacts/a1': { status: 200, body: HACKERS },
      '/api/v1/discs?artifact=a1&limit=5': { status: 200, body: { items: [], more: false } },
      '/api/v1/discs?search=Hackers&artifact=a1&limit=20': {
        status: 200,
        body: { items: [], more: false },
      },
      'POST /api/v1/discs': { status: 201, body: place({ edition: 'DVD', linked: true }) },
      'POST /api/v1/burn-jobs': QUEUED,
    });
    const sent = vi.mocked(fetch);
    const { container } = render(NewBurnPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Catalogue a new disc…' }));
    // Started from the request and the artifact's size: a film of 1995 on a DVD.
    expect((screen.getByLabelText('Title') as HTMLInputElement).value).toBe('Hackers');
    expect((screen.getByLabelText('Kind of work') as HTMLSelectElement).value).toBe('movie');
    expect((screen.getByLabelText('Year') as HTMLInputElement).value).toBe('1995');
    expect((screen.getByLabelText('Medium') as HTMLSelectElement).value).toBe('dvd');
    expect((screen.getByLabelText(/^Edition/) as HTMLInputElement).value).toBe('DVD');
    expect(await axeViolations(container)).toEqual([]);
    await fireEvent.click(screen.getByRole('button', { name: 'Catalogue and use this disc' }));
    expect(await screen.findByText('Hackers (1995) · DVD · Disc 1')).toBeTruthy();
    expect(posted(sent, '/api/v1/discs')).toEqual({
      title: 'Hackers',
      kind: 'movie',
      release_year: 1995,
      edition: 'DVD',
      media_family: 'dvd',
      artifact_id: 'a1',
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Queue this burn' }));
    expect(await screen.findByRole('heading', { name: 'Queued' })).toBeTruthy();
    expect(posted(sent, '/api/v1/burn-jobs')).toMatchObject({ artifact_id: 'a1', disc_id: 'd1' });
  });

  it('opened from the catalog with a disc, offers the artifacts linked to it', async () => {
    where.search = '?disc=d1';
    server({
      '/api/v1/discs?id=d1': { status: 200, body: { items: [place()], more: false } },
      '/api/v1/discs/d1/artifacts': {
        status: 200,
        body: [{ artifact_id: 'a1', relationship: 'representation_of', confidence: 1 }],
      },
      '/api/v1/artifacts/a1': { status: 200, body: HACKERS },
      '/api/v1/discs?artifact=a1&id=d1': {
        status: 200,
        body: { items: [place({ linked: true })], more: false },
      },
    });
    render(NewBurnPage);
    expect(await screen.findByText('Hackers (1995) · NTSC DVD · Disc 1')).toBeTruthy();
    const offered = await screen.findByRole('group', { name: 'Artifacts linked to this disc' });
    await fireEvent.click(within(offered).getByRole('button', { name: 'Use Hackers' }));
    expect((screen.getByLabelText('Artifact identifier') as HTMLInputElement).value).toBe('a1');
    expect(await screen.findByText('Linked to this artifact')).toBeTruthy();
    expect(screen.queryByRole('group', { name: 'Artifacts linked to this disc' })).toBeNull();
  });
});
