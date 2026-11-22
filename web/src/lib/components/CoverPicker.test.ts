// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen, within } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { axeViolations } from '$lib/testing/axe';
import { rememberSession } from '$lib/session';
import CoverPicker from './CoverPicker.svelte';

afterEach(() => {
  vi.unstubAllGlobals();
  rememberSession(null);
});

type Sent = { url: string; method: string; body: unknown };

const FOUND = {
  candidates: [
    {
      provider: 'tmdb',
      kind: 'movie',
      title: 'Hackers',
      details: { year: '1995' },
      refs: [
        { provider: 'tmdb', external_id: 'movie/10428' },
        { provider: 'imdb', external_id: 'tt0113243' },
      ],
      link: null,
      thumbnail: '/api/v1/metadata/thumbnails/0123456789abcdef0123456789abcdef',
    },
    // A DAT's game has no cover to fetch, so it is not offered.
    {
      provider: 'dat',
      kind: 'game',
      title: 'Hackers (USA)',
      details: {},
      refs: [{ provider: 'dat', external_id: 'Hackers (USA)' }],
      link: null,
    },
  ],
  sources: [],
};

function server(): Sent[] {
  const sent: Sent[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const url = String(input);
      const method = init?.method ?? 'GET';
      sent.push({ url, method, body: init?.body ? JSON.parse(String(init.body)) : null });
      const body = url.startsWith('/api/v1/metadata/search')
        ? FOUND
        : method === 'PUT'
          ? { source: 'tmdb', external_id: 'movie/10428' }
          : null;
      return {
        ok: true,
        status: method === 'DELETE' ? 204 : method === 'PUT' ? 202 : 200,
        json: async () => body,
        headers: new Headers(),
      } as Response;
    }),
  );
  return sent;
}

function picker(props: Record<string, unknown> = {}) {
  const onchanged = vi.fn();
  const rendered = render(CoverPicker, {
    props: {
      artifactId: 'a1',
      name: 'Hackers',
      cover: null,
      picked: false,
      onchanged,
      ...props,
    },
  });
  return { ...rendered, onchanged };
}

describe('Picking a cover', () => {
  it('searches as the request form does and picks what the artifact is', async () => {
    rememberSession({ role: 'operator' } as Parameters<typeof rememberSession>[0]);
    const sent = server();
    const { container, onchanged } = picker();
    expect(screen.getByText(/None yet/)).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: 'Find cover art…' }));
    // The artifact's name, to start.
    expect((screen.getByRole('searchbox', { name: /What is it/ }) as HTMLInputElement).value).toBe(
      'Hackers',
    );
    await fireEvent.click(screen.getByRole('button', { name: 'Search' }));
    const list = await screen.findByRole('list', { name: 'What it might be' });
    expect(within(list).getAllByRole('listitem').length).toBe(1);
    // Seen before it is picked, at Tangible's address.
    expect(list.querySelector('img.thumbnail')?.getAttribute('src')).toBe(
      FOUND.candidates[0]?.thumbnail,
    );
    expect(await axeViolations(container)).toEqual([]);
    await fireEvent.click(screen.getByRole('button', { name: 'Use the cover of Hackers' }));
    await vi.waitFor(() => expect(onchanged).toHaveBeenCalled());
    const put = sent.find((call) => call.method === 'PUT');
    expect(put?.url).toBe('/api/v1/artifacts/a1/artwork');
    // Every reference the candidate has: the server takes the one it fetches by.
    expect(put?.body).toEqual({ refs: FOUND.candidates[0]?.refs });
    expect(screen.getByText(/Its cover appears once Tangible has fetched it/)).toBeTruthy();
  });

  it('says where a cover came from, and removes one picked by hand', async () => {
    rememberSession({ role: 'operator' } as Parameters<typeof rememberSession>[0]);
    const sent = server();
    const { onchanged } = picker({
      cover: { url: '/api/v1/artwork/c1?v=abc', source: 'musicbrainz' },
      picked: true,
    });
    expect(
      screen.getByText('Picked by hand, from the Cover Art Archive (MusicBrainz).'),
    ).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: 'Remove the picked cover' }));
    await vi.waitFor(() => expect(onchanged).toHaveBeenCalled());
    expect(sent.find((call) => call.method === 'DELETE')?.url).toBe('/api/v1/artifacts/a1/artwork');
  });

  it('shows a viewer where the cover came from, and nothing to change', () => {
    rememberSession({ role: 'viewer' } as Parameters<typeof rememberSession>[0]);
    server();
    picker({ cover: { url: '/api/v1/artwork/c1?v=abc', source: 'tmdb' } });
    expect(screen.getByText('From TMDB.')).toBeTruthy();
    expect(screen.queryByRole('button')).toBeNull();
  });
});
