// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { axeViolations } from '$lib/testing/axe';
import { rememberSession } from '$lib/session';

const { default: UsersPage } = await import('./+page.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
  rememberSession(null);
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

  it('passes the accessibility checks, the list and the form both shown', async () => {
    server({
      '/api/v1/users': { status: 200, body: { items: [user(1), user(2)], next_cursor: null } },
    });
    const { container } = render(UsersPage);
    await screen.findByRole('rowheader', { name: 'user1' });
    expect(screen.getByRole('region', { name: 'Create an account' })).toBeTruthy();
    expect(screen.getByLabelText(/^Password/)).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);
  });

  it('offers no form where only setup may use a password, and says why', async () => {
    server({
      '/api/v1/users': {
        status: 200,
        body: { items: [user(1), { ...user(3), signs_in_with: 'none' }], next_cursor: null },
      },
      '/api/v1/session/oidc': {
        status: 200,
        body: { enabled: true, display_name: 'VoidAuth', password_sign_in: 'setup-only' },
      },
    });
    const { container } = render(UsersPage);
    const panel = await screen.findByRole('region', { name: 'New accounts' });
    expect(screen.queryByRole('region', { name: 'Create an account' })).toBeNull();
    expect(screen.queryByLabelText(/^Password/)).toBeNull();
    expect(panel.textContent).toContain('TANGIBLE_PASSWORD_SIGN_IN=setup-only');
    expect(panel.textContent).toContain('signs in through VoidAuth');
    expect(panel.textContent).toContain('TANGIBLE_PASSWORD_SIGN_IN=all');
    // The account that cannot sign in is marked, and the page says why.
    expect(screen.getByText('Cannot sign in')).toBeTruthy();
    expect(
      screen.getByText(/An account that cannot sign in was made with a password/),
    ).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);
  });

  it('offers the form where passwords are for everyone', async () => {
    server({
      '/api/v1/users': { status: 200, body: { items: [user(1)], next_cursor: null } },
      '/api/v1/session/oidc': {
        status: 200,
        body: { enabled: false, display_name: null, password_sign_in: 'all' },
      },
    });
    render(UsersPage);
    expect(await screen.findByRole('region', { name: 'Create an account' })).toBeTruthy();
    expect(screen.queryByRole('region', { name: 'New accounts' })).toBeNull();
    expect(screen.queryByText(/cannot sign in/i)).toBeNull();
  });

  it('disables an account after asking, and enables one in a click', async () => {
    rememberSession({ username: 'admin', role: 'administrator' } as Parameters<
      typeof rememberSession
    >[0]);
    const asked = server({
      '/api/v1/users': {
        status: 200,
        body: { items: [user(1), { ...user(4), disabled: true }], next_cursor: null },
      },
      '/api/v1/users/u1/disable': { status: 200, body: { ...user(1), disabled: true } },
      '/api/v1/users/u4/enable': { status: 200, body: user(4) },
    });
    const { container } = render(UsersPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Disable user1…' }));
    // Asked first, saying what it does.
    expect(screen.getByRole('heading', { name: 'Disable user1?' })).toBeTruthy();
    expect(screen.getByText(/They are signed out now/)).toBeTruthy();
    expect(asked).not.toContain('/api/v1/users/u1/disable');
    expect(await axeViolations(container)).toEqual([]);
    await fireEvent.click(screen.getByRole('button', { name: 'Disable user1' }));
    expect(await screen.findByText('Disabled user1.')).toBeTruthy();
    expect(asked).toContain('/api/v1/users/u1/disable');
    expect(screen.queryByRole('heading', { name: 'Disable user1?' })).toBeNull();

    await fireEvent.click(screen.getByRole('button', { name: 'Enable user4' }));
    expect(await screen.findByText('Enabled user4.')).toBeTruthy();
    expect(asked).toContain('/api/v1/users/u4/enable');
  });

  it('deletes an account that never signed in, after asking, and offers it for no other', async () => {
    rememberSession({ username: 'admin', role: 'administrator' } as Parameters<
      typeof rememberSession
    >[0]);
    const asked = server({
      '/api/v1/users': {
        status: 200,
        body: {
          items: [
            { ...user(5), can_delete: true },
            { ...user(6), can_delete: false, last_signed_in_at: '2026-11-20T10:00:00Z' },
          ],
          next_cursor: null,
        },
      },
      '/api/v1/users/u5': { status: 204, body: null },
    });
    const { container } = render(UsersPage);
    await screen.findByRole('rowheader', { name: 'user5' });
    expect(screen.queryByRole('button', { name: 'Delete user6…' })).toBeNull();
    expect(
      screen.getByText(/Only an account that has never signed in can be deleted/),
    ).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: 'Delete user5…' }));
    expect(screen.getByRole('heading', { name: 'Delete user5?' })).toBeTruthy();
    expect(screen.getByText(/its name can be given to someone again/)).toBeTruthy();
    expect(asked).not.toContain('/api/v1/users/u5');
    expect(await axeViolations(container)).toEqual([]);
    await fireEvent.click(screen.getByRole('button', { name: 'Delete user5' }));
    expect(await screen.findByText('Deleted user5.')).toBeTruthy();
    expect(asked).toContain('/api/v1/users/u5');
  });

  it("offers nothing against one's own account, and says why a change was refused", async () => {
    rememberSession({ username: 'user1', role: 'administrator' } as Parameters<
      typeof rememberSession
    >[0]);
    server({
      '/api/v1/users': { status: 200, body: { items: [user(1), user(3)], next_cursor: null } },
      '/api/v1/users/u3/disable': {
        status: 409,
        body: {
          type: 'about:blank',
          title: 'The last administrator',
          status: 409,
          code: 'LAST_ADMINISTRATOR',
          detail: 'it is the only enabled administrator',
        },
      },
    });
    render(UsersPage);
    await screen.findByRole('rowheader', { name: 'user1' });
    expect(screen.queryByRole('button', { name: /user1/ })).toBeNull();
    expect(screen.getByText('You')).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: 'Disable user3…' }));
    await fireEvent.click(screen.getByRole('button', { name: 'Disable user3' }));
    expect(await screen.findByText('it is the only enabled administrator')).toBeTruthy();
  });

  it('tells anybody but an administrator that the page is not theirs', async () => {
    server({
      '/api/v1/users': {
        status: 403,
        body: {
          type: 'about:blank',
          title: 'Forbidden',
          status: 403,
          code: 'FORBIDDEN',
          detail: 'the operator role does not allow manage_users',
        },
      },
    });
    render(UsersPage);
    expect(
      await screen.findByText('Only an administrator can see or create accounts.'),
    ).toBeTruthy();
    expect(screen.queryByRole('region', { name: 'Create an account' })).toBeNull();
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
