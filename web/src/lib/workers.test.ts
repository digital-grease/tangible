// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import { describe, expect, it } from 'vitest';
import {
  anyOpen,
  cancelErasure,
  changeWorker,
  credentialLabel,
  credentialTone,
  loadWorkers,
  workerActions,
  workerStatusLabel,
  workerStatusTone,
  connectionLabel,
  driveModel,
  erasureStateLabel,
  erasureStateTone,
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

  it('gives every state a tone, a refusal a warning rather than a failure', () => {
    expect(erasureStateTone('erased')).toBe('ok');
    expect(erasureStateTone('already_blank')).toBe('ok');
    expect(erasureStateTone('refused')).toBe('warn');
    expect(erasureStateTone('failed')).toBe('bad');
    expect(erasureStateTone('canceled')).toBe('idle');
    expect(erasureStateTone('queued')).toBe('busy');
    expect(erasureStateTone('erasing')).toBe('busy');
  });

  it('keeps checking only while something is still going', () => {
    const erasure = (is_open: boolean) => ({ is_open }) as ErasureView;
    expect(anyOpen([erasure(false), erasure(true)])).toBe(true);
    expect(anyOpen([erasure(false)])).toBe(false);
    expect(anyOpen([])).toBe(false);
  });
});

describe('drive descriptions', () => {
  it('names the drive with its firmware when it gave one', () => {
    expect(driveModel({ vendor: 'Slimtype', model: 'DVD A DS8A8SH', firmware: 'KS21' })).toBe(
      'Slimtype DVD A DS8A8SH, firmware KS21',
    );
    expect(driveModel({ vendor: 'PIONEER', model: null, firmware: null })).toBe('PIONEER');
    expect(driveModel({ vendor: null, model: null, firmware: 'KS21' })).toBe('Firmware KS21');
    expect(driveModel({ vendor: null, model: null, firmware: null })).toBe('Not reported');
  });

  it('says how a drive is attached in words', () => {
    expect(connectionLabel('usb')).toBe('USB');
    expect(connectionLabel('sata')).toBe('SATA');
    expect(connectionLabel('unknown')).toBe('Not known');
    expect(connectionLabel('thunderbolt')).toBe('Not known');
  });
});

describe('managing workers', () => {
  const recording = (status: number, body: unknown) => {
    const seen: { path: string; method: string }[] = [];
    const fetcher = (async (input: string | URL | Request, init?: RequestInit) => {
      seen.push({ path: String(input), method: init?.method ?? 'GET' });
      return {
        ok: status < 400,
        status,
        headers: new Headers(),
        json: async () => body,
      } as Response;
    }) as typeof fetch;
    return { seen, fetcher };
  };

  it('lists workers and changes one by its action', async () => {
    const list = recording(200, { items: [], next_cursor: null });
    await loadWorkers(list.fetcher);
    expect(list.seen[0]).toEqual({ path: '/api/v1/workers?limit=200', method: 'GET' });

    for (const action of ['drain', 'enable', 'revoke'] as const) {
      const change = recording(200, { id: 'w 1', status: 'draining' });
      const result = await changeWorker('w 1', action, change.fetcher);
      expect(change.seen[0]).toEqual({ path: `/api/v1/workers/w%201/${action}`, method: 'POST' });
      expect(result.kind).toBe('ok');
    }
  });

  it('says each status in words and offers only what the server allows', () => {
    expect(workerStatusLabel('draining')).toContain('taking nothing new');
    expect(workerStatusLabel('revoked')).toContain('refused');
    expect(workerStatusLabel('something-new')).toBe('something-new');
    expect(workerActions('online')).toEqual(['drain', 'rotate-credential', 'revoke']);
    expect(workerActions('pending')).toEqual(['drain', 'rotate-credential', 'revoke']);
    expect(workerActions('draining')).toEqual(['enable', 'rotate-credential', 'revoke']);
    expect(workerActions('incompatible')).toEqual(['revoke']);
    expect(workerActions('revoked')).toEqual([]);
  });

  it('gives each status a tone, a revoked worker retired rather than failing', () => {
    expect(workerStatusTone('online')).toBe('ok');
    expect(workerStatusTone('offline')).toBe('warn');
    expect(workerStatusTone('draining')).toBe('warn');
    expect(workerStatusTone('incompatible')).toBe('bad');
    expect(workerStatusTone('pending')).toBe('idle');
    expect(workerStatusTone('revoked')).toBe('idle');
  });

  it('does not offer a rotation already asked for', () => {
    expect(workerActions('online', true)).toEqual(['drain', 'revoke']);
    expect(workerActions('draining', true)).toEqual(['enable', 'revoke']);
  });
});

describe('a worker credential in words', () => {
  const now = new Date('2026-11-02T12:00:00Z');
  const base = {
    status: 'online',
    credential_issued_at: '2026-10-01T12:00:00Z',
    credential_rotation_requested_at: null,
    credential_rotation_due: false,
  };

  it('says how old it is', () => {
    expect(credentialLabel(base, now)).toBe('Issued 32 days ago');
    expect(credentialLabel({ ...base, credential_issued_at: '2026-11-01T11:00:00Z' }, now)).toBe(
      'Issued yesterday',
    );
    expect(credentialLabel({ ...base, credential_issued_at: '2026-11-02T08:00:00Z' }, now)).toBe(
      'Issued today',
    );
  });

  it('says when a rotation is asked for or due', () => {
    expect(
      credentialLabel({ ...base, credential_rotation_requested_at: '2026-11-02T10:00:00Z' }, now),
    ).toMatch(/^Rotation asked for/);
    expect(credentialLabel({ ...base, credential_rotation_due: true }, now)).toMatch(
      /^Due for rotation/,
    );
  });

  it("calls a revoked worker's credential refused", () => {
    expect(credentialLabel({ ...base, status: 'revoked' }, now)).toBe('Refused');
  });

  it('marks only a credential with something to say', () => {
    expect(credentialTone(base)).toBeNull();
    expect(credentialTone({ ...base, credential_rotation_due: true })).toBe('warn');
    expect(
      credentialTone({ ...base, credential_rotation_requested_at: '2026-11-02T10:00:00Z' }),
    ).toBe('busy');
    expect(credentialTone({ ...base, status: 'revoked' })).toBe('idle');
  });
});
