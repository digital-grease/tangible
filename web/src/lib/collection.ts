// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// Collecting orphaned files (ADR 0024, docs/plans/gc-sweep.md G3): a plan
// kept with every candidate, collected on request, and what became of each.
// What goes is at most what the kept plan listed; the server checks each
// candidate again before it removes it.

import type { components } from './api-types';
import type { Tone } from './components/ui/tone';
import type { GcPlan } from './integrity';
import { apiCall, apiFetch, type Result } from './session';

export type KeptPlan = components['schemas']['StoredGcPlanView'];
export type KeptPlanDetail = components['schemas']['GcPlanDetail'];
export type KeptPlanObject = components['schemas']['GcPlanObjectView'];
export type FailedImportStaging = components['schemas']['FailedImportStaging'];
export type FailedImportArea = components['schemas']['FailedImportArea'];

/** The shortest grace period a plan to be collected may have, in days. */
export const MIN_COLLECT_GRACE_DAYS = 1;

/** Make a plan and keep it, to be collected within the hour. */
export function keepPlan(
  graceDays: number,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<KeptPlanDetail>> {
  return apiCall(fetcher, '/api/v1/storage/gc-plans', 'POST', { grace_days: graceDays });
}

/** A kept plan, with its largest candidates and, once collected, their outcomes. */
export function loadKeptPlan(
  planId: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<KeptPlanDetail>> {
  return apiCall(fetcher, `/api/v1/storage/gc-plans/${encodeURIComponent(planId)}?limit=100`);
}

/** Ask for a kept plan to be collected. The server does it shortly. */
export function collectPlan(
  planId: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<KeptPlan>> {
  return apiCall(fetcher, `/api/v1/storage/gc-plans/${encodeURIComponent(planId)}/collect`, 'POST');
}

/** The plans kept, newest first. */
export function loadKeptPlans(
  fetcher: typeof fetch = apiFetch,
  limit = 20,
): Promise<Result<{ items: KeptPlan[] }>> {
  return apiCall(fetcher, `/api/v1/storage/gc-plans?limit=${limit}`);
}

/**
 * Whether a dry-run plan can lead to a collection: every mark was read, it
 * kept a day's grace at least, and there is something to remove.
 */
export function canCollect(plan: GcPlan): boolean {
  return (
    plan.certain &&
    plan.grace_seconds >= MIN_COLLECT_GRACE_DAYS * 86_400 &&
    (plan.collectable.count > 0 || plan.stale_partial_files > 0)
  );
}

/** Whether a kept plan is a collection worth listing: asked for, or the schedule's. */
export function isCollection(plan: KeptPlan): boolean {
  return plan.collect_requested_at != null || plan.planned_by == null;
}

/** Who made a plan: an administrator, or the schedule. */
export function plannedBy(plan: Pick<KeptPlan, 'planned_by'>): string {
  return plan.planned_by ?? 'The schedule';
}

/** A plan's grace period as a span of days: `7 days`, `1 day`. */
export function graceDays(plan: Pick<KeptPlan, 'grace_seconds'>): string {
  const days = Math.floor(plan.grace_seconds / 86_400);
  return `${days} day${days === 1 ? '' : 's'}`;
}

/** How a kept plan stands, in words. */
export function keptPlanLabel(plan: Pick<KeptPlan, 'state'>): string {
  switch (plan.state) {
    case 'planned':
      return 'Planned, not collected';
    case 'expired':
      return 'Expired, not collected';
    case 'uncertain':
      return 'Uncertain: not collected';
    case 'queued':
      return 'Waiting to be collected';
    case 'collected':
      return 'Collected';
    default:
      return plan.state;
  }
}

/** How a kept plan stands, as a tone beside its words. */
export function keptPlanTone(plan: Pick<KeptPlan, 'state'>): Tone {
  switch (plan.state) {
    case 'queued':
      return 'busy';
    case 'collected':
      return 'ok';
    case 'uncertain':
      return 'warn';
    default:
      return 'idle';
  }
}

/** What collecting did with one file, in words. */
export function outcomeLabel(outcome: string | null | undefined): string {
  switch (outcome) {
    case 'removed':
      return 'Removed';
    case 'kept_live':
      return 'Kept: something refers to it now';
    case 'kept_held':
      return 'Kept: counted, or under a retention lock';
    case 'kept_changed':
      return 'Kept: its file changed since the plan';
    case 'kept_gone':
      return 'Kept: it was gone already';
    case 'kept_uncertain':
      return 'Kept: not every manifest could be read';
    case 'kept_failed':
      return 'Kept: removing it failed';
    case null:
    case undefined:
      return 'Not collected yet';
    default:
      return outcome;
  }
}

/** Removed, kept, or not yet, as a tone. */
export function outcomeTone(outcome: string | null | undefined): Tone {
  if (outcome === 'removed') return 'ok';
  if (outcome === 'kept_failed') return 'bad';
  if (outcome == null) return 'idle';
  return 'warn';
}

/** How an ended import ended, in a word. */
export function endedLabel(state: string): string {
  switch (state) {
    case 'failed_terminal':
      return 'failed';
    case 'canceled':
      return 'canceled';
    default:
      return state;
  }
}

/** Where an orphan came from: the import that stored it and ended, or nobody known. */
export function originLabel(object: Pick<KeptPlanObject, 'import'>): string {
  const origin = object.import;
  if (!origin) return 'Not recorded by any import';
  const name = origin.name ?? 'An import';
  const how = origin.error_code
    ? `${endedLabel(origin.state)}: ${origin.error_code}`
    : endedLabel(origin.state);
  return `${name} (${how})`;
}
