// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import { describe, expect, it } from 'vitest';
import {
  attemptOutcomeLabel,
  canCancel,
  canRetry,
  cancelBurnJob,
  createBurnJob,
  isLive,
  isSettled,
  jobStateLabel,
  jobStateMark,
  loadBurnJob,
  loadBurnQueue,
  newIdempotencyKey,
  progressPercent,
  progressSentence,
  resolveAttention,
  retryBurnJob,
  stageLabel,
  type BurnJobView,
} from './burns';

interface Recorded {
  path: string;
  method: string;
  headers: Record<string, string>;
  body: unknown;
}

function stubFetch(
  routes: Record<string, { status: number; body: unknown }>,
  recorded: Recorded[] = [],
): typeof fetch {
  return (async (input: string | URL | Request, init?: RequestInit) => {
    const path = typeof input === 'string' ? input : input.toString();
    recorded.push({
      path,
      method: init?.method ?? 'GET',
      headers: (init?.headers ?? {}) as Record<string, string>,
      body: init?.body ? JSON.parse(init.body as string) : undefined,
    });
    const route = routes[path];
    if (!route) throw new Error(`unexpected request to ${path}`);
    return {
      ok: route.status >= 200 && route.status < 300,
      status: route.status,
      json: async () => route.body,
    } as Response;
  }) as typeof fetch;
}

const job = (overrides: Partial<BurnJobView> = {}): BurnJobView => ({
  id: 'job-1',
  disc_id: 'disc-1',
  artifact_id: 'artifact-1',
  state: 'queued',
  has_started_writing: false,
  priority: 0,
  verification_policy: ['full_sector_readback'],
  eject_policy: 'eject_on_success',
  requested_media_profile: null,
  created_by: 'operator',
  created_at: '2026-01-01T00:00:00Z',
  updated_at: '2026-01-01T00:00:00Z',
  completed_at: null,
  attempt_count: 0,
  progress: null,
  ...overrides,
});

describe('loadBurnQueue', () => {
  it('distinguishes an empty queue from a populated one', async () => {
    // An empty queue needs its own copy telling the operator what to do next,
    // so it cannot be a zero-length success.
    const empty = await loadBurnQueue(
      stubFetch({ '/api/v1/burn-jobs': { status: 200, body: { items: [], next_cursor: null } } }),
    );
    expect(empty.kind).toBe('empty');

    const populated = await loadBurnQueue(
      stubFetch({
        '/api/v1/burn-jobs': { status: 200, body: { items: [job()], next_cursor: null } },
      }),
    );
    expect(populated.kind).toBe('ready');
  });

  it('surfaces a problem document as an error', async () => {
    const view = await loadBurnQueue(
      stubFetch({
        '/api/v1/burn-jobs': {
          status: 503,
          body: { type: 'about:blank', title: 't', status: 503, code: 'X', detail: 'd' },
        },
      }),
    );
    expect(view).toEqual({
      kind: 'error',
      problem: { type: 'about:blank', title: 't', status: 503, code: 'X', detail: 'd' },
    });
  });

  it('wraps a non-problem error body rather than showing two error shapes', async () => {
    const view = await loadBurnQueue(
      stubFetch({ '/api/v1/burn-jobs': { status: 500, body: { oops: true } } }),
    );
    expect(view.kind).toBe('error');
    if (view.kind === 'error') expect(view.problem.code).toBe('TRANSPORT_FAILURE');
  });
});

describe('createBurnJob', () => {
  it('always sends an idempotency key', async () => {
    // The one request in the product that spends a physical disc. A retry
    // without a key burns a second one.
    const recorded: Recorded[] = [];
    await createBurnJob(
      { artifact_id: 'a', disc_id: 'd' },
      'key-1',
      stubFetch(
        { '/api/v1/burn-jobs': { status: 201, body: { ...job(), warnings: [] } } },
        recorded,
      ),
    );

    expect(recorded[0]?.method).toBe('POST');
    expect(recorded[0]?.headers['idempotency-key']).toBe('key-1');
    expect(recorded[0]?.body).toEqual({ artifact_id: 'a', disc_id: 'd' });
  });

  it('returns the refusal rather than throwing', async () => {
    const result = await createBurnJob(
      { artifact_id: 'a', disc_id: 'd' },
      'key-1',
      stubFetch({
        '/api/v1/burn-jobs': {
          status: 422,
          body: {
            type: 'about:blank',
            title: 'Request is not valid',
            status: 422,
            code: 'VALIDATION_FAILED',
            detail: 'quarantined',
          },
        },
      }),
    );
    expect(result.kind).toBe('error');
    if (result.kind === 'error') expect(result.problem.code).toBe('VALIDATION_FAILED');
  });
});

