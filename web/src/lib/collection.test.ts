// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
import { describe, expect, it, vi } from 'vitest';
import {
  canCollect,
  collectPlan,
  graceDays,
  isCollection,
  keepPlan,
  keptPlanLabel,
  keptPlanTone,
  loadKeptPlan,
  loadKeptPlans,
  originLabel,
  outcomeLabel,
  outcomeTone,
  plannedBy,
  type KeptPlan,
} from './collection';
import type { GcPlan } from './integrity';

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
      json: async () => ({}),
      headers: new Headers(),
    } as Response;
  }) as unknown as typeof fetch;
  return { calls, fetcher };
}

function dryRun(extra: Partial<GcPlan> = {}): GcPlan {
  const none = { count: 0, bytes: 0 };
  return {
    planned_at: '2026-11-17T12:00:00Z',
    grace_seconds: 7 * 86_400,
    stored: { count: 3, bytes: 3072 },
    live: { count: 1, bytes: 1024 },
    recent: none,
    collectable: { count: 2, bytes: 2048 },
    candidates: [],
    partial_files: none,
    stale_partial_files: 0,
    unrecognized: none,
    missing: none,
    missing_digests: [],
    manifests_unreadable: 0,
    checkpoints_unreadable: 0,
    certain: true,
    failed_import_staging: null,
    ...extra,
  };
}

function kept(extra: Partial<KeptPlan> = {}): KeptPlan {
  return {
    id: 'p1',
    planned_at: '2026-11-17T12:00:00Z',
    planned_by: 'alice',
    grace_seconds: 7 * 86_400,
    certain: true,
    candidates: { count: 2, bytes: 2048 },
    stale_partial_files: 0,
    state: 'planned',
    expires_at: '2026-11-17T13:00:00Z',
    collect_requested_at: null,
    collect_requested_by: null,
    collected: null,
    ...extra,
  };
}

describe('collection', () => {
  it('asks the routes the server answers', async () => {
    const { calls, fetcher } = recording();
    await keepPlan(7, fetcher);
    await loadKeptPlan('p/1', fetcher);
    await collectPlan('p/1', fetcher);
    await loadKeptPlans(fetcher);
    expect(calls).toEqual([
      { url: '/api/v1/storage/gc-plans', method: 'POST', body: { grace_days: 7 } },
      { url: '/api/v1/storage/gc-plans/p%2F1?limit=100', method: 'GET', body: undefined },
      { url: '/api/v1/storage/gc-plans/p%2F1/collect', method: 'POST', body: undefined },
      { url: '/api/v1/storage/gc-plans?limit=20', method: 'GET', body: undefined },
    ]);
  });

  it('offers to collect only a certain plan, with a day of grace, and something to remove', () => {
    expect(canCollect(dryRun())).toBe(true);
    expect(canCollect(dryRun({ certain: false }))).toBe(false);
    expect(canCollect(dryRun({ grace_seconds: 86_399 }))).toBe(false);
    expect(canCollect(dryRun({ grace_seconds: 86_400 }))).toBe(true);
    const nothing = { count: 0, bytes: 0 };
    expect(canCollect(dryRun({ collectable: nothing }))).toBe(false);
    // Stale partial files alone are worth collecting.
    expect(canCollect(dryRun({ collectable: nothing, stale_partial_files: 2 }))).toBe(true);
  });

  it('lists a plan as a collection once asked for, or when the schedule made it', () => {
    expect(isCollection(kept())).toBe(false);
    expect(isCollection(kept({ collect_requested_at: '2026-11-17T12:01:00Z' }))).toBe(true);
    expect(isCollection(kept({ planned_by: null, state: 'uncertain' }))).toBe(true);
    expect(plannedBy(kept())).toBe('alice');
    expect(plannedBy(kept({ planned_by: null }))).toBe('The schedule');
  });

  it('says how a plan stands and what became of each file, in words and a tone', () => {
    expect(keptPlanLabel(kept({ state: 'queued' }))).toBe('Waiting to be collected');
    expect(keptPlanTone(kept({ state: 'queued' }))).toBe('busy');
    expect(keptPlanLabel(kept({ state: 'uncertain' }))).toBe('Uncertain: not collected');
    expect(keptPlanTone(kept({ state: 'uncertain' }))).toBe('warn');
    expect(keptPlanTone(kept({ state: 'collected' }))).toBe('ok');
    expect(keptPlanLabel(kept({ state: 'expired' }))).toBe('Expired, not collected');
    expect(outcomeLabel('removed')).toBe('Removed');
    expect(outcomeTone('removed')).toBe('ok');
    expect(outcomeLabel('kept_changed')).toBe('Kept: its file changed since the plan');
    expect(outcomeTone('kept_changed')).toBe('warn');
    expect(outcomeTone('kept_failed')).toBe('bad');
    expect(outcomeLabel(null)).toBe('Not collected yet');
    expect(outcomeTone(null)).toBe('idle');
    expect(graceDays(kept())).toBe('7 days');
    expect(graceDays(kept({ grace_seconds: 86_400 }))).toBe('1 day');
  });

  it('says where an orphan came from', () => {
    const origin = {
      import_id: 'i1',
      state: 'failed_terminal',
      error_code: 'IMPORT_DESCRIPTOR_INVALID',
      name: 'Hackers.zip',
      ended_at: '2026-11-14T12:00:00Z',
    };
    expect(originLabel({ import: origin })).toBe('Hackers.zip (failed: IMPORT_DESCRIPTOR_INVALID)');
    expect(
      originLabel({ import: { ...origin, state: 'canceled', error_code: null, name: null } }),
    ).toBe('An import (canceled)');
    expect(originLabel({ import: null })).toBe('Not recorded by any import');
  });
});
