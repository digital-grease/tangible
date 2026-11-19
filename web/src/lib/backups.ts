// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// Copies of the database (ADR 0026, docs/plans/backups-ui.md): the copies
// in the backups volume, taking one now, fetching one, adding one from
// elsewhere, and removing one. The library's files and the integration key
// are not in a copy; the page says so.

import type { components } from './api-types';
import type { Tone } from './components/ui/tone';
import { formatBytes } from './library';
import { apiCall, apiFetch, transportProblem, type Problem, type Result } from './session';

export type Backups = components['schemas']['BackupsView'];
export type Copy = components['schemas']['CopyEntry'];
export type CopyKind = components['schemas']['CopyKind'];
export type Operation = components['schemas']['Operation'];

/** How often the page looks again while something is under way, in milliseconds. */
export const FOLLOW_EVERY_MS = 2000;

/** The copies, and what the server can do with them. */
export function loadBackups(fetcher: typeof fetch = apiFetch): Promise<Result<Backups>> {
  return apiCall(fetcher, '/api/v1/backups');
}

/** Take a copy now; it is taken in the background. */
export function takeCopy(fetcher: typeof fetch = apiFetch): Promise<Result<Backups>> {
  return apiCall(fetcher, '/api/v1/backups', 'POST');
}

/** Remove a copy. */
export function deleteCopy(name: string, fetcher: typeof fetch = apiFetch): Promise<Result<void>> {
  return apiCall(fetcher, `/api/v1/backups/${encodeURIComponent(name)}`, 'DELETE');
}

/** Where a copy downloads from. */
export function downloadHref(name: string): string {
  return `/api/v1/backups/${encodeURIComponent(name)}/download`;
}

/** Add a copy from a file: a PostgreSQL custom-format archive. */
export async function uploadCopy(
  file: Blob,
  limit: number,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<Copy>> {
  if (file.size > limit) {
    return { kind: 'error', problem: tooLarge(file.size, limit), retryAfter: null };
  }
  let response: Response;
  try {
    response = await fetcher('/api/v1/backups/uploads', {
      method: 'PUT',
      headers: { 'content-type': 'application/octet-stream' },
      body: file,
    });
  } catch (error) {
    return {
      kind: 'error',
      problem: transportProblem(error instanceof Error ? error.message : 'unknown failure'),
      retryAfter: null,
    };
  }
  let parsed: unknown;
  try {
    parsed = await response.json();
  } catch {
    // A proxy in front of the server refusing a large body answers in plain text.
    return {
      kind: 'error',
      problem:
        response.status === 413
          ? tooLarge(file.size, null)
          : transportProblem(`the server returned ${response.status} with no readable body`),
      retryAfter: null,
    };
  }
  if (!response.ok) {
    const problem = parsed as Partial<Problem>;
    return {
      kind: 'error',
      problem:
        typeof problem?.code === 'string'
          ? (parsed as Problem)
          : transportProblem(`the server returned ${response.status}`),
      retryAfter: null,
    };
  }
  return { kind: 'ok', data: parsed as Copy };
}

function tooLarge(size: number, limit: number | null): Problem {
  return {
    type: 'about:blank',
    title: 'Request is not valid',
    status: 413,
    code: 'VALIDATION_FAILED',
    detail:
      limit === null
        ? `the file, ${formatBytes(size)}, was refused as too large before it reached Tangible; a proxy in front of it may limit uploads`
        : `the file is ${formatBytes(size)}, more than the ${formatBytes(limit)} this server accepts (TANGIBLE_MAX_BACKUP_UPLOAD_BYTES)`,
  };
}

/** Who took or added a copy, in words. */
export function kindLabel(kind: CopyKind): string {
  switch (kind) {
    case 'taken':
      return 'Taken by Tangible';
    case 'uploaded':
      return 'Uploaded';
    case 'added':
      return 'Added at the machine';
  }
}

/** Whether a copy counts towards those kept, and so goes when it is the oldest. */
export function isPruned(kind: CopyKind): boolean {
  return kind === 'taken';
}

/** What is under way, in words. */
export function operationLabel(operation: Operation): string {
  return operation.kind === 'taking'
    ? `Taking a copy, asked for by ${operation.by}`
    : `Receiving a copy uploaded by ${operation.by}`;
}

/** Whether copies can be taken here, and if not, why not. */
export function readiness(backups: Backups): { tone: Tone; label: string; detail: string | null } {
  if (!backups.tools.ready) {
    return {
      tone: 'bad',
      label: 'Copies cannot be taken',
      detail: backups.tools.problem ?? 'the dump tools cannot copy this database',
    };
  }
  if (!backups.writable) {
    return {
      tone: 'bad',
      label: 'The backups volume cannot be written',
      detail:
        "start the stack once with this release's compose.yaml (docker compose up -d), whose backup service lets the server write there",
    };
  }
  if (backups.kept === 0) {
    return {
      tone: 'warn',
      label: 'Copies are turned off',
      detail: 'TANGIBLE_BACKUPS_KEPT is 0, so none is taken; copies uploaded or added are kept',
    };
  }
  return {
    tone: 'ok',
    label: `The newest ${backups.kept} copies Tangible takes are kept`,
    detail: `PostgreSQL ${backups.tools.database_version ?? '?'}, copied with its version ${backups.tools.tools_version ?? '?'} tools`,
  };
}

/** The volume's copies in all, for the panel's aside. */
export function totalSize(copies: readonly Copy[]): string {
  return formatBytes(copies.reduce((sum, copy) => sum + copy.size_bytes, 0));
}