describe('newIdempotencyKey', () => {
  it('never returns the same key twice', () => {
    // A constant would make every burn a replay of the first, which is the
    // failure that would be hardest to notice.
    const keys = new Set(Array.from({ length: 50 }, () => newIdempotencyKey()));
    expect(keys.size).toBe(50);
  });
});

describe('commands', () => {
  it('posts to the cancel route and returns the updated job', async () => {
    const recorded: Recorded[] = [];
    const result = await cancelBurnJob(
      'job-1',
      stubFetch(
        {
          '/api/v1/burn-jobs/job-1/cancel': {
            status: 200,
            body: { ...job({ state: 'canceled' }), attempts: [] },
          },
        },
        recorded,
      ),
    );
    expect(recorded[0]?.method).toBe('POST');
    expect(result.kind).toBe('ready');
    if (result.kind === 'ready') expect(result.data.state).toBe('canceled');
  });

  it('reports a refused cancellation as a problem, not a page failure', async () => {
    // The server declined because honouring it would ruin a disc. That is an
    // answer the operator needs to read, not an error to retry.
    const result = await cancelBurnJob(
      'job-1',
      stubFetch({
        '/api/v1/burn-jobs/job-1/cancel': {
          status: 409,
          body: {
            type: 'about:blank',
            title: 'A write is already in progress',
            status: 409,
            code: 'WRITE_IN_PROGRESS',
            detail: 'the attempt will finish',
          },
        },
      }),
    );
    expect(result.kind).toBe('error');
    if (result.kind === 'error') expect(result.problem.code).toBe('WRITE_IN_PROGRESS');
  });

  it('posts to the resolve-attention route', async () => {
    // The only way out of needs_attention, and a separate action from
    // retrying on purpose.
    const recorded: Recorded[] = [];
    await resolveAttention(
      'job-1',
      stubFetch(
        {
          '/api/v1/burn-jobs/job-1/resolve-attention': {
            status: 200,
            body: { ...job({ state: 'failed' }), attempts: [] },
          },
        },
        recorded,
      ),
    );
    expect(recorded[0]?.path).toBe('/api/v1/burn-jobs/job-1/resolve-attention');
    expect(recorded[0]?.method).toBe('POST');
  });

  it('posts to the retry route', async () => {
    const recorded: Recorded[] = [];
    await retryBurnJob(
      'job-1',
      stubFetch(
        {
          '/api/v1/burn-jobs/job-1/retry': {
            status: 200,
            body: { ...job({ state: 'queued' }), attempts: [] },
          },
        },
        recorded,
      ),
    );
    expect(recorded[0]?.path).toBe('/api/v1/burn-jobs/job-1/retry');
    expect(recorded[0]?.method).toBe('POST');
  });

  it('encodes identifiers into the path', async () => {
    const recorded: Recorded[] = [];
    await loadBurnJob(
      'a/b',
      stubFetch(
        { '/api/v1/burn-jobs/a%2Fb': { status: 200, body: { ...job(), attempts: [] } } },
        recorded,
      ),
    );
    expect(recorded[0]?.path).toBe('/api/v1/burn-jobs/a%2Fb');
  });
});

