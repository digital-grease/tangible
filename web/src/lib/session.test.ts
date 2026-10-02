// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  apiFetch,
  createUser,
  rememberSession,
  roleLabel,
  safeNext,
  signIn,
  signInRefusal,
  signOut,
  whenSignedOut,
  withCsrf,
  type Problem,
  type SessionView,
} from './session';

const session: SessionView = {
  username: 'owner',
  role: 'administrator',
  csrf_token: 'c'.repeat(64),
  expires_at: '2026-10-08T00:00:00Z',
};

const problem = (code: string, detail = 'nope'): Problem => ({
  type: 'about:blank',
  title: 'Refused',
  status: 401,
  code,
  detail,
});

interface Recorded {
  path: string;
  method: string;
  body: unknown;
}

function stubFetch(
  status: number,
  body: unknown,
  recorded: Recorded[] = [],
  headers: Record<string, string> = {},
): typeof fetch {
  return (async (input: string | URL | Request, init?: RequestInit) => {
    recorded.push({
      path: typeof input === 'string' ? input : input.toString(),
      method: init?.method ?? 'GET',
      body: typeof init?.body === 'string' ? JSON.parse(init.body) : undefined,
    });
    return {
      ok: status >= 200 && status < 300,
      status,
      headers: new Headers(headers),
      json: async () => body,
    } as Response;
  }) as typeof fetch;
}

afterEach(() => {
  rememberSession(null);
  whenSignedOut(null);
  vi.unstubAllGlobals();
});

describe('the anti-forgery token', () => {
  it('goes on mutations and not on reads', () => {
    for (const method of ['POST', 'PATCH', 'PUT', 'DELETE', 'post']) {
      const init = withCsrf({ method }, 'token');
      expect(new Headers(init.headers).get('x-csrf-token')).toBe('token');
    }
    for (const method of [undefined, 'GET', 'HEAD', 'OPTIONS']) {
      const init = withCsrf({ method }, 'token');
      expect(new Headers(init.headers).get('x-csrf-token')).toBeNull();
    }
  });

  it('keeps the headers a caller set and always sends the cookie', () => {
    const init = withCsrf(
      { method: 'POST', headers: { 'idempotency-key': 'k', 'content-type': 'application/json' } },
      'token',
    );
    const headers = new Headers(init.headers);
    expect(headers.get('idempotency-key')).toBe('k');
    expect(headers.get('content-type')).toBe('application/json');
    expect(init.credentials).toBe('same-origin');
  });

  it('is what apiFetch sends once a session is remembered, and not after', async () => {
    const seen: Headers[] = [];
    vi.stubGlobal('fetch', async (_input: string, init?: RequestInit) => {
      seen.push(new Headers(init?.headers));
      return { ok: true, status: 200 } as Response;
    });
    rememberSession(session);
    await apiFetch('/api/v1/titles', { method: 'POST' });
    rememberSession(null);
    await apiFetch('/api/v1/titles', { method: 'POST' });
    expect(seen[0]?.get('x-csrf-token')).toBe(session.csrf_token);
    expect(seen[1]?.get('x-csrf-token')).toBeNull();
  });
});

describe('a lapsed session', () => {
  it('is reported when any API call comes back 401', async () => {
    vi.stubGlobal('fetch', async () => ({ ok: false, status: 401 }) as Response);
    const signedOut = vi.fn();
    whenSignedOut(signedOut);
    await apiFetch('/api/v1/artifacts');
    expect(signedOut).toHaveBeenCalledOnce();
  });

  it('is not reported for other refusals', async () => {
    vi.stubGlobal('fetch', async () => ({ ok: false, status: 403 }) as Response);
    const signedOut = vi.fn();
    whenSignedOut(signedOut);
    await apiFetch('/api/v1/users');
    expect(signedOut).not.toHaveBeenCalled();
  });
});

