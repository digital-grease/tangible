// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import { describe, expect, it } from 'vitest';
import {
  SETTABLE_CONDITIONS,
  conditionLabel,
  conditionMark,
  isEditable,
  loadDisc,
  loadDiscs,
  markDestroyed,
  methodLabel,
  recordCheck,
  updateDisc,
  type PhysicalCopyView,
} from './discs';

interface Recorded {
  path: string;
  method: string;
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
      body: typeof init?.body === 'string' ? JSON.parse(init.body) : undefined,
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

const copy = (overrides: Partial<PhysicalCopyView> = {}): PhysicalCopyView => ({
  id: 'copy-1',
  disc_id: 'disc-1',
  artifact_id: 'artifact-1',
  burn_attempt_id: 'attempt-1',
  status: 'verified',
  should_be_destroyed: false,
  is_settled: false,
  media_profile: 'bd-r-25',
  manufacturer_id: null,
  media_serial: null,
  label: null,
  storage_location: null,
  verification_level: 'full_sector_readback',
  verification_result: 'passed',
  notes: null,
  created_at: '2026-01-01T00:00:00Z',
  updated_at: '2026-01-01T00:00:00Z',
  last_checked_at: null,
  check_count: 0,
  ...overrides,
});

describe('loadDiscs', () => {
  it('distinguishes an empty inventory from a populated one', async () => {
    const empty = await loadDiscs(
      stubFetch({
        '/api/v1/physical-copies': { status: 200, body: { items: [], next_cursor: null } },
      }),
    );
    expect(empty.kind).toBe('empty');

    const populated = await loadDiscs(
      stubFetch({
        '/api/v1/physical-copies': { status: 200, body: { items: [copy()], next_cursor: null } },
      }),
    );
    expect(populated.kind).toBe('ready');
  });

  it('passes a condition filter through', async () => {
    const recorded: Recorded[] = [];
    await loadDiscs(
      stubFetch(
        {
          '/api/v1/physical-copies?status=degraded': {
            status: 200,
            body: { items: [copy({ status: 'degraded' })], next_cursor: null },
          },
        },
        recorded,
      ),
      { status: 'degraded' },
    );
    expect(recorded[0]?.path).toBe('/api/v1/physical-copies?status=degraded');
  });
});

describe('commands', () => {
  it('patches rather than replacing, so absent fields are left alone', async () => {
    const recorded: Recorded[] = [];
    await updateDisc(
      'copy-1',
      { label: 'Disc 1' },
      stubFetch(
        { '/api/v1/physical-copies/copy-1': { status: 200, body: { ...copy(), checks: [] } } },
        recorded,
      ),
    );
    expect(recorded[0]?.method).toBe('PATCH');
    expect(recorded[0]?.body).toEqual({ label: 'Disc 1' });
  });

  it('posts a check to its own route', async () => {
    const recorded: Recorded[] = [];
    await recordCheck(
      'copy-1',
      { method: 'full_sector_readback', result: 'passed' },
      stubFetch(
        {
          '/api/v1/physical-copies/copy-1/checks': {
            status: 200,
            body: { ...copy(), checks: [] },
          },
        },
        recorded,
      ),
    );
    expect(recorded[0]?.path).toBe('/api/v1/physical-copies/copy-1/checks');
    expect(recorded[0]?.method).toBe('POST');
  });

  it('reports a refused edit rather than throwing', async () => {
    const result = await updateDisc(
      'copy-1',
      { status: 'verified' },
      stubFetch({
        '/api/v1/physical-copies/copy-1': {
          status: 422,
          body: {
            type: 'about:blank',
            title: 'Request is not valid',
            status: 422,
            code: 'VALIDATION_FAILED',
            detail: 'record a check instead',
          },
        },
      }),
    );
    expect(result.kind).toBe('error');
    if (result.kind === 'error') expect(result.problem.code).toBe('VALIDATION_FAILED');
  });

  it('marks a disc destroyed through its own route', async () => {
    const recorded: Recorded[] = [];
    await markDestroyed(
      'copy-1',
      stubFetch(
        {
          '/api/v1/physical-copies/copy-1/mark-destroyed': {
            status: 200,
            body: { ...copy({ status: 'destroyed', is_settled: true }), checks: [] },
          },
        },
        recorded,
      ),
    );
    expect(recorded[0]?.path).toBe('/api/v1/physical-copies/copy-1/mark-destroyed');
  });

  it('encodes identifiers into the path', async () => {
    const recorded: Recorded[] = [];
    await loadDisc(
      'a/b',
      stubFetch(
        { '/api/v1/physical-copies/a%2Fb': { status: 200, body: { ...copy(), checks: [] } } },
        recorded,
      ),
    );
    expect(recorded[0]?.path).toBe('/api/v1/physical-copies/a%2Fb');
  });
});

describe('what the UI offers', () => {
  it('never offers a condition the server would refuse', () => {
    // `verified` means a disc was read back and matched. Offering it in a
    // dropdown would invite a refusal the operator cannot act on.
    expect(SETTABLE_CONDITIONS).not.toContain('verified');
    expect(SETTABLE_CONDITIONS).not.toContain('verification_failed');
    expect(SETTABLE_CONDITIONS).toContain('degraded');
    expect(SETTABLE_CONDITIONS).toContain('lost');
  });

  it('stops offering edits once a disc is destroyed', () => {
    expect(isEditable(copy())).toBe(true);
    expect(isEditable(copy({ status: 'destroyed', is_settled: true }))).toBe(false);
  });
});

describe('labels', () => {
  it('never softens a disc that was not verified', () => {
    // "Written, not verified" is not a gentler way of saying verified.
    expect(conditionLabel('produced_unverified')).toContain('not verified');
    expect(conditionLabel('verified')).toContain('read back and matched');
  });

  it('tells an operator what to do with a bad disc', () => {
    expect(conditionLabel('verification_failed')).toContain('destroy');
    expect(conditionLabel('degraded')).toContain('destroy');
  });

  it('passes an unknown condition through rather than inventing one', () => {
    expect(conditionLabel('melting')).toBe('melting');
    expect(methodLabel('sniffing')).toBe('sniffing');
  });

  it('marks every condition with a symbol, so status is never colour alone', () => {
    for (const status of ['verified', 'verification_failed', 'lost', 'destroyed', 'anything']) {
      expect(conditionMark(status).length).toBeGreaterThan(0);
    }
  });

  it('says what a check actually did', () => {
    expect(methodLabel('full_sector_readback')).toContain('whole disc');
    expect(methodLabel('tool_verify')).toContain('engine');
  });
});
