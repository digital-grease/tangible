// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen, within } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { forgetReportSettings } from '$lib/report';
import { axeViolations } from '$lib/testing/axe';
import type { ReleaseSearchView, ReleaseView, RequestView } from '$lib/requests';
import ReleaseSearch from './ReleaseSearch.svelte';

afterEach(() => {
  vi.unstubAllGlobals();
  forgetReportSettings();
});

type Reply = { status: number; body: unknown };
type Asked = { url: string; method: string; body?: string };

function server(routes: Record<string, Reply>): Asked[] {
  const asked: Asked[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const url = String(input);
      const method = init?.method ?? 'GET';
      asked.push({ url, method, ...(typeof init?.body === 'string' ? { body: init.body } : {}) });
      const reply = routes[`${method} ${url}`] ?? routes[url] ?? { status: 404, body: {} };
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

const SETTINGS = { status: 200, body: { enabled: false, version: '0.1.0', build: '0.1.0' } };

function request(extra: Partial<RequestView> = {}): RequestView {
  return {
    id: 'q1',
    requested_by: 'viewer',
    kind: 'movie',
    title: 'Big Buck Bunny',
    details: { year: '2008' },
    refs: [{ provider: 'imdb', external_id: 'tt1254207' }],
    state: 'approved',
    created_at: '2026-11-13T12:00:00Z',
    updated_at: '2026-11-13T12:00:00Z',
    can_approve: false,
    can_decline: true,
    can_cancel: true,
    can_search: true,
    ...extra,
  } as RequestView;
}

function release(extra: Partial<ReleaseView> = {}): ReleaseView {
  return {
    id: 'r1',
    title: 'Big Buck Bunny (2008) [Blu-ray ISO]',
    indexer: 'Fake Torznab',
    protocol: 'torrent',
    size_bytes: 23_622_320_128,
    seeders: 48,
    leechers: 4,
    published_at: '2026-11-10T12:00:00Z',
    categories: ['Movies'],
    score: 130,
    reasons: ['every word searched for is in its title', 'it names the year, 2008'],
    can_grab: true,
    ...extra,
  } as ReleaseView;
}

function found(extra: Partial<ReleaseSearchView> = {}): ReleaseSearchView {
  return {
    request_id: 'q1',
    searched_by: 'imdb_id',
    query: 'tt1254207',
    searched_at: '2026-11-13T12:00:00Z',
    held_until: '2026-11-13T13:00:00Z',
    results: [
      release(),
      release({
        id: 'r2',
        title: 'Big.Buck.Bunny.2008.DVD.ISO',
        indexer: 'Fake Newznab',
        protocol: 'usenet',
        seeders: null,
        leechers: null,
        grabs: 7,
        can_grab: false,
      }),
    ],
    indexers_enabled: 2,
    indexers_paused: [{ name: 'Fake Newznab', until: '2026-11-13T12:05:00Z' }],
    clients: [{ id: 'c1', name: 'qBittorrent', protocol: 'torrent' }],
    ...extra,
  } as ReleaseSearchView;
}

const GRABBED = {
  status: 200,
  body: {
    request: request({ state: 'downloading', can_search: false, import_id: 'i1' }),
    outcome: 'created',
    integration_id: 'c1',
    import_id: 'i1',
  },
};

describe('Finding a release', () => {
  it('shows the search held, best first, saying why each ranks there', async () => {
    server({
      'GET /api/v1/requests/q1/search': { status: 200, body: found() },
      '/api/v1/report-settings': SETTINGS,
    });
    const { container } = render(ReleaseSearch, {
      props: { request: request(), ongrabbed: () => {} },
    });
    const table = await screen.findByRole('table', { name: 'Releases found, best first' });
    const rows = within(table).getAllByRole('row');
    expect(
      within(rows[1] as HTMLElement).getByText('Big Buck Bunny (2008) [Blu-ray ISO]'),
    ).toBeTruthy();
    expect(within(rows[1] as HTMLElement).getByText('48 seeding, 4 leeching')).toBeTruthy();
    expect(within(rows[1] as HTMLElement).getByText('22.0 GiB')).toBeTruthy();
    expect(
      screen.getByRole('list', { name: 'Why Big Buck Bunny (2008) [Blu-ray ISO] ranks here' }),
    ).toBeTruthy();
    expect(within(rows[1] as HTMLElement).getByText('it names the year, 2008')).toBeTruthy();
    // A release no client takes cannot be grabbed, and says why.
    expect(within(rows[2] as HTMLElement).getByText('No Usenet client')).toBeTruthy();
    expect(within(rows[2] as HTMLElement).getByText('7 grabs')).toBeTruthy();
    expect(screen.getByText(/Searched by IMDb ID tt1254207/)).toBeTruthy();
    expect(screen.getByText(/Prowlarr has paused Fake Newznab after it failed/)).toBeTruthy();
    expect(screen.getByText(/searched for by its IMDb ID, then by its title/)).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);
  });

  it('searches the words given, and says when nothing was found', async () => {
    const asked = server({
      'POST /api/v1/requests/q1/search': {
        status: 200,
        body: found({
          searched_by: 'words',
          query: 'nothing at all',
          results: [],
          indexers_paused: [],
        }),
      },
      '/api/v1/report-settings': SETTINGS,
    });
    render(ReleaseSearch, { props: { request: request(), ongrabbed: () => {} } });
    await fireEvent.input(screen.getByRole('searchbox', { name: /Words to search for/ }), {
      target: { value: 'nothing at all' },
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Search' }));
    expect(await screen.findByText('Nothing found')).toBeTruthy();
    expect(screen.getByText('Found 0 releases.')).toBeTruthy();
    expect(asked.find((call) => call.method === 'POST')?.body).toBe('{"query":"nothing at all"}');
  });

  it('grabs only after asking, naming the client it goes to', async () => {
    const asked = server({
      'GET /api/v1/requests/q1/search': { status: 200, body: found() },
      'POST /api/v1/requests/q1/grab': GRABBED,
      '/api/v1/report-settings': SETTINGS,
    });
    const ongrabbed = vi.fn();
    render(ReleaseSearch, { props: { request: request(), ongrabbed } });
    await fireEvent.click(
      await screen.findByRole('button', { name: 'Grab: Big Buck Bunny (2008) [Blu-ray ISO]' }),
    );
    expect(asked.some((call) => call.url.endsWith('/grab'))).toBe(false);
    expect(screen.getByText(/handed to\s+qBittorrent/)).toBeTruthy();
    // One client takes torrents: none is chosen, and none is sent.
    expect(screen.queryByRole('combobox', { name: 'Download client' })).toBeNull();
    await fireEvent.click(screen.getByRole('button', { name: 'Yes, grab it' }));
    await vi.waitFor(() => expect(ongrabbed).toHaveBeenCalled());
    expect(asked.find((call) => call.url.endsWith('/grab'))?.body).toBe('{"result_id":"r1"}');
    expect(await screen.findByText(/Handed to qBittorrent\./)).toBeTruthy();
  });

  it('asks which client when more than one takes the release', async () => {
    const asked = server({
      'GET /api/v1/requests/q1/search': {
        status: 200,
        body: found({
          clients: [
            { id: 'c1', name: 'qBittorrent', protocol: 'torrent' },
            { id: 'c2', name: 'Seedbox', protocol: 'torrent' },
          ],
        }),
      },
      'POST /api/v1/requests/q1/grab': {
        ...GRABBED,
        body: { ...GRABBED.body, integration_id: 'c2' },
      },
      '/api/v1/report-settings': SETTINGS,
    });
    render(ReleaseSearch, { props: { request: request(), ongrabbed: () => {} } });
    await fireEvent.click(
      await screen.findByRole('button', { name: 'Grab: Big Buck Bunny (2008) [Blu-ray ISO]' }),
    );
    await fireEvent.change(screen.getByRole('combobox', { name: 'Download client' }), {
      target: { value: 'c2' },
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Yes, grab it' }));
    expect(await screen.findByText(/Handed to Seedbox\./)).toBeTruthy();
    expect(asked.find((call) => call.url.endsWith('/grab'))?.body).toBe(
      '{"result_id":"r1","integration_id":"c2"}',
    );
  });

  it('says to search again when the search held has expired', async () => {
    server({
      'GET /api/v1/requests/q1/search': { status: 200, body: found() },
      'POST /api/v1/requests/q1/grab': {
        status: 409,
        body: {
          type: 'about:blank',
          title: 'The search is no longer held',
          status: 409,
          code: 'SEARCH_EXPIRED',
          detail: 'that result is not among those held for this request',
        },
      },
      '/api/v1/report-settings': SETTINGS,
    });
    const ongrabbed = vi.fn();
    render(ReleaseSearch, { props: { request: request(), ongrabbed } });
    await fireEvent.click(
      await screen.findByRole('button', { name: 'Grab: Big Buck Bunny (2008) [Blu-ray ISO]' }),
    );
    await fireEvent.click(screen.getByRole('button', { name: 'Yes, grab it' }));
    expect(await screen.findByText(/That search is over an hour old/)).toBeTruthy();
    expect(ongrabbed).not.toHaveBeenCalled();
  });
});
