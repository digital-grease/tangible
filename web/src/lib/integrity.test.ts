// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
import { describe, expect, it, vi } from 'vitest';
import {
  checkPercent,
  checkProgress,
  checkStateLabel,
  checkStateTone,
  graceLabel,
  integrityLabel,
  isActive,
  loadGcPlan,
  problemKindLabel,
  requestScrub,
  scopeLabel,
  tallyLabel,
  validateArtifact,
  type IntegrityCheck,
} from './integrity';

function check(extra: Partial<IntegrityCheck> = {}): IntegrityCheck {
  return {
    id: 'c1',
    scope: 'artifact',
    artifact_id: 'a1',
    state: 'running',
    objects_total: 4,
    bytes_total: 4096,
    objects_checked: 1,
    bytes_checked: 1024,
    problems: [],
    problem_count: 0,
    created_by: 'alice',
    created_at: '2026-10-25T12:00:00Z',
    ...extra,
  };
}

function recording() {
  const calls: { url: string; method: string; body: unknown }[] = [];
  const fetcher = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
    calls.push({
      url: String(input),
      method: init?.method ?? 'GET',
      body: init?.body ? JSON.parse(String(init.body)) : undefined,
    });
    return {
      ok: true,
      status: 200,
      json: async () => check(),
      headers: new Headers(),
    } as Response;
  }) as unknown as typeof fetch;
  return { calls, fetcher };
}

describe('integrity', () => {
  it('asks the routes the server answers', async () => {
    const { calls, fetcher } = recording();
    await validateArtifact('a/1', fetcher);
    await requestScrub(fetcher);
    await loadGcPlan(0, fetcher);
    expect(calls).toEqual([
      { url: '/api/v1/artifacts/a%2F1/validate', method: 'POST', body: undefined },
      { url: '/api/v1/integrity-checks', method: 'POST', body: { scope: 'all' } },
      { url: '/api/v1/storage/gc-plan?grace_days=0&limit=100', method: 'GET', body: undefined },
    ]);
  });

  it('says an artifact’s state in words and a mark', () => {
    expect(integrityLabel('damaged')).toMatch(/^Damaged/);
    expect(integrityLabel('intact')).toMatch(/^Intact/);
    expect(integrityLabel('unchecked')).toBe('Not checked yet');
  });

  it('says how a check stands and how far it has got', () => {
    expect(isActive(check({ state: 'queued' }))).toBe(true);
    expect(isActive(check({ state: 'complete' }))).toBe(false);
    expect(checkStateLabel(check({ state: 'complete' }))).toBe('Done: nothing wrong');
    expect(checkStateLabel(check({ state: 'complete', problem_count: 2 }))).toBe(
      'Done: 2 problems',
    );
    expect(checkStateLabel(check({ state: 'canceled' }))).toBe('Withdrawn');
    expect(checkStateTone(check({ state: 'running' }))).toBe('busy');
    expect(checkStateTone(check({ state: 'complete' }))).toBe('ok');
    expect(checkStateTone(check({ state: 'complete', problem_count: 2 }))).toBe('bad');
    expect(checkStateTone(check({ state: 'failed' }))).toBe('bad');
    expect(checkStateTone(check({ state: 'canceled' }))).toBe('idle');
    expect(checkProgress(check())).toBe('1 of 4 files (1.0 KiB of 4.0 KiB)');
    expect(checkProgress(check({ state: 'queued', objects_total: null, bytes_total: null }))).toBe(
      'Not started',
    );
    expect(checkPercent(check())).toBe(25);
    expect(checkPercent(check({ bytes_total: 0 }))).toBe(100);
    expect(checkPercent(check({ bytes_total: null }))).toBeNull();
    expect(scopeLabel(check({ scope: 'all' }))).toBe('Whole library');
  });

  it('names problems and plans in words', () => {
    expect(problemKindLabel('corrupt')).toBe('Reads back as other bytes');
    expect(problemKindLabel('something_new')).toBe('something_new');
    expect(tallyLabel({ count: 1, bytes: 2048 })).toBe('1 file (2.0 KiB)');
    expect(tallyLabel({ count: 0, bytes: 0 })).toBe('0 files (0 B)');
    expect(graceLabel({ grace_seconds: 7 * 86_400 })).toBe('the 7-day grace period');
    expect(graceLabel({ grace_seconds: 0 })).toBe('no grace period');
  });
});
