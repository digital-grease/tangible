// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { forgetReportSettings } from '$lib/report';
import { axeViolations } from '$lib/testing/axe';

const goto = vi.fn();
vi.mock('$app/navigation', () => ({ goto: (...args: unknown[]) => goto(...args) }));
vi.mock('$app/state', () => ({ page: { params: { id: 'q1' } } }));

const { default: RequestsPage } = await import('./+page.svelte');
const { default: NewRequestPage } = await import('./new/+page.svelte');
const { default: RequestPage } = await import('./[id]/+page.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
  forgetReportSettings();
  goto.mockReset();
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

function request(extra: Record<string, unknown> = {}) {
  return {
    id: 'q1',
    requested_by: 'viewer',
    kind: 'game',
    title: 'Some Game',
    details: { platform: 'Sony PlayStation', region: 'USA' },
    refs: [],
    note: 'my copy is scratched',
    state: 'pending_approval',
    created_at: '2026-11-13T12:00:00Z',
    updated_at: '2026-11-13T12:00:00Z',
    can_approve: false,
    can_decline: false,
    can_cancel: true,
    ...extra,
  };
}

const DECIDER = { can_approve: true, can_decline: true, can_cancel: true };

function problem(status: number, code: string, detail: string) {
  return { status, body: { type: 'about:blank', title: code, status, code, detail } };
}

describe('Requests page', () => {
  it('lists requests and narrows them by state', async () => {
    const asked = server({
      '/api/v1/requests?limit=50': { status: 200, body: { items: [request()] } },
      '/api/v1/requests?limit=50&state=declined': { status: 200, body: { items: [] } },
      '/api/v1/report-settings': SETTINGS,
    });
    const { container } = render(RequestsPage);
    const link = await screen.findByRole('link', { name: 'Some Game' });
    expect(link.getAttribute('href')).toBe('/requests/q1');
    expect(screen.getByText('Game · Sony PlayStation · USA')).toBeTruthy();
    expect(screen.getByRole('cell', { name: 'Waiting for approval' })).toBeTruthy();
    expect(screen.getByRole('link', { name: 'Ask for an item' }).getAttribute('href')).toBe(
      '/requests/new',
    );
    expect(await axeViolations(container)).toEqual([]);

    const show = screen.getByRole('combobox', { name: 'Show' });
    await fireEvent.change(show, { target: { value: 'declined' } });
    expect(await screen.findByText('No request is declined.')).toBeTruthy();
    expect(asked.map((call) => call.url)).toContain('/api/v1/requests?limit=50&state=declined');
  });

  it('shows older requests after the cursor', async () => {
    server({
      '/api/v1/requests?limit=50': {
        status: 200,
        body: { items: [request()], next_cursor: 'c1' },
      },
      '/api/v1/requests?limit=50&cursor=c1': {
        status: 200,
        body: { items: [request({ id: 'q0', title: 'Older Game' })], next_cursor: null },
      },
      '/api/v1/report-settings': SETTINGS,
    });
    render(RequestsPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Show older requests' }));
    expect(await screen.findByRole('link', { name: 'Older Game' })).toBeTruthy();
    expect(screen.getByRole('link', { name: 'Some Game' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Show older requests' })).toBeNull();
  });

  it('says when nothing has been asked for', async () => {
    server({
      '/api/v1/requests?limit=50': { status: 200, body: { items: [] } },
      '/api/v1/report-settings': SETTINGS,
    });
    render(RequestsPage);
    expect(await screen.findByText('Nothing has been asked for yet.')).toBeTruthy();
  });
});

describe('Asking for an item', () => {
  it('asks only once the requester attests to owning it', async () => {
    const asked = server({
      'POST /api/v1/requests': { status: 201, body: request() },
      '/api/v1/report-settings': SETTINGS,
    });
    const { container } = render(NewRequestPage);
    expect(await axeViolations(container)).toEqual([]);
    const box = screen.getByRole('checkbox', {
      name: 'I legally own this item and want a digital image of it.',
    }) as HTMLInputElement;
    expect(box.checked).toBe(false);

    await fireEvent.input(screen.getByRole('textbox', { name: /Title/ }), {
      target: { value: ' Some Game ' },
    });
    await fireEvent.input(screen.getByRole('textbox', { name: /Platform or system/ }), {
      target: { value: 'Sony PlayStation' },
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Ask for this item' }));
    expect(await screen.findByText('A request cannot be made without this.')).toBeTruthy();
    expect(asked.filter((call) => call.method === 'POST')).toEqual([]);

    await fireEvent.click(box);
    await fireEvent.click(screen.getByRole('button', { name: 'Ask for this item' }));
    await vi.waitFor(() => expect(goto).toHaveBeenCalledWith('/requests/q1'));
    const sent = asked.find((call) => call.method === 'POST');
    expect(JSON.parse(sent?.body ?? '{}')).toEqual({
      kind: 'game',
      title: 'Some Game',
      details: { platform: 'Sony PlayStation' },
      refs: [],
      note: null,
      attests_ownership: true,
    });
  });

  it('takes an IMDb title for a film, and refuses what is not one', async () => {
    const asked = server({
      'POST /api/v1/requests': { status: 201, body: request({ kind: 'movie' }) },
      '/api/v1/report-settings': SETTINGS,
    });
    render(NewRequestPage);
    expect(screen.queryByRole('textbox', { name: /IMDb title/ })).toBeNull();
    await fireEvent.change(screen.getByRole('combobox', { name: 'Kind' }), {
      target: { value: 'movie' },
    });
    const imdb = await screen.findByRole('textbox', { name: /IMDb title/ });
    await fireEvent.input(screen.getByRole('textbox', { name: /Title/ }), {
      target: { value: 'A Film' },
    });
    await fireEvent.click(screen.getByRole('checkbox'));
    await fireEvent.input(imdb, { target: { value: 'https://example.com/title/tt0083658/' } });
    expect(await screen.findByText(/Not an IMDb title ID/)).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: 'Ask for this item' }));
    expect(asked.filter((call) => call.method === 'POST')).toEqual([]);

    await fireEvent.input(imdb, { target: { value: 'https://www.imdb.com/title/tt0083658/' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Ask for this item' }));
    await vi.waitFor(() => expect(goto).toHaveBeenCalled());
    const sent = JSON.parse(asked.find((call) => call.method === 'POST')?.body ?? '{}');
    expect(sent.refs).toEqual([{ provider: 'imdb', external_id: 'tt0083658' }]);
  });

  it('says why the server refused', async () => {
    server({
      'POST /api/v1/requests': problem(429, 'RATE_LIMITED', 'too many requests from this account'),
      '/api/v1/report-settings': SETTINGS,
    });
    render(NewRequestPage);
    await fireEvent.input(screen.getByRole('textbox', { name: /Title/ }), {
      target: { value: 'Some Game' },
    });
    await fireEvent.click(screen.getByRole('checkbox'));
    await fireEvent.click(screen.getByRole('button', { name: 'Ask for this item' }));
    expect(await screen.findByText(/many requests in the last hour/)).toBeTruthy();
    expect(goto).not.toHaveBeenCalled();
  });
});

const FOUND = {
  candidates: [
    {
      provider: 'musicbrainz',
      kind: 'music',
      title: 'Dark Side of the Moon',
      details: { artist: 'Pink Floyd', year: '1994', region: 'GB', format: 'CD' },
      refs: [{ provider: 'musicbrainz', external_id: '8a2b...' }],
      link: 'https://musicbrainz.org/release/8a2b',
    },
  ],
  sources: [{ provider: 'musicbrainz', state: 'answered', count: 1 }],
};

describe('Finding an item to ask for', () => {
  it('searches, fills the request from what is picked, and asks with it', async () => {
    const asked = server({
      '/api/v1/metadata/search?q=dark+side&kind=music': { status: 200, body: FOUND },
      'POST /api/v1/requests': { status: 201, body: request({ kind: 'music' }) },
      '/api/v1/report-settings': SETTINGS,
    });
    const { container } = render(NewRequestPage);
    await fireEvent.change(screen.getByRole('combobox', { name: 'Kind' }), {
      target: { value: 'music' },
    });
    const search = screen.getByRole('searchbox', { name: /Find it/ });
    await fireEvent.input(search, { target: { value: 'dark side' } });
    // Enter searches; it does not ask.
    await fireEvent.keyDown(search, { key: 'Enter' });
    expect(await screen.findByText('Dark Side of the Moon')).toBeTruthy();
    expect(screen.getByText('MusicBrainz · Pink Floyd · 1994 · GB · CD')).toBeTruthy();
    expect(asked.filter((call) => call.method === 'POST')).toEqual([]);
    const link = screen.getByRole('link', { name: /See it at MusicBrainz/ });
    expect(link.getAttribute('href')).toBe('https://musicbrainz.org/release/8a2b');
    expect(link.getAttribute('rel')).toContain('noopener');
    expect(await axeViolations(container)).toEqual([]);

    await fireEvent.click(screen.getByRole('button', { name: /^Use this: Dark Side of the Moon/ }));
    expect((screen.getByRole('textbox', { name: /Title/ }) as HTMLInputElement).value).toBe(
      'Dark Side of the Moon',
    );
    expect((screen.getByRole('textbox', { name: /Year/ }) as HTMLInputElement).value).toBe('1994');
    expect(screen.getByText('Pink Floyd')).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);

    await fireEvent.click(screen.getByRole('checkbox', { name: /I legally own this item/ }));
    await fireEvent.click(screen.getByRole('button', { name: 'Ask for this item' }));
    await vi.waitFor(() => expect(goto).toHaveBeenCalledWith('/requests/q1'));
    const sent = JSON.parse(asked.find((call) => call.method === 'POST')?.body ?? '{}');
    expect(sent).toMatchObject({
      kind: 'music',
      title: 'Dark Side of the Moon',
      details: { artist: 'Pink Floyd', format: 'CD', year: '1994', region: 'GB' },
      refs: [{ provider: 'musicbrainz', external_id: '8a2b...' }],
      attests_ownership: true,
    });
  });

  it('says which sources did not answer, and that nothing was found', async () => {
    server({
      '/api/v1/metadata/search?q=nothing&kind=game': {
        status: 200,
        body: {
          candidates: [],
          sources: [
            { provider: 'dat', state: 'answered', count: 0 },
            {
              provider: 'igdb',
              state: 'unconfigured',
              count: 0,
              detail:
                'IGDB is not configured; an administrator can add it on the Integrations page',
            },
          ],
        },
      },
      '/api/v1/report-settings': SETTINGS,
    });
    render(NewRequestPage);
    await fireEvent.input(screen.getByRole('searchbox', { name: /Find it/ }), {
      target: { value: 'nothing' },
    });
    await fireEvent.click(screen.getByRole('button', { name: /^Search/ }));
    expect(await screen.findByText(/Nothing was found/)).toBeTruthy();
    expect(screen.getByText(/IGDB is not configured/)).toBeTruthy();
  });

  it('searches an IMDb address whatever the kind', async () => {
    const asked = server({
      '/api/v1/metadata/search?q=https%3A%2F%2Fwww.imdb.com%2Ftitle%2Ftt0083658%2F': {
        status: 200,
        body: { candidates: [], sources: [{ provider: 'tmdb', state: 'answered', count: 0 }] },
      },
      '/api/v1/report-settings': SETTINGS,
    });
    render(NewRequestPage);
    await fireEvent.input(screen.getByRole('searchbox', { name: /Find it/ }), {
      target: { value: 'https://www.imdb.com/title/tt0083658/' },
    });
    await fireEvent.click(screen.getByRole('button', { name: /^Search/ }));
    await vi.waitFor(() =>
      expect(asked.map((call) => call.url)).toContain(
        '/api/v1/metadata/search?q=https%3A%2F%2Fwww.imdb.com%2Ftitle%2Ftt0083658%2F',
      ),
    );
  });
});

describe('A request', () => {
  it('shows a requester what they asked for and lets them withdraw it', async () => {
    const asked = server({
      '/api/v1/requests/q1': { status: 200, body: request() },
      'POST /api/v1/requests/q1/cancel': {
        status: 200,
        body: request({ state: 'canceled', can_cancel: false }),
      },
      '/api/v1/report-settings': SETTINGS,
    });
    const { container } = render(RequestPage);
    expect(await screen.findByRole('heading', { name: 'Some Game', level: 1 })).toBeTruthy();
    expect(screen.getByText('Sony PlayStation')).toBeTruthy();
    expect(screen.getByText('my copy is scratched')).toBeTruthy();
    expect(
      screen.getByText(/Nothing is searched for or downloaded until one approves it/),
    ).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Approve' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'Decline…' })).toBeNull();
    expect(await axeViolations(container)).toEqual([]);

    await fireEvent.click(screen.getByRole('button', { name: 'Withdraw…' }));
    await fireEvent.click(await screen.findByRole('button', { name: 'Withdraw this request' }));
    expect(await screen.findByText('Withdrawn')).toBeTruthy();
    expect(asked).toContainEqual({ url: '/api/v1/requests/q1/cancel', method: 'POST', body: '{}' });
    expect(screen.queryByRole('button', { name: 'Withdraw…' })).toBeNull();
  });

  it('lets a decider approve it', async () => {
    const asked = server({
      '/api/v1/requests/q1': { status: 200, body: request(DECIDER) },
      'POST /api/v1/requests/q1/approve': {
        status: 200,
        body: request({
          state: 'approved',
          decided_by: 'operator',
          decided_at: '2026-11-13T13:00:00Z',
          can_approve: false,
          can_decline: true,
          can_cancel: true,
        }),
      },
      '/api/v1/report-settings': SETTINGS,
    });
    render(RequestPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Approve' }));
    expect(await screen.findByText(/finds a release for it next/)).toBeTruthy();
    expect(screen.getByText(/by operator/)).toBeTruthy();
    expect(asked).toContainEqual({
      url: '/api/v1/requests/q1/approve',
      method: 'POST',
      body: '{}',
    });
    expect(screen.queryByRole('button', { name: 'Approve' })).toBeNull();
  });

  it('asks a decider why before declining, and shows the reason after', async () => {
    const asked = server({
      '/api/v1/requests/q1': { status: 200, body: request(DECIDER) },
      'POST /api/v1/requests/q1/decline': {
        status: 200,
        body: request({
          state: 'declined',
          decided_by: 'operator',
          decided_at: '2026-11-13T13:00:00Z',
          decline_reason: 'already in the library',
          can_approve: false,
          can_decline: false,
          can_cancel: false,
        }),
      },
      '/api/v1/report-settings': SETTINGS,
    });
    const { container } = render(RequestPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Decline…' }));
    expect(await screen.findByRole('heading', { name: 'Decline this request?' })).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);
    await fireEvent.click(screen.getByRole('button', { name: 'Decline this request' }));
    expect(await screen.findByText('Say why, for the requester.')).toBeTruthy();
    expect(asked.filter((call) => call.method === 'POST')).toEqual([]);

    await fireEvent.input(screen.getByRole('textbox', { name: /Why/ }), {
      target: { value: ' already in the library ' },
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Decline this request' }));
    expect(await screen.findByText('Why: already in the library')).toBeTruthy();
    expect(asked).toContainEqual({
      url: '/api/v1/requests/q1/decline',
      method: 'POST',
      body: '{"reason":"already in the library"}',
    });
    expect(screen.queryByRole('heading', { name: 'Decline this request?' })).toBeNull();
  });

  it('says why an action was refused and keeps the request on screen', async () => {
    server({
      '/api/v1/requests/q1': { status: 200, body: request(DECIDER) },
      'POST /api/v1/requests/q1/approve': problem(
        409,
        'CONFLICT',
        'this request is canceled, so it cannot be approved',
      ),
      '/api/v1/report-settings': SETTINGS,
    });
    render(RequestPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Approve' }));
    expect(
      await screen.findByText('this request is canceled, so it cannot be approved'),
    ).toBeTruthy();
    expect(screen.getByRole('heading', { name: 'Some Game', level: 1 })).toBeTruthy();
  });

  it('says when it cannot be found', async () => {
    server({
      '/api/v1/requests/q1': problem(404, 'NOT_FOUND', 'no request q1'),
      '/api/v1/report-settings': SETTINGS,
    });
    render(RequestPage);
    expect(await screen.findByText('Could not load this request')).toBeTruthy();
  });
});