describe('where to go after signing in', () => {
  it('follows a path on this site', () => {
    expect(safeNext('/burns/abc?tab=events')).toBe('/burns/abc?tab=events');
  });

  it('refuses anything that could leave the site', () => {
    for (const next of [
      'https://evil.example',
      '//evil.example/path',
      '/\\evil.example',
      'javascript:alert(1)',
      '',
      null,
      undefined,
    ]) {
      expect(safeNext(next)).toBe('/');
    }
  });

  it('does not send someone back to the sign-in page', () => {
    expect(safeNext('/login?next=/login')).toBe('/');
    expect(safeNext('/setup')).toBe('/');
  });
});

describe('telling someone why they were not signed in', () => {
  it('does not say which of the two was wrong', () => {
    expect(signInRefusal(problem('UNAUTHENTICATED'), null)).toBe(
      'The username or password is incorrect.',
    );
  });

  it('says how long to wait when locked out', () => {
    expect(signInRefusal(problem('RATE_LIMITED'), 61)).toContain('2 minutes');
    expect(signInRefusal(problem('RATE_LIMITED'), 30)).toContain('1 minute.');
    expect(signInRefusal(problem('RATE_LIMITED'), null)).toContain('later');
  });

  it('passes anything else through', () => {
    expect(signInRefusal(problem('STORAGE_UNAVAILABLE', 'database down'), null)).toBe(
      'database down',
    );
  });
});

describe('signing in and out', () => {
  it('remembers the session it gets back', async () => {
    const recorded: Recorded[] = [];
    const result = await signIn('Owner', 'a long passphrase', stubFetch(200, session, recorded));
    expect(result.kind).toBe('ok');
    expect(recorded[0]).toEqual({
      path: '/api/v1/session',
      method: 'POST',
      body: { username: 'Owner', password: 'a long passphrase' },
    });

    const seen: Headers[] = [];
    vi.stubGlobal('fetch', async (_input: string, init?: RequestInit) => {
      seen.push(new Headers(init?.headers));
      return { ok: true, status: 200 } as Response;
    });
    await apiFetch('/api/v1/titles', { method: 'POST' });
    expect(seen[0]?.get('x-csrf-token')).toBe(session.csrf_token);
  });

  it('carries Retry-After back with a rate-limit refusal', async () => {
    const result = await signIn(
      'owner',
      'wrong',
      stubFetch(429, problem('RATE_LIMITED'), [], { 'retry-after': '120' }),
    );
    expect(result).toMatchObject({ kind: 'error', retryAfter: 120 });
  });

  it('forgets the token on sign out even if the server is unreachable', async () => {
    rememberSession(session);
    const failing = (async () => {
      throw new Error('offline');
    }) as typeof fetch;
    const result = await signOut(failing);
    expect(result.kind).toBe('error');

    const seen: Headers[] = [];
    vi.stubGlobal('fetch', async (_input: string, init?: RequestInit) => {
      seen.push(new Headers(init?.headers));
      return { ok: true, status: 200 } as Response;
    });
    await apiFetch('/api/v1/titles', { method: 'POST' });
    expect(seen[0]?.get('x-csrf-token')).toBeNull();
  });

  it('treats a 204 sign-out as success', async () => {
    const result = await signOut(stubFetch(204, null));
    expect(result.kind).toBe('ok');
  });
});

describe('accounts', () => {
  it('posts the new account as given', async () => {
    const recorded: Recorded[] = [];
    await createUser(
      { username: 'alice', password: 'a long passphrase', role: 'viewer' },
      stubFetch(201, { id: 'u1' }, recorded),
    );
    expect(recorded[0]).toMatchObject({ path: '/api/v1/users', method: 'POST' });
  });

  it('labels roles in words', () => {
    expect(roleLabel('administrator')).toBe('Administrator');
    expect(roleLabel('viewer')).toBe('Viewer');
    expect(roleLabel('something-new')).toBe('something-new');
  });
});
