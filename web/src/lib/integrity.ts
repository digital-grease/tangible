// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// Integrity checks and the garbage collection plan
// (docs/plans/storage-integrity.md): reading stored bytes back against their
// digests, and saying what nothing refers to. Nothing here repairs or
// removes anything; the server cannot do either.

import type { components } from './api-types';
import { formatBytes } from './library';
import { apiCall, apiFetch, type Result } from './session';

export type IntegrityCheck = components['schemas']['IntegrityCheckView'];
export type IntegrityProblem = components['schemas']['IntegrityProblemView'];
export type IntegrityCheckPage = components['schemas']['IntegrityCheckPage'];
export type GcPlan = components['schemas']['GcPlan'];
export type Tally = components['schemas']['Tally'];

/** How often a check under way is looked at again, in milliseconds. */
export const FOLLOW_EVERY_MS = 3000;

/** Check one artifact. Answers a check already under way rather than queue a second. */
export function validateArtifact(
  artifactId: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<IntegrityCheck>> {
  return apiCall(fetcher, `/api/v1/artifacts/${encodeURIComponent(artifactId)}/validate`, 'POST');
}

/** Check the whole library: every stored byte. */
export function requestScrub(fetcher: typeof fetch = apiFetch): Promise<Result<IntegrityCheck>> {
  return apiCall(fetcher, '/api/v1/integrity-checks', 'POST', { scope: 'all' });
}

/** Checks, newest first. */
export function loadChecks(
  fetcher: typeof fetch = apiFetch,
  limit = 20,
): Promise<Result<IntegrityCheckPage>> {
  return apiCall(fetcher, `/api/v1/integrity-checks?limit=${limit}`);
}

/** One check, and how far it has got. */
export function loadCheck(
  checkId: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<IntegrityCheck>> {
  return apiCall(fetcher, `/api/v1/integrity-checks/${encodeURIComponent(checkId)}`);
}

/** Withdraw a check. */
export function cancelCheck(
  checkId: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<IntegrityCheck>> {
  return apiCall(fetcher, `/api/v1/integrity-checks/${encodeURIComponent(checkId)}/cancel`, 'POST');
}

/** What garbage collection would remove now. Walks the store, so asked for, not loaded. */
export function loadGcPlan(
  graceDays: number,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<GcPlan>> {
  return apiCall(fetcher, `/api/v1/storage/gc-plan?grace_days=${graceDays}&limit=100`);
}

/** Whether a check is still going. */
export function isActive(check: Pick<IntegrityCheck, 'state'>): boolean {
  return check.state === 'queued' || check.state === 'running';
}

/** An artifact's integrity, in words. Never colour alone. */
export function integrityLabel(state: string): string {
  switch (state) {
    case 'intact':
      return 'Intact: every stored file read back as it was stored';
    case 'damaged':
      return 'Damaged: a stored file is missing or reads back wrong';
    case 'unchecked':
      return 'Not checked yet';
    default:
      return state;
  }
}

/** A mark beside the words, so the state is never colour alone. */
export function integrityMark(state: string): string {
  switch (state) {
    case 'intact':
      return '✓';
    case 'damaged':
      return '✕';
    default:
      return '·';
  }
}

/** How a check stands, in words. */
export function checkStateLabel(check: Pick<IntegrityCheck, 'state' | 'problem_count'>): string {
  switch (check.state) {
    case 'queued':
      return 'Waiting for its turn';
    case 'running':
      return 'Reading';
    case 'complete':
      return check.problem_count === 0
        ? 'Done: nothing wrong'
        : `Done: ${check.problem_count} problem${check.problem_count === 1 ? '' : 's'}`;
    case 'failed':
      return 'Failed';
    case 'canceled':
      return 'Withdrawn';
    default:
      return check.state;
  }
}

/** How far a check has got, once it has counted what it reads. */
export function checkProgress(check: IntegrityCheck): string {
  if (check.objects_total == null || check.bytes_total == null) {
    return check.state === 'queued' ? 'Not started' : 'Counting what to read';
  }
  return `${check.objects_checked} of ${check.objects_total} files (${formatBytes(
    check.bytes_checked,
  )} of ${formatBytes(check.bytes_total)})`;
}

/** The share read, 0 to 100, for a progress bar; `null` until counted. */
export function checkPercent(check: IntegrityCheck): number | null {
  if (check.bytes_total == null) return null;
  if (check.bytes_total === 0) return 100;
  return Math.min(100, Math.floor((check.bytes_checked / check.bytes_total) * 100));
}

/** A problem's kind, in words. */
export function problemKindLabel(kind: string): string {
  switch (kind) {
    case 'missing':
      return 'Missing';
    case 'corrupt':
      return 'Reads back as other bytes';
    case 'size_mismatch':
      return 'Wrong length';
    case 'unreadable':
      return 'Could not be read';
    case 'manifest_mismatch':
      return 'Manifest disagrees with the catalogue';
    case 'manifest_unreadable':
      return 'Manifest could not be read';
    default:
      return kind;
  }
}

/** What a check read: the whole library, or one artifact. */
export function scopeLabel(check: Pick<IntegrityCheck, 'scope'>): string {
  return check.scope === 'all' ? 'Whole library' : 'One artifact';
}

/** A count and its bytes: `3 files (1.5 MiB)`. */
export function tallyLabel(tally: Tally): string {
  return `${tally.count} file${tally.count === 1 ? '' : 's'} (${formatBytes(tally.bytes)})`;
}

/** A plan's grace period, in words. */
export function graceLabel(plan: Pick<GcPlan, 'grace_seconds'>): string {
  const days = Math.floor(plan.grace_seconds / 86_400);
  if (days === 0) return 'no grace period';
  return `the ${days}-day grace period`;
}
