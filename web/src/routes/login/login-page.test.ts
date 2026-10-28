// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

let search = '';
vi.mock('$app/state', () => ({
  page: {
    get url() {
      return new URL(`http://tangible.test/login${search}`);
    },
  },
}));
vi.mock('$app/navigation', () => ({ goto: vi.fn() }));

const { default: LoginPage } = await import('./+page.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
  search = '';
});

function offered(status: unknown) {
  vi.stubGlobal(
    'fetch',
    vi.fn(
      async () =>
        ({
          ok: true,
          status: 200,
          json: async () => status,
          headers: new Headers(),
        }) as Response,
    ),
  );
}

describe('Sign-in page', () => {
  it('offers the identity provider, coming back to where it was asked', async () => {
    search = '?next=%2Flibrary';
    offered({ enabled: true, display_name: 'VoidAuth', password_sign_in: 'all' });
    render(LoginPage);
    const link = await screen.findByRole('link', { name: 'Sign in with VoidAuth' });
    expect(link.getAttribute('href')).toBe('/api/v1/session/oidc/start?return_to=%2Flibrary');
    expect(screen.getByLabelText('Password')).toBeTruthy();
  });

  it('offers only passwords when no provider is set up', async () => {
    offered({ enabled: false, display_name: null, password_sign_in: 'all' });
    render(LoginPage);
    await screen.findByLabelText('Password');
    expect(screen.queryByRole('link', { name: /Sign in with/ })).toBeNull();
  });

  it('says in words why the provider sign-in did not work', async () => {
    search = '?oidc_error=OIDC_NO_ROLE';
    offered({ enabled: true, display_name: 'VoidAuth', password_sign_in: 'setup-only' });
    render(LoginPage);
    expect(await screen.findByText(/in none of the groups Tangible gives access to/)).toBeTruthy();
    expect(await screen.findByText(/Passwords are for the administrator setup made/)).toBeTruthy();
  });
});
