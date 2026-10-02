// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import type { components } from './api-types';

export type SessionView = components['schemas']['SessionView'];
export type SetupStatus = components['schemas']['SetupStatus'];
export type UserView = components['schemas']['UserView'];
export type UserList = components['schemas']['UserList'];
export type CreateUserRequest = components['schemas']['CreateUserRequest'];
export type Problem = components['schemas']['Problem'];

/** Shortest password the server accepts. Shown as a hint; the server decides. */
export const MIN_PASSWORD_CHARS = 12;

/** Methods that change nothing, and so carry no anti-forgery token. */
const SAFE_METHODS = new Set(['GET', 'HEAD', 'OPTIONS']);

/**
 * The signed-in session's anti-forgery token.
 *
 * Held in memory only. It is not a credential on its own, but there is no
 * reason to leave it in storage that outlives the page: a reload asks the
 * server for the session again and gets it back.
 */
let csrfToken: string | null = null;

/** Called when the server says nobody is signed in. */
let onSignedOut: (() => void) | null = null;

/** Remember the session the server returned, or forget it. */
export function rememberSession(session: SessionView | null): void {
  csrfToken = session?.csrf_token ?? null;
}

/** Register what to do when a request comes back 401. */
export function whenSignedOut(handler: (() => void) | null): void {
  onSignedOut = handler;
}

/** Add the anti-forgery token to a request that changes something. */
export function withCsrf(init: RequestInit | undefined, token: string | null): RequestInit {
  const method = (init?.method ?? 'GET').toUpperCase();
  const headers = new Headers(init?.headers);
  if (!SAFE_METHODS.has(method) && token) headers.set('x-csrf-token', token);
  return { ...init, headers, credentials: 'same-origin' };
}

/**
 * `fetch` for the API: sends the session cookie, adds the anti-forgery token
 * to mutations, and reports a lapsed session so the page can ask the person
 * to sign in again rather than show a wall of errors.
 */
export const apiFetch: typeof fetch = async (input, init) => {
  const response = await fetch(input, withCsrf(init, csrfToken));
  if (response.status === 401 && onSignedOut) onSignedOut();
  return response;
};

/**
 * Where to go after signing in.
 *
 * Only a path on this site. Anything else, including `//elsewhere`, which a
 * browser reads as another host, goes to the start page, so a crafted sign-in
 * link cannot send someone off-site with a fresh session behind them.
 */
export function safeNext(next: string | null | undefined): string {
  if (!next || !next.startsWith('/') || next.startsWith('//') || next.startsWith('/\\')) {
    return '/';
  }
  if (next.startsWith('/login') || next.startsWith('/setup')) return '/';
  return next;
}

/** What to tell someone whose sign-in was refused. */
export function signInRefusal(problem: Problem, retryAfterSeconds: number | null): string {
  if (problem.code === 'RATE_LIMITED') {
    const minutes = retryAfterSeconds ? Math.max(1, Math.ceil(retryAfterSeconds / 60)) : null;
    return minutes
      ? `Too many failed attempts for this username. Try again in ${minutes} minute${minutes === 1 ? '' : 's'}.`
      : 'Too many failed attempts for this username. Try again later.';
  }
  if (problem.code === 'UNAUTHENTICATED') return 'The username or password is incorrect.';
  return problem.detail;
}

/** A role, as a person reads it. */
export function roleLabel(role: string): string {
  switch (role) {
    case 'administrator':
      return 'Administrator';
    case 'operator':
      return 'Operator';
    case 'viewer':
      return 'Viewer';
    default:
      return role;
  }
}

export type Result<T> =
  { kind: 'ok'; data: T } | { kind: 'error'; problem: Problem; retryAfter: number | null };

function transportProblem(detail: string): Problem {
  return {
    type: 'about:blank',
    title: 'Could not reach the server',
    status: 0,
    code: 'TRANSPORT_FAILURE',
    detail,
  };
}

/** One API call, with a lapsed session and a refusal both reported the same way. */
export async function apiCall<T>(
  fetcher: typeof fetch,
  path: string,
  method = 'GET',
  body?: unknown,
): Promise<Result<T>> {
  let response: Response;
  try {
    response = await fetcher(path, {
      method,
      headers: body === undefined ? undefined : { 'content-type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
  } catch (error) {
    return {
      kind: 'error',
      problem: transportProblem(error instanceof Error ? error.message : 'unknown failure'),
      retryAfter: null,
    };
  }
  if (response.status === 204) return { kind: 'ok', data: undefined as T };

  let parsed: unknown;
  try {
    parsed = await response.json();
  } catch {
    return {
      kind: 'error',
      problem: transportProblem(`the server returned ${response.status} with no readable body`),
      retryAfter: null,
    };
  }
  if (!response.ok) {
    const problem = parsed as Partial<Problem>;
    const retry = Number(response.headers?.get?.('retry-after'));
    return {
      kind: 'error',
      problem:
        typeof problem?.code === 'string'
          ? (parsed as Problem)
          : transportProblem(`the server returned ${response.status}`),
      retryAfter: Number.isFinite(retry) && retry > 0 ? retry : null,
    };
  }
  return { kind: 'ok', data: parsed as T };
}

/** Whether the server still needs its first account. */
export function loadSetupStatus(fetcher: typeof fetch = apiFetch): Promise<Result<SetupStatus>> {
  return apiCall(fetcher, '/api/v1/setup');
}

/** The signed-in session, if there is one. */
export function loadSession(fetcher: typeof fetch = apiFetch): Promise<Result<SessionView>> {
  return apiCall(fetcher, '/api/v1/session');
}

/** Create the first administrator, signing them in. */
export async function completeSetup(
  username: string,
  password: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<SessionView>> {
  const result = await apiCall<SessionView>(fetcher, '/api/v1/setup', 'POST', {
    username,
    password,
  });
  if (result.kind === 'ok') rememberSession(result.data);
  return result;
}

/** Sign in. */
export async function signIn(
  username: string,
  password: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<SessionView>> {
  const result = await apiCall<SessionView>(fetcher, '/api/v1/session', 'POST', {
    username,
    password,
  });
  if (result.kind === 'ok') rememberSession(result.data);
  return result;
}

/** Sign out. The token is forgotten whatever the server says. */
export async function signOut(fetcher: typeof fetch = apiFetch): Promise<Result<void>> {
  const result = await apiCall<void>(fetcher, '/api/v1/session', 'DELETE');
  rememberSession(null);
  return result;
}

/** Every account. Administrators only. */
export function loadUsers(fetcher: typeof fetch = apiFetch): Promise<Result<UserList>> {
  return apiCall(fetcher, '/api/v1/users');
}

/** Create an account. Administrators only. */
export function createUser(
  request: CreateUserRequest,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<UserView>> {
  return apiCall(fetcher, '/api/v1/users', 'POST', request);
}
