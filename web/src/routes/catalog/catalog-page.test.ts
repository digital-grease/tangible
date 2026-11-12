// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen, within } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { axeViolations } from '$lib/testing/axe';

const { default: CatalogPage } = await import('./+page.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
});

const at = '2026-11-11T12:00:00Z';
const title = {
  id: 't1',
  kind: 'software',
  display_title: 'Debian',
  sort_title: 'Debian',
  release_year: 2025,
  edition_count: 1,
  created_at: at,
  version: 1,
};
const edition = {
  id: 'e1',
  title_id: 't1',
  display_name: '13.1 netinst',
  region: null,
  set_count: 1,
  created_at: at,
  version: 1,
};
const set = {
  id: 's1',
  edition_id: 'e1',
  name: 'Single disc',
  set_kind: 'single',
  disc_count: 1,
  disc_count_expected: 2,
  created_at: at,
  version: 1,
};
const disc = {
  id: 'd1',
  disc_set_id: 's1',
  sequence_number: 1,
  display_name: null,
  media_family: 'cd',
  compatibility_claim: 'unknown',
  artifact_count: 1,
  copy_count: 0,
  created_at: at,
  version: 1,
};

function server() {
  const routes: Record<string, unknown> = {
    '/api/v1/titles': { items: [title], next_cursor: null },
    '/api/v1/titles/t1/editions': [edition],
    '/api/v1/editions/e1/disc-sets': [set],
    '/api/v1/disc-sets/s1/discs': [disc],
  };
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request) => {
      const url = String(input);
      const body = routes[url];
      return {
        ok: body !== undefined,
        status: body === undefined ? 404 : 200,
        json: async () =>
          body ?? {
            type: 'about:blank',
            title: 'Not found',
            status: 404,
            code: 'NOT_FOUND',
            detail: url,
          },
        headers: new Headers(),
      } as Response;
    }),
  );
}

describe('Catalog page', () => {
  it('opens the chain one column at a time, saying which item is chosen', async () => {
    server();
    const { container } = render(CatalogPage);
    const titles = screen.getByRole('region', { name: 'Titles' });
    const debian = await within(titles).findByRole('button', { name: /Debian/ });
    expect(debian.getAttribute('aria-pressed')).toBe('false');
    const editions = screen.getByRole('region', { name: 'Editions' });
    expect(within(editions).getByText('Choose a title to see its editions.')).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);

    await fireEvent.click(debian);
    expect(debian.getAttribute('aria-pressed')).toBe('true');
    await fireEvent.click(await within(editions).findByRole('button', { name: /13\.1 netinst/ }));
    const sets = screen.getByRole('region', { name: 'Disc sets' });
    await fireEvent.click(await within(sets).findByRole('button', { name: /Single disc/ }));
    expect(within(sets).getByText('Incomplete')).toBeTruthy();

    const discs = await screen.findByRole('table', { name: 'Discs in this set' });
    expect(
      within(discs).getByRole('link', { name: 'Burn to this disc' }).getAttribute('href'),
    ).toBe('/burns/new?disc=d1');
    expect(await axeViolations(container)).toEqual([]);
  });
});
