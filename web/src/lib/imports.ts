// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import type { components } from './api-types';
import { apiFetch, transportProblem } from './session';
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
export type UrlImportRequest = components['schemas']['UrlImportRequest'];
export type UploadView = components['schemas']['UploadView'];
export type DownloadClient = components['schemas']['DownloadClientView'];
export type SubmitJobRequest = components['schemas']['SubmitJobRequest'];
export type IntegrationJobView = components['schemas']['IntegrationJobView'];

/** The largest `.torrent` the server takes, as it says in its own limit. */
export const MAX_TORRENT_BYTES = 8 * 1024 * 1024;

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
  fetcher: typeof fetch = apiFetch,
): Promise<LoadState<ImportSources>> {
  const result = await request<ImportSources>(fetcher, '/api/v1/import-sources');
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/** Fetch one page of imports, newest first. */
export async function loadImports(
  fetcher: typeof fetch = apiFetch,
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
  fetcher: typeof fetch = apiFetch,
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
 * Have the server fetch a file from a URL and import it (ADR 0016). Only
 * where an administrator turned URL imports on; the server refuses an
 * address it will not reach, and says why.
 */
export async function importFromUrl(
  body: UrlImportRequest,
  fetcher: typeof fetch = apiFetch,
): Promise<CommandResult<ImportView>> {
  const result = await request<ImportView>(fetcher, '/api/v1/imports/url', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/**
 * What a URL import asks for: the URL, and the SHA-256 only when one was
 * typed, lowercased, without spaces.
 */
export function urlImportRequest(url: string, sha256: string): UrlImportRequest {
  const digest = sha256.replace(/\s+/g, '').toLowerCase();
  return { url: url.trim(), ...(digest ? { expected_sha256: digest } : {}) };
}

/**
 * Hand a torrent the operator already has to a download client (ADR 0016).
 * The import that follows it is in the list once the client has it.
 */
export async function sendTorrent(
  integrationId: string,
  body: SubmitJobRequest,
  fetcher: typeof fetch = apiFetch,
): Promise<CommandResult<IntegrationJobView>> {
  const result = await request<IntegrationJobView>(
    fetcher,
    `/api/v1/integrations/${encodeURIComponent(integrationId)}/jobs`,
    {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
    },
  );
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/** Bytes as base64, in slices, so a large file does not overflow the stack. */
export function toBase64(bytes: Uint8Array): string {
  let binary = '';
  for (let start = 0; start < bytes.length; start += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(start, start + 0x8000));
  }
  return btoa(binary);
}

/**
 * What a torrent submission sends: the `.torrent` file when one was chosen,
 * otherwise the magnet link. `null` when there is neither, or the file is
 * past the server's limit.
 */
export async function torrentRequest(
  magnet: string,
  file: File | null | undefined,
): Promise<SubmitJobRequest | null> {
  if (file) {
    if (file.size > MAX_TORRENT_BYTES) return null;
    return { torrent: toBase64(new Uint8Array(await file.arrayBuffer())) };
  }
  const link = magnet.trim();
  return link ? { magnet: link } : null;
}

/** What handing over a torrent came to, in words. */
export function torrentOutcome(job: IntegrationJobView, client: string): string {
  switch (job.outcome) {
    case 'created':
      return `Handed to ${client}. The import below follows it and brings it in when it finishes.`;
    case 'resubmitted':
      return `Handed to ${client} again; a new import follows it.`;
    case 'already_active':
      return `${client} already has this torrent, and its import is under way below.`;
    case 'already_imported':
      return 'This torrent is already in the library.';
    default:
      return `Handed to ${client}.`;
  }
}

/** Where an import came from, in words. */
export function sourceLabel(sourceKind: string): string {
  switch (sourceKind) {
    case 'upload':
      return 'Upload';
    case 'watch_folder':
      return 'Watched folder';
    case 'http_url':
      return 'URL';
    case 'qbittorrent_job':
      return 'qBittorrent';
    case 'physical_drive':
      return 'Ripped from a disc';
    default:
      return sourceKind.replace(/_/g, ' ');
  }
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
  fetcher: typeof fetch = apiFetch,
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

// --- uploads sent in parts ------------------------------------------------------

/** Files larger than this are sent in parts, which survive a dropped connection. */
export const PARTS_ABOVE_BYTES = 64 * 1024 * 1024;

/** How many times one part is tried before the upload stops and says why. */
const PART_ATTEMPTS = 3;

/** Where an upload's id is remembered, so choosing the same file again resumes it. */
export interface UploadMemory {
  get(key: string): string | null;
  set(key: string, value: string): void;
  remove(key: string): void;
}

/**
 * The browser's local storage, quietly forgetting where it is unavailable
 * (a private window, blocked site data): the upload still works, it just
 * cannot be resumed after a reload.
 */
export const browserMemory: UploadMemory = {
  get(key) {
    try {
      return globalThis.localStorage?.getItem(key) ?? null;
    } catch {
      return null;
    }
  },
  set(key, value) {
    try {
      globalThis.localStorage?.setItem(key, value);
    } catch {
      // Not remembered; nothing else changes.
    }
  },
  remove(key) {
    try {
      globalThis.localStorage?.removeItem(key);
    } catch {
      // Nothing to forget.
    }
  },
};

/** What identifies a file across reloads: its name, size and modification time. */
export function uploadKey(file: File): string {
  return `tangible.upload:${file.name}:${file.size}:${file.lastModified}`;
}

/**
 * A part's SHA-256, or nothing where the browser cannot compute one: Web
 * Crypto exists only over https and on localhost. The server hashes every
 * part either way.
 */
export async function partDigest(bytes: ArrayBuffer): Promise<string | undefined> {
  const subtle = globalThis.crypto?.subtle;
  if (!subtle) return undefined;
  try {
    const digest = new Uint8Array(await subtle.digest('SHA-256', bytes));
    return Array.from(digest, (byte) => byte.toString(16).padStart(2, '0')).join('');
  } catch {
    return undefined;
  }
}

/** An upload of this file to carry on with, if one is remembered and still open. */
async function resumable(
  file: File,
  fetcher: typeof fetch,
  memory: UploadMemory,
): Promise<UploadView | null> {
  const id = memory.get(uploadKey(file));
  if (!id) return null;
  const found = await request<UploadView>(fetcher, `/api/v1/uploads/${encodeURIComponent(id)}`);
  if (!isProblem(found) && found.state === 'open' && found.total_bytes === file.size) {
    return found;
  }
  memory.remove(uploadKey(file));
  return null;
}

/**
 * Upload a file in parts and queue its import, carrying on with an upload of
 * the same file the page started before, if there is one.
 *
 * Each part is tried up to three times. `onProgress` hears the bytes the
 * server has, after each part.
 */
export async function uploadInParts(
  file: File,
  onProgress: (sent: number, total: number) => void,
  fetcher: typeof fetch = apiFetch,
  memory: UploadMemory = browserMemory,
): Promise<CommandResult<ImportView>> {
  let upload = await resumable(file, fetcher, memory);
  if (!upload) {
    const started = await request<UploadView>(fetcher, '/api/v1/uploads', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ filename: file.name, total_bytes: file.size }),
    });
    if (isProblem(started)) return { kind: 'error', problem: started };
    upload = started;
    memory.set(uploadKey(file), upload.id);
  }

  let sent = upload.bytes_received;
  onProgress(sent, file.size);
  for (let part = 0; part < upload.part_count; part++) {
    if (upload.received_parts.includes(part)) continue;
    const start = part * upload.part_size;
    const bytes = await file
      .slice(start, Math.min(start + upload.part_size, file.size))
      .arrayBuffer();
    const digest = await partDigest(bytes);
    let outcome: UploadView | Problem = transportProblem('the part was not sent');
    for (let attempt = 0; attempt < PART_ATTEMPTS; attempt++) {
      outcome = await request<UploadView>(
        fetcher,
        `/api/v1/uploads/${encodeURIComponent(upload.id)}/parts/${part}`,
        {
          method: 'PUT',
          headers: {
            'content-type': 'application/octet-stream',
            ...(digest ? { 'x-content-sha256': digest } : {}),
          },
          body: bytes,
        },
      );
      if (!isProblem(outcome)) break;
    }
    if (isProblem(outcome)) return { kind: 'error', problem: outcome };
    sent = outcome.bytes_received;
    onProgress(sent, file.size);
  }

  const queued = await request<ImportView>(
    fetcher,
    `/api/v1/uploads/${encodeURIComponent(upload.id)}/complete`,
    { method: 'POST' },
  );
  if (isProblem(queued)) return { kind: 'error', problem: queued };
  memory.remove(uploadKey(file));
  return { kind: 'ready', data: queued };
}

/** Stop an import. */
export async function cancelImport(
  id: string,
  fetcher: typeof fetch = apiFetch,
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
  fetcher: typeof fetch = apiFetch,
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
export function importStateLabel(
  state: string,
  sourceKind?: string,
  errorCode?: string | null,
): string {
  switch (state) {
    case 'requested':
      return 'Queued';
    case 'acquiring':
      // A torrent import waits in acquiring while the client downloads, and
      // says why when the client cannot be asked.
      if (sourceKind === 'qbittorrent_job') {
        return errorCode ? 'Waiting for qBittorrent' : 'Downloading in qBittorrent';
      }
      return sourceKind === 'http_url' ? 'Downloading' : 'Copying in';
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
