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
export type Schedule = components['schemas']['ScheduleView'];
export type Restore = components['schemas']['RestoreView'];
export type RestoreStatus = components['schemas']['RestoreStatus'];
export type Progress = components['schemas']['Progress'];
export type Step = components['schemas']['Step'];
export type Registered = components['schemas']['RegisteredView'];

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

/** Take a copy every day at `time` (HH:MM, UTC), or none. */
export function setSchedule(
  enabled: boolean,
  time: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<Schedule>> {
  return apiCall(fetcher, '/api/v1/backups/schedule', 'PUT', { enabled, time });
}

/** The schedule, in words. */
export function scheduleLabel(schedule: Schedule, kept: number): string {
  if (!schedule.enabled) return 'Copies are taken only at a start and when asked for';
  if (kept === 0) return `Every day at ${schedule.time} UTC, but copies are turned off`;
  return `Every day at ${schedule.time} UTC`;
}

/**
 * Restore the database from a copy, `confirm` being its name typed again.
 * The server answers nothing else until it starts again on the result.
 */
export function restoreCopy(
  name: string,
  confirm: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<Progress>> {
  return apiCall(fetcher, `/api/v1/backups/${encodeURIComponent(name)}/restore`, 'POST', {
    confirm,
  });
}

/** Whether a restore is under way; anyone may ask, and it names nothing. */
export function restoreStatus(fetcher: typeof fetch = apiFetch): Promise<Result<RestoreStatus>> {
  return apiCall(fetcher, '/api/v1/backups/restore');
}

/** Catalogue again what the library has and the restored catalogue lacks. */
export function registerMissing(fetcher: typeof fetch = apiFetch): Promise<Result<Registered>> {
  return apiCall(fetcher, '/api/v1/backups/register-missing', 'POST');
}

/** Where a restore is, in words. */
export function stepLabel(step: Step): string {
  switch (step) {
    case 'stopping':
      return 'Letting the background work finish';
    case 'safety_copy':
      return 'Taking a copy of the database as it is';
    case 'schema':
      return "Making the new database's schema";
    case 'rows':
      return "Loading the copy's rows";
    case 'checking':
      return 'Checking the rows, migrating, and checking against the library';
    case 'swapping':
      return 'Putting the restored database in place';
    case 'restarting':
      return 'Starting again on it';
  }
}

/** One finding of a restore's check against the library. */
export interface Finding {
  tone: Tone;
  text: string;
}

/** What a restore's check against the library found, in words. */
export function reportFindings(report: unknown): Finding[] {
  if (!report || typeof report !== 'object') return [];
  const r = report as Record<string, unknown>;
  const count = (key: string): number => {
    const value = r[key] as { count?: unknown } | undefined;
    return typeof value?.count === 'number' ? value.count : 0;
  };
  const findings: Finding[] = [];
  const missing = count('missing_objects');
  const wrong = count('wrong_size');
  const manifests = count('missing_manifests');
  const uncatalogued = count('uncatalogued');
  const registered = typeof r.registered === 'number' ? r.registered : 0;
  if (missing > 0)
    findings.push({
      tone: 'bad',
      text: `${missing} ${missing === 1 ? 'file the catalogue names is' : 'files the catalogue names are'} not in the library: restore a later copy of the library`,
    });
  if (wrong > 0)
    findings.push({
      tone: 'bad',
      text: `${wrong} ${wrong === 1 ? 'file is' : 'files are'} in the library at another size than the catalogue says`,
    });
  if (manifests > 0)
    findings.push({
      tone: 'bad',
      text: `${manifests} ${manifests === 1 ? 'artifact has' : 'artifacts have'} no manifest in the library`,
    });
  if (uncatalogued > registered)
    findings.push({
      tone: 'warn',
      text: `${uncatalogued - registered} ${uncatalogued - registered === 1 ? 'artifact was' : 'artifacts were'} imported after the copy was taken: the library has ${uncatalogued - registered === 1 ? 'it' : 'them'}, the catalogue does not`,
    });
  const unfinished = Array.isArray(r.unfinished) ? (r.unfinished as unknown[]) : [];
  for (const item of unfinished) {
    if (Array.isArray(item) && item.length === 3) {
      const [kind, state, n] = item as [string, string, number];
      findings.push({
        tone: 'idle',
        text: `${n} ${String(kind).replaceAll('_', ' ')} ${state}, as the copy recorded it: taken up again, or said why not`,
      });
    }
  }
  if (findings.length === 0)
    findings.push({ tone: 'ok', text: 'The catalogue and the library agree.' });
  return findings;
}

/** How many artifacts the library has that the restored catalogue lacks. */
export function uncataloguedCount(report: unknown): number {
  if (!report || typeof report !== 'object') return 0;
  const r = report as { uncatalogued?: { count?: unknown }; registered?: unknown };
  const found = typeof r.uncatalogued?.count === 'number' ? r.uncatalogued.count : 0;
  const registered = typeof r.registered === 'number' ? r.registered : 0;
  return Math.max(0, found - registered);
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
