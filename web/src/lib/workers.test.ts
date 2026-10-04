// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import { describe, expect, it } from 'vitest';
import {
  anyOpen,
  cancelErasure,
  erasureStateLabel,
  issueEnrollment,
  requestErasure,
  type ErasureView,
} from './workers';

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

describe('erasing a disc', () => {
  const recorder = (status: number, body: unknown) => {
    const seen: { path: string; method: string; body: unknown }[] = [];
    const fetcher = (async (input: string | URL | Request, init?: RequestInit) => {
      seen.push({
        path: String(input),
        method: init?.method ?? 'GET',
        body: typeof init?.body === 'string' ? JSON.parse(init.body) : undefined,
      });
      return {
        ok: status >= 200 && status < 300,
        status,
        headers: new Headers(),
        json: async () => body,
      } as Response;
    }) as typeof fetch;
    return { seen, fetcher };
  };

  it('sends the confirmation exactly as the person gave it', async () => {
    const { seen, fetcher } = recorder(422, {
      type: 'about:blank',
      title: 'Request is not valid',
      status: 422,
      code: 'VALIDATION_FAILED',
      detail: 'set confirm_data_loss',
    });
    const result = await requestErasure('drive/1', 'quick', false, fetcher);
    expect(seen[0]).toEqual({
      path: '/api/v1/drives/drive%2F1/erasures',
      method: 'POST',
      body: { mode: 'quick', confirm_data_loss: false },
    });
    expect(result.kind).toBe('error');
  });

  it('withdraws by identifier', async () => {
    const { seen, fetcher } = recorder(200, { id: 'e1', state: 'canceled' });
    await cancelErasure('e1', fetcher);
    expect(seen[0]).toMatchObject({ path: '/api/v1/erasures/e1/cancel', method: 'POST' });
  });

  it('describes every state in words', () => {
    for (const state of [
      'queued',
      'erasing',
      'erased',
      'already_blank',
      'refused',
      'failed',
      'canceled',
    ]) {
      expect(erasureStateLabel(state)).not.toBe(state);
    }
  });

  it('keeps checking only while something is still going', () => {
    const erasure = (is_open: boolean) => ({ is_open }) as ErasureView;
    expect(anyOpen([erasure(false), erasure(true)])).toBe(true);
    expect(anyOpen([erasure(false)])).toBe(false);
    expect(anyOpen([])).toBe(false);
  });
});
