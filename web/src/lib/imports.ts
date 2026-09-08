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
export type ImportView = components['schemas']['ImportView'];
export type ImportPage = components['schemas']['ImportPage'];
export type ImportSources = components['schemas']['ImportSources'];
export type CreateImportRequest = components['schemas']['CreateImportRequest'];

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

/** Where this server will take imports from. */
export async function loadImportSources(
  fetcher: typeof fetch = fetch,
): Promise<LoadState<ImportSources>> {
  const result = await request<ImportSources>(fetcher, '/api/v1/import-sources');
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/** Fetch one page of imports, newest first. */
export async function loadImports(
  fetcher: typeof fetch = fetch,
  cursor?: string,
): Promise<LoadState<ImportPage>> {
  const query = cursor ? `?cursor=${encodeURIComponent(cursor)}` : '';
  const result = await request<ImportPage>(fetcher, `/api/v1/imports${query}`);
  if (isProblem(result)) return { kind: 'error', problem: result };
  // Empty is a distinct state, not a zero-length success: it needs its own
  // copy telling the operator what to do next.
  if (result.items.length === 0) return { kind: 'empty' };
  return { kind: 'ready', data: result };
}

/** Import a file from a configured watched root. */
export async function importFromWatchedRoot(
  body: CreateImportRequest,
  fetcher: typeof fetch = fetch,
): Promise<CommandResult<ImportView>> {
  const result = await request<ImportView>(fetcher, '/api/v1/imports', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/**
 * Upload a file and queue it for import.
 *
 * The file is sent as the request body rather than as multipart form data: it
 * may be tens of gigabytes, and the server streams the body straight to disk
 * without buffering a copy of it.
 */
export async function uploadImport(
  file: File,
  fetcher: typeof fetch = fetch,
): Promise<CommandResult<ImportView>> {
  const result = await request<ImportView>(
    fetcher,
    `/api/v1/imports/upload?filename=${encodeURIComponent(file.name)}`,
    {
      method: 'POST',
      headers: { 'content-type': 'application/octet-stream' },
      body: file,
    },
  );
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/** Stop an import. */
export async function cancelImport(
  id: string,
  fetcher: typeof fetch = fetch,
): Promise<CommandResult<ImportView>> {
  const result = await request<ImportView>(
    fetcher,
    `/api/v1/imports/${encodeURIComponent(id)}/cancel`,
    { method: 'POST' },
  );
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/** Try a failed or cancelled import again. */
export async function retryImport(
  id: string,
  fetcher: typeof fetch = fetch,
): Promise<CommandResult<ImportView>> {
  const result = await request<ImportView>(
    fetcher,
    `/api/v1/imports/${encodeURIComponent(id)}/retry`,
    { method: 'POST' },
  );
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

// --- labels ---------------------------------------------------------------

/**
 * Plain-language state label.
 *
 * The states are the pipeline's; the words are the operator's. "Hashing" and
 * "inspecting" mean nothing to somebody who just dropped a file on a page.
 */
export function importStateLabel(state: string): string {
  switch (state) {
    case 'requested':
      return 'Queued';
    case 'acquiring':
      return 'Copying in';
    case 'staged':
      return 'Ready to process';
    case 'hashing':
      return 'Hashing';
    case 'inspecting':
      return 'Inspecting';
    case 'registering':
      return 'Filing in the library';
    case 'complete':
      return 'Imported';
    case 'failed_retryable':
      return 'Failed, will try again';
    case 'failed_terminal':
      return 'Failed';
    case 'canceled':
      return 'Cancelled';
    case 'quarantined':
      return 'Quarantined';
    default:
      return state;
  }
}

/** A short symbol accompanying a state, so status is never colour alone. */
export function importStateMark(state: string): string {
  switch (state) {
    case 'complete':
      return '✓';
    case 'failed_terminal':
    case 'quarantined':
      return '✕';
    case 'failed_retryable':
      return '!';
    case 'canceled':
      return '–';
    default:
      return '·';
  }
}

/** Whether the import has finished, one way or another. */
export function isSettled(job: ImportView): boolean {
  return job.is_settled;
}

/** Whether cancelling should be offered. */
export function canCancel(job: ImportView): boolean {
  return !job.is_settled;
}

/**
 * Whether a retry should be offered.
 *
 * Not for a completed import: the artifact it produced is already in the
 * library, and importing the same bytes again is a new import.
 */
export function canRetry(job: ImportView): boolean {
  return (
    job.state === 'failed_terminal' ||
    job.state === 'failed_retryable' ||
    job.state === 'canceled' ||
    job.state === 'quarantined'
  );
}

/**
 * How far along an import is, as a fraction, when that can be said honestly.
 *
 * Only during the transfer, and only when the source said how many bytes to
 * expect. Hashing and inspecting report no progress, and inventing a
 * percentage for them would be a progress bar that lies.
 */
export function transferFraction(job: ImportView): number | null {
  const expected = job.bytes_expected;
  if (!expected || expected <= 0) return null;
  if (job.state !== 'acquiring' && job.state !== 'requested') return null;
  return Math.min(job.bytes_received / expected, 1);
}
