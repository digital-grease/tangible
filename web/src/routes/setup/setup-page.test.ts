// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { rememberSession } from '$lib/session';
import { axeViolations } from '$lib/testing/axe';

const goto = vi.fn();
vi.mock('$app/navigation', () => ({ goto: (...args: unknown[]) => goto(...args) }));

const { default: SetupPage } = await import('./+page.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
  goto.mockReset();
  rememberSession(null);
});

type Reply = { status: number; body: unknown };

/** Answer by method and path; record what was asked. */
function server(routes: Record<string, Reply>): string[] {
  const asked: string[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const key = `${init?.method ?? 'GET'} ${String(input)}`;
      asked.push(key);
      const reply = routes[key] ?? { status: 404, body: {} };
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

const SESSION = {
  user_id: 'u1',
  username: 'admin',
  role: 'administrator',
  csrf_token: 'token',
  expires_at: '2026-11-13T00:00:00Z',
};

async function fill(label: string, value: string) {
  await fireEvent.input(screen.getByLabelText(label), { target: { value } });
}

describe('Set-up page', () => {
  it('asks for the administrator, and refuses two different passwords itself', async () => {
    const asked = server({ 'GET /api/v1/setup': { status: 200, body: { needed: true } } });
    const { container } = render(SetupPage);
    expect(await screen.findByText(/This server has no accounts yet/)).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);

    await fill('Username', 'admin');
    await fill('Password', 'a long passphrase');
    await fill('Password again', 'another passphrase');
    await fireEvent.submit(screen.getByRole('button', { name: 'Create the administrator' }));
    expect(await screen.findByText('The two passwords are different.')).toBeTruthy();
    expect(asked).not.toContain('POST /api/v1/setup');
  });

  it('creates the administrator and goes to the Overview', async () => {
    const asked = server({
      'GET /api/v1/setup': { status: 200, body: { needed: true } },
      'POST /api/v1/setup': { status: 201, body: SESSION },
    });
    render(SetupPage);
    await screen.findByText(/This server has no accounts yet/);
    await fill('Username', 'admin');
    await fill('Password', 'a long passphrase');
    await fill('Password again', 'a long passphrase');
    await fireEvent.submit(screen.getByRole('button', { name: 'Create the administrator' }));
    await vi.waitFor(() => expect(goto).toHaveBeenCalledWith('/', { replaceState: true }));
    expect(asked).toContain('POST /api/v1/setup');
  });

  it('says setup is closed once somebody else has finished it', async () => {
    server({
      'GET /api/v1/setup': { status: 200, body: { needed: true } },
      'POST /api/v1/setup': {
        status: 409,
        body: {
          type: 'about:blank',
          title: 'Conflict',
          status: 409,
          code: 'CONFLICT',
          detail: 'setup is already complete',
        },
      },
    });
    render(SetupPage);
    await screen.findByText(/This server has no accounts yet/);
    await fill('Username', 'admin');
    await fill('Password', 'a long passphrase');
    await fill('Password again', 'a long passphrase');
    await fireEvent.submit(screen.getByRole('button', { name: 'Create the administrator' }));
    expect(await screen.findByText('Already set up')).toBeTruthy();
    expect(goto).not.toHaveBeenCalled();
  });

  it('sends a visitor to a server already set up to sign in', async () => {
    server({ 'GET /api/v1/setup': { status: 200, body: { needed: false } } });
    const { container } = render(SetupPage);
    expect(await screen.findByText('Already set up')).toBeTruthy();
    expect(screen.getByRole('link', { name: 'Sign in' }).getAttribute('href')).toBe('/login');
    expect(screen.queryByLabelText('Password')).toBeNull();
    expect(await axeViolations(container)).toEqual([]);
  });
});