describe('offering cancellation', () => {
  it('is offered while nothing physical has happened', () => {
    for (const state of ['queued', 'leased', 'staging', 'waiting_for_media', 'preflighting']) {
      expect(canCancel(job({ state }))).toBe(true);
    }
  });

  it('is withdrawn once a disc is being consumed', () => {
    // Not merely disabled: a control that cannot be honoured should not be
    // presented as an option.
    expect(canCancel(job({ state: 'writing', has_started_writing: true }))).toBe(false);
    expect(canCancel(job({ state: 'verifying', has_started_writing: true }))).toBe(false);
  });

  it('is withdrawn once the job has finished', () => {
    for (const state of ['complete', 'failed', 'canceled', 'needs_attention']) {
      expect(canCancel(job({ state }))).toBe(false);
    }
  });

  it('trusts the server rather than recomputing which states write', () => {
    // The flag comes from the domain. Deriving it in the browser would put a
    // second, drifting copy of a safety rule in the client.
    expect(canCancel(job({ state: 'queued', has_started_writing: true }))).toBe(false);
  });
});

describe('offering a retry', () => {
  it('is offered for failed and cancelled burns', () => {
    expect(canRetry(job({ state: 'failed' }))).toBe(true);
    expect(canRetry(job({ state: 'canceled' }))).toBe(true);
  });

  it('is not offered for a finished or unsettled burn', () => {
    // A completed burn is repeated by queueing a new job; one needing
    // attention has a disc unaccounted for.
    for (const state of ['complete', 'needs_attention', 'writing', 'queued']) {
      expect(canRetry(job({ state }))).toBe(false);
    }
  });
});

describe('labels', () => {
  it('never claims a written disc was verified', () => {
    // The distinction the whole design rests on: a successful tool exit is
    // not verified media.
    expect(attemptOutcomeLabel('written')).toContain('not verified');
    expect(attemptOutcomeLabel('verified')).toContain('matched');
  });

  it('says a failed verification produced a disc that exists', () => {
    // It has to be findable and destroyed, so the label cannot read like a
    // burn that never happened.
    expect(attemptOutcomeLabel('verification_failed')).toContain('exists');
    expect(attemptOutcomeLabel('failed_before_write')).toContain('no disc');
  });

  it('translates states into what an operator should do', () => {
    expect(jobStateLabel('waiting_for_media')).toBe('Waiting for a disc');
    expect(stageLabel('waiting_for_media')).toBe('Waiting for a disc');
  });

  it('passes an unknown state through rather than inventing one', () => {
    // A server from a later release must not be mistranslated into something
    // reassuring.
    expect(jobStateLabel('polishing')).toBe('polishing');
    expect(stageLabel('polishing')).toBe('polishing');
    expect(attemptOutcomeLabel('polishing')).toBe('polishing');
  });

  it('marks every state with a symbol, so status is never colour alone', () => {
    for (const state of ['queued', 'writing', 'complete', 'failed', 'canceled', 'anything']) {
      expect(jobStateMark(state).length).toBeGreaterThan(0);
    }
  });
});

describe('progress', () => {
  it('reports null when the stage does not measure progress', () => {
    expect(progressPercent(null)).toBeNull();
    expect(progressPercent(undefined)).toBeNull();
  });

  it('clamps a nonsensical fraction rather than rendering it', () => {
    expect(progressPercent(1.4)).toBe(100);
    expect(progressPercent(-1)).toBe(0);
    expect(progressPercent(0.415)).toBe(42);
  });

  it('announces the state alone when nothing has been reported', () => {
    expect(progressSentence(job({ state: 'queued' }))).toBe('Queued');
  });

  it('announces state, stage and percentage together', () => {
    const sentence = progressSentence(
      job({
        state: 'writing',
        has_started_writing: true,
        progress: { stage: 'writing', fraction: 0.5, code: 'WRITE_PROGRESS', observed_at: '' },
      }),
    );
    expect(sentence).toBe('Writing: Writing, 50%');
  });
});

describe('settled and live', () => {
  it('treats the four terminal states as settled', () => {
    for (const state of ['complete', 'failed', 'canceled', 'needs_attention']) {
      expect(isSettled(state)).toBe(true);
      expect(isLive(state)).toBe(false);
    }
  });

  it('treats everything else as live', () => {
    for (const state of ['queued', 'leased', 'writing', 'verifying']) {
      expect(isLive(state)).toBe(true);
    }
  });
});
