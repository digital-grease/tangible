// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import { describe, expect, it } from 'vitest';
import { issueEnrollment } from './workers';

describe('issuing an enrollment token', () => {
  it('asks for the lifetime given and returns the token', async () => {
    const seen: { path: string; method: string; body: unknown }[] = [];
    const fetcher = (async (input: string | URL | Request, init?: RequestInit) => {
      seen.push({
        path: String(input),
        method: init?.method ?? 'GET',
        body: typeof init?.body === 'string' ? JSON.parse(init.body) : undefined,
      });
      return {
        ok: true,
        status: 201,
        headers: new Headers(),
        json: async () => ({
          enrollment_id: 'e1',
          enrollment_token: 'tgw_enroll_abc',
          expires_at: '2026-10-01T00:15:00Z',
        }),
      } as Response;
    }) as typeof fetch;

    const result = await issueEnrollment(30, fetcher);
    expect(seen[0]).toEqual({
      path: '/api/v1/worker-enrollments',
      method: 'POST',
      body: { expires_in_minutes: 30 },
    });
    expect(result).toMatchObject({ kind: 'ok', data: { enrollment_token: 'tgw_enroll_abc' } });
  });

  it('passes a refusal through for a role that may not', async () => {
    const fetcher = (async () =>
      ({
        ok: false,
        status: 403,
        headers: new Headers(),
        json: async () => ({
          type: 'about:blank',
          title: 'Not permitted',
          status: 403,
          code: 'FORBIDDEN',
          detail: 'the operator role does not allow workers.manage',
        }),
      }) as Response) as typeof fetch;
    const result = await issueEnrollment(15, fetcher);
    expect(result).toMatchObject({ kind: 'error', problem: { code: 'FORBIDDEN' } });
  });
});
