// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import type { components } from './api-types';
import type { LoadState, Problem } from './library';

/**
 * Wire types come from the generated OpenAPI client, never hand-written.
 * A drift between server and browser then becomes a type error rather than a
 * runtime surprise.
 */
export type BurnJobView = components['schemas']['BurnJobView'];
export type BurnJobPage = components['schemas']['BurnJobPage'];
export type BurnJobDetail = components['schemas']['BurnJobDetail'];
export type BurnJobCreated = components['schemas']['BurnJobCreated'];
export type BurnAttemptView = components['schemas']['BurnAttemptView'];
export type BurnEventPage = components['schemas']['BurnEventPage'];
export type BurnEventView = components['schemas']['BurnEventView'];
export type CreateBurnJobRequest = components['schemas']['CreateBurnJobRequest'];

/**
 * A problem to show when the server did not return one.
 *
 * Duplicated deliberately rather than exported from `library`: the two modules
 * describe different resources, and a shared private helper here is cheaper
 * than a shared abstraction that would have to be right for both.
 */
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

interface RequestOptions {
  method?: string;
  body?: unknown;
  idempotencyKey?: string;
}

async function request<T>(
  fetcher: typeof fetch,
  path: string,
  options: RequestOptions = {},
): Promise<T | Problem> {
  const headers: Record<string, string> = {};
  if (options.body !== undefined) headers['content-type'] = 'application/json';
  if (options.idempotencyKey) headers['idempotency-key'] = options.idempotencyKey;

  let response: Response;
  try {
    response = await fetcher(path, {
      method: options.method ?? 'GET',
      headers,
      body: options.body === undefined ? undefined : JSON.stringify(options.body),
    });
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

/** Fetch one page of the burn queue, newest first. */
export async function loadBurnQueue(
  fetcher: typeof fetch = fetch,
  cursor?: string,
): Promise<LoadState<BurnJobPage>> {
  const query = cursor ? `?cursor=${encodeURIComponent(cursor)}` : '';
  const result = await request<BurnJobPage>(fetcher, `/api/v1/burn-jobs${query}`);
  if (isProblem(result)) return { kind: 'error', problem: result };
  // Empty is a distinct state, not a zero-length success: it needs its own
  // copy telling the operator what to do next.
  if (result.items.length === 0) return { kind: 'empty' };
  return { kind: 'ready', data: result };
}

/** Fetch one burn job with its attempts. */
export async function loadBurnJob(
  id: string,
  fetcher: typeof fetch = fetch,
): Promise<LoadState<BurnJobDetail>> {
  const result = await request<BurnJobDetail>(
    fetcher,
    `/api/v1/burn-jobs/${encodeURIComponent(id)}`,
  );
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/** Fetch the events one attempt reported, oldest first. */
export async function loadAttemptEvents(
  attemptId: string,
  fetcher: typeof fetch = fetch,
  after?: number,
): Promise<LoadState<BurnEventPage>> {
  const query = after === undefined ? '' : `?after=${after}`;
  const result = await request<BurnEventPage>(
    fetcher,
    `/api/v1/burn-attempts/${encodeURIComponent(attemptId)}/events${query}`,
  );
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/**
 * What a command answered.
 *
 * Narrower than `LoadState` on purpose: a command either did the thing or was
 * refused. It never loads and is never empty, and saying so keeps the callers
 * from having to handle two states that cannot occur.
 */
export type CommandResult<T> = { kind: 'ready'; data: T } | { kind: 'error'; problem: Problem };

/**
 * Queue a burn.
 *
 * The key is required rather than optional. This is the one request in the
 * product that spends a physical disc, and a browser that retried without one
 * would burn two.
 */
export async function createBurnJob(
  body: CreateBurnJobRequest,
  idempotencyKey: string,
  fetcher: typeof fetch = fetch,
): Promise<CommandResult<BurnJobCreated>> {
  const result = await request<BurnJobCreated>(fetcher, '/api/v1/burn-jobs', {
    method: 'POST',
    body,
    idempotencyKey,
  });
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/** Ask the server to cancel a burn. It may refuse, and refusing is correct. */
export async function cancelBurnJob(
  id: string,
  fetcher: typeof fetch = fetch,
): Promise<CommandResult<BurnJobDetail>> {
  const result = await request<BurnJobDetail>(
    fetcher,
    `/api/v1/burn-jobs/${encodeURIComponent(id)}/cancel`,
    { method: 'POST' },
  );
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/**
 * Record that a person has accounted for a burn's disc.
 *
 * The only way out of `needs_attention`, and deliberately a separate action
 * from retrying: the release is a claim about the physical world, and the
 * disc's own record is where what happened to it belongs.
 */
export async function resolveAttention(
  id: string,
  fetcher: typeof fetch = fetch,
): Promise<CommandResult<BurnJobDetail>> {
  const result = await request<BurnJobDetail>(
    fetcher,
    `/api/v1/burn-jobs/${encodeURIComponent(id)}/resolve-attention`,
    { method: 'POST' },
  );
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/** Requeue a failed or cancelled burn for another attempt. */
export async function retryBurnJob(
  id: string,
  fetcher: typeof fetch = fetch,
): Promise<CommandResult<BurnJobDetail>> {
  const result = await request<BurnJobDetail>(
    fetcher,
    `/api/v1/burn-jobs/${encodeURIComponent(id)}/retry`,
    { method: 'POST' },
  );
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/**
 * A fresh idempotency key.
 *
 * Held by the caller across retries of the same submission, so a form the
 * operator submits twice because the first response was lost produces one
 * burn. A new key means a new disc, which is why this is never called from
 * inside the request path.
 */
export function newIdempotencyKey(): string {
  if (typeof crypto !== 'undefined' && 'randomUUID' in crypto) {
    return crypto.randomUUID();
  }
  // Only reached in environments without the Web Crypto API. Uniqueness here
  // matters less than never returning a constant, which would make every
  // burn a replay of the first.
  return `burn-${Date.now()}-${Math.random().toString(36).slice(2)}`;
}

// --- labels ---------------------------------------------------------------

/**
 * Plain-language state label.
 *
 * The queue is read by someone deciding whether to walk to the drive, so the
 * labels answer that rather than naming internal states.
 */
export function jobStateLabel(state: string): string {
  switch (state) {
    case 'queued':
      return 'Queued';
    case 'leased':
      return 'Assigned to a worker';
    case 'staging':
      return 'Copying to the worker';
    case 'waiting_for_media':
      return 'Waiting for a disc';
    case 'preflighting':
      return 'Checking the disc';
    case 'ready':
      return 'Ready to write';
    case 'writing':
      return 'Writing';
    case 'finalizing':
      return 'Closing the disc';
    case 'verifying':
      return 'Reading back';
    case 'complete':
      return 'Verified';
    case 'failed':
      return 'Failed';
    case 'canceled':
      return 'Cancelled';
    case 'needs_attention':
      return 'Needs attention';
    default:
      return state;
  }
}

/** A short symbol accompanying a state, so status is never colour alone. */
export function jobStateMark(state: string): string {
  switch (state) {
    case 'complete':
      return '✓';
    case 'failed':
    case 'needs_attention':
      return '✕';
    case 'waiting_for_media':
      return '!';
    case 'canceled':
      return '–';
    case 'writing':
    case 'finalizing':
    case 'verifying':
      return '●';
    default:
      return '·';
  }
}

/** Plain-language stage label for a worker's latest report. */
export function stageLabel(stage: string): string {
  switch (stage) {
    case 'claimed':
      return 'Claimed';
    case 'staging':
      return 'Copying to the worker';
    case 'waiting_for_media':
      return 'Waiting for a disc';
    case 'preflighting':
      return 'Checking the disc';
    case 'writing':
      return 'Writing';
    case 'finalizing':
      return 'Closing the disc';
    case 'verifying':
      return 'Reading back';
    case 'completing':
      return 'Finishing';
    default:
      return stage;
  }
}

/**
 * What an attempt's outcome means, in the words the design settled on.
 *
 * Each of these is a different claim, and collapsing any two would promise
 * something nothing checked. A written disc is not a verified one, and a disc
 * that failed verification physically exists and has to be dealt with.
 */
export function attemptOutcomeLabel(state: string): string {
  switch (state) {
    case 'verified':
      return 'Verified — read back and matched';
    case 'written':
      return 'Written but not verified';
    case 'verification_failed':
      return 'Verification failed — the disc exists and does not match';
    case 'write_failed':
      return 'Write failed — a disc was consumed';
    case 'failed_before_write':
      return 'Failed before writing — no disc was consumed';
    case 'canceled':
      return 'Cancelled before writing';
    case 'interrupted':
      return 'Interrupted — the worker lost contact mid-attempt';
    case 'claimed':
      return 'Claimed';
    case 'staging':
      return 'Copying to the worker';
    case 'preflighting':
      return 'Checking the disc';
    case 'writing':
      return 'Writing';
    case 'verifying':
      return 'Reading back';
    default:
      return state;
  }
}

/**
 * Whether cancelling should be offered at all.
 *
 * Not merely disabled once writing has begun: the button is not shown. A
 * casual cancel control during an irreversible write invites a click that
 * cannot be honoured, and the honest answer is that the option no longer
 * exists.
 */
export function canCancel(job: BurnJobView): boolean {
  return !job.has_started_writing && !isSettled(job.state);
}

/** Whether a retry should be offered. */
export function canRetry(job: BurnJobView): boolean {
  return job.state === 'failed' || job.state === 'canceled';
}

/** Whether the job has finished, one way or another. */
export function isSettled(state: string): boolean {
  return (
    state === 'complete' ||
    state === 'failed' ||
    state === 'canceled' ||
    state === 'needs_attention'
  );
}

/** Whether a job is doing something a viewer would want refreshed. */
export function isLive(state: string): boolean {
  return !isSettled(state);
}

/** Progress as a whole percentage, or null when the stage does not report it. */
export function progressPercent(fraction: number | null | undefined): number | null {
  if (fraction === null || fraction === undefined) return null;
  return Math.round(Math.min(Math.max(fraction, 0), 1) * 100);
}

/**
 * One line describing where a burn is, for a live region.
 *
 * Assembled in one place so the announcement a screen reader hears and the
 * text on the page cannot drift apart.
 */
export function progressSentence(job: BurnJobView): string {
  const state = jobStateLabel(job.state);
  const percent = progressPercent(job.progress?.fraction);
  if (!job.progress) return state;
  const stage = stageLabel(job.progress.stage);
  return percent === null ? `${state}: ${stage}` : `${state}: ${stage}, ${percent}%`;
}
