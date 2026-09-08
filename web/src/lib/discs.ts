// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import type { components } from './api-types';
import type { CommandResult } from './burns';
import type { LoadState, Problem } from './library';

/**
 * Wire types come from the generated OpenAPI client, never hand-written.
 * A drift between server and browser then becomes a type error rather than a
 * runtime surprise.
 */
export type PhysicalCopyView = components['schemas']['PhysicalCopyView'];
export type PhysicalCopyPage = components['schemas']['PhysicalCopyPage'];
export type PhysicalCopyDetail = components['schemas']['PhysicalCopyDetail'];
export type CheckView = components['schemas']['CheckView'];
export type UpdatePhysicalCopyRequest = components['schemas']['UpdatePhysicalCopyRequest'];
export type RecordCheckRequest = components['schemas']['RecordCheckRequest'];

function transportProblem(detail: string): Problem {
  return {
    type: 'about:blank',
    title: 'Could not reach the server',
    status: 0,
    code: 'TRANSPORT_FAILURE',
    detail,
  };
}

function isProblem(value: unknown): value is Problem {
  return typeof value === 'object' && value !== null && 'code' in value && 'detail' in value;
}

async function request<T>(
  fetcher: typeof fetch,
  path: string,
  init?: RequestInit,
): Promise<T | Problem> {
  let response: Response;
  try {
    response = await fetcher(path, init);
  } catch (error) {
    return transportProblem(error instanceof Error ? error.message : 'unknown transport failure');
  }

  let body: unknown;
  try {
    body = await response.json();
  } catch {
    return transportProblem(`the server returned ${response.status} with no readable body`);
  }

  if (!response.ok) {
    const problem = body as Partial<Problem>;
    return typeof problem?.code === 'string'
      ? (body as Problem)
      : transportProblem(`the server returned ${response.status}`);
  }
  return body as T;
}

/** Fetch one page of the disc inventory, newest first. */
export async function loadDiscs(
  fetcher: typeof fetch = fetch,
  options: { cursor?: string; status?: string } = {},
): Promise<LoadState<PhysicalCopyPage>> {
  const query = new URLSearchParams();
  if (options.cursor) query.set('cursor', options.cursor);
  if (options.status) query.set('status', options.status);
  const suffix = query.toString() ? `?${query.toString()}` : '';

  const result = await request<PhysicalCopyPage>(fetcher, `/api/v1/physical-copies${suffix}`);
  if (isProblem(result)) return { kind: 'error', problem: result };
  // Empty is a distinct state, not a zero-length success: it needs its own
  // copy telling the operator what to do next.
  if (result.items.length === 0) return { kind: 'empty' };
  return { kind: 'ready', data: result };
}

/** Fetch one disc and its check history. */
export async function loadDisc(
  id: string,
  fetcher: typeof fetch = fetch,
): Promise<LoadState<PhysicalCopyDetail>> {
  const result = await request<PhysicalCopyDetail>(
    fetcher,
    `/api/v1/physical-copies/${encodeURIComponent(id)}`,
  );
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/** Record what an operator knows about a disc. */
export async function updateDisc(
  id: string,
  body: UpdatePhysicalCopyRequest,
  fetcher: typeof fetch = fetch,
): Promise<CommandResult<PhysicalCopyDetail>> {
  const result = await request<PhysicalCopyDetail>(
    fetcher,
    `/api/v1/physical-copies/${encodeURIComponent(id)}`,
    {
      method: 'PATCH',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
    },
  );
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/** Record a check performed on a disc. */
export async function recordCheck(
  id: string,
  body: RecordCheckRequest,
  fetcher: typeof fetch = fetch,
): Promise<CommandResult<PhysicalCopyDetail>> {
  const result = await request<PhysicalCopyDetail>(
    fetcher,
    `/api/v1/physical-copies/${encodeURIComponent(id)}/checks`,
    {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
    },
  );
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/** Record that a disc has been destroyed. */
export async function markDestroyed(
  id: string,
  fetcher: typeof fetch = fetch,
): Promise<CommandResult<PhysicalCopyDetail>> {
  const result = await request<PhysicalCopyDetail>(
    fetcher,
    `/api/v1/physical-copies/${encodeURIComponent(id)}/mark-destroyed`,
    { method: 'POST' },
  );
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

// --- labels ---------------------------------------------------------------

/**
 * Plain-language condition label.
 *
 * Each of these is a different claim about a physical object, and the words
 * say which. "Produced, not verified" is not a softer way of saying verified.
 */
export function conditionLabel(status: string): string {
  switch (status) {
    case 'produced_unverified':
      return 'Written, not verified';
    case 'verified':
      return 'Verified — read back and matched';
    case 'verification_failed':
      return 'Failed verification — destroy this disc';
    case 'degraded':
      return 'Degraded — destroy this disc';
    case 'lost':
      return 'Lost';
    case 'destroyed':
      return 'Destroyed';
    case 'unknown':
      return 'Condition unknown';
    default:
      return status;
  }
}

/** A short symbol accompanying a condition, so status is never colour alone. */
export function conditionMark(status: string): string {
  switch (status) {
    case 'verified':
      return '✓';
    case 'verification_failed':
    case 'degraded':
      return '✕';
    case 'lost':
      return '?';
    case 'destroyed':
      return '–';
    default:
      return '·';
  }
}

/** Plain-language label for what a check did. */
export function methodLabel(method: string): string {
  switch (method) {
    case 'full_sector_readback':
      return 'Read the whole disc back';
    case 'filesystem_compare':
      return 'Compared the filesystem';
    case 'tool_verify':
      return "Trusted the engine's own check";
    case 'track_hash_compare':
      return 'Compared track hashes';
    case 'provider_match':
      return 'Matched against a provider';
    case 'none':
      return 'No verification';
    default:
      return method;
  }
}

/**
 * Conditions an operator may set by hand.
 *
 * `verified` and `verification_failed` are missing on purpose: those describe
 * what a read-back found, and recording a check is how they are written. The
 * server refuses them too; this list is so the UI never offers what would be
 * refused.
 */
export const SETTABLE_CONDITIONS = ['degraded', 'lost', 'unknown'] as const;

/** Whether anything further can be recorded about this disc. */
export function isEditable(copy: PhysicalCopyView): boolean {
  return !copy.is_settled;
}
