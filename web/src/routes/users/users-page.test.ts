// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

const { default: UsersPage } = await import('./+page.svelte');

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

function user(n: number) {
  return {
    id: `u${n}`,
    username: `user${n}`,
    role: 'operator',
    disabled: false,
    last_signed_in_at: null,
    created_at: '2026-10-24T12:00:00Z',
    signs_in_with: n === 2 ? 'provider' : 'password',
  };
}

describe('Accounts page', () => {
  it('shows the first page and adds the next when asked', async () => {
    const asked = server({
      '/api/v1/users': { status: 200, body: { items: [user(1), user(2)], next_cursor: 'c2' } },
      '/api/v1/users?cursor=c2': { status: 200, body: { items: [user(3)], next_cursor: null } },
    });
    render(UsersPage);
    expect(await screen.findByRole('rowheader', { name: 'user1' })).toBeTruthy();
    expect(screen.queryByRole('rowheader', { name: 'user3' })).toBeNull();
    // How each signs in, in words.
    expect(screen.getAllByText('Password').length).toBeGreaterThan(0);
    expect(screen.getByText('Identity provider')).toBeTruthy();

    await fireEvent.click(screen.getByRole('button', { name: 'Show more accounts' }));
    expect(await screen.findByRole('rowheader', { name: 'user3' })).toBeTruthy();
    expect(screen.getByRole('rowheader', { name: 'user1' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Show more accounts' })).toBeNull();
    expect(asked).toContain('/api/v1/users?cursor=c2');
  });

  it('offers no more when everything fits on one page', async () => {
    server({ '/api/v1/users': { status: 200, body: { items: [user(1)], next_cursor: null } } });
    render(UsersPage);
    expect(await screen.findByRole('rowheader', { name: 'user1' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Show more accounts' })).toBeNull();
  });

  it('keeps the accounts shown when the next page cannot be loaded', async () => {
    server({
      '/api/v1/users': { status: 200, body: { items: [user(1)], next_cursor: 'c1' } },
      '/api/v1/users?cursor=c1': {
        status: 503,
        body: {
          type: 'about:blank',
          title: 'Unavailable',
          status: 503,
          code: 'STORAGE_UNAVAILABLE',
          detail: 'accounts could not be read',
        },
      },
    });
    render(UsersPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Show more accounts' }));
    expect(await screen.findByText(/Could not load more accounts/)).toBeTruthy();
    expect(screen.getByRole('rowheader', { name: 'user1' })).toBeTruthy();
  });
});
