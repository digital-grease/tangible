// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// What identifies an artifact (docs/plans/dat-matching.md): the preservation
// hashes the server computes from its stored bytes, and the games of loaded
// DATs (Redump's, No-Intro's) they match. A match names a game; it renames
// and changes nothing.

import type { components } from './api-types';
import { apiCall, apiFetch, transportProblem, type Problem, type Result } from './session';

export type Identification = components['schemas']['IdentificationView'];
export type DatMatch = components['schemas']['DatMatchView'];
export type DatMatchFile = components['schemas']['DatMatchFileView'];
export type BestMatch = components['schemas']['BestMatchView'];
export type Dat = components['schemas']['DatView'];
export type DatLoad = components['schemas']['DatLoadView'];
export type DatList = components['schemas']['DatList'];

/** The largest DAT the server reads; a larger one is refused before it is sent. */
export const MAX_DAT_BYTES = 64 * 1024 * 1024;

/** How often hashes still being computed are looked at again, in milliseconds. */
export const FOLLOW_EVERY_MS = 5000;

/** An artifact's hashes and matches. */
export function loadIdentification(
  artifactId: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<Identification>> {
  return apiCall(fetcher, `/api/v1/artifacts/${encodeURIComponent(artifactId)}/identification`);
}

/** Compute an artifact's hashes again, after a failure or a restore. */
export function rehash(
  artifactId: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<Identification>> {
  return apiCall(
    fetcher,
    `/api/v1/artifacts/${encodeURIComponent(artifactId)}/identification/rehash`,
    'POST',
  );
}

/** Every loaded DAT. */
export function loadDats(fetcher: typeof fetch = apiFetch): Promise<Result<DatList>> {
  return apiCall(fetcher, '/api/v1/dats');
}

/** Remove a DAT, and every match it made. */
export function removeDat(id: string, fetcher: typeof fetch = apiFetch): Promise<Result<void>> {
  return apiCall(fetcher, `/api/v1/dats/${encodeURIComponent(id)}`, 'DELETE');
}

/**
 * Load a DAT: the file's XML as the request body, as the server reads it.
 * A file over the limit is refused here, with the same words the server
 * would use, rather than sent.
 */
export async function uploadDat(
  file: Pick<File, 'size'> & Blob,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<DatLoad>> {
  if (file.size > MAX_DAT_BYTES) {
    return {
      kind: 'error',
      problem: datTooLarge(file.size),
      retryAfter: null,
    };
  }
  let response: Response;
  try {
    response = await fetcher('/api/v1/dats', {
      method: 'POST',
      headers: { 'content-type': 'application/xml' },
      body: file,
    });
  } catch (error) {
    return {
      kind: 'error',
      problem: transportProblem(error instanceof Error ? error.message : 'unknown failure'),
      retryAfter: null,
    };
  }
  // The server's body limit answers before any handler, in plain text.
  if (response.status === 413) {
    return { kind: 'error', problem: datTooLarge(null), retryAfter: null };
  }
  let parsed: unknown;
  try {
    parsed = await response.json();
  } catch {
    return {
      kind: 'error',
      problem: transportProblem(`the server returned ${response.status} with no readable body`),
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
  return { kind: 'ok', data: parsed as DatLoad };
}

function datTooLarge(size: number | null): Problem {
  return {
    type: 'about:blank',
    title: 'Not a DAT this server reads',
    status: 413,
    code: 'DAT_INVALID',
    detail: `the file is ${size === null ? 'larger than' : `${size} bytes, more than`} the ${MAX_DAT_BYTES} bytes a DAT may be; unzip it if it is zipped, or load the system's DAT on its own`,
  };
}

/** Whether the hashes are still to come. */
export function isPending(view: Pick<Identification, 'state'>): boolean {
  return view.state === 'pending';
}

/** The hashes' state, in words. Never colour alone. */
export function identificationStateLabel(view: Pick<Identification, 'state' | 'error'>): string {
  switch (view.state) {
    case 'done':
      return 'Hashed';
    case 'pending':
      return 'Hashes are being computed in the background';
    case 'failed':
      return view.error ? `Could not be hashed: ${view.error}` : 'Could not be hashed';
    default:
      return view.state;
  }
}

/** A match's kind, in words, with what matched. */
export function matchLabel(match: DatMatch): string {
  if (match.kind === 'exact') {
    const every = match.files_total === 1 ? 'its file' : `all ${match.files_total} of its files`;
    return `Matches ${match.game_name}, ${every}`;
  }
  return `Partly matches ${match.game_name}: ${match.files_matched} of ${match.files_total} files`;
}

/** Where a match comes from: the DAT and its version. */
export function matchSource(match: Pick<DatMatch, 'dat_name' | 'dat_version'>): string {
  return match.dat_version ? `${match.dat_name} (${match.dat_version})` : match.dat_name;
}

/** What a partial match most often means, for one with tracks that did not match. */
export function partialHint(match: DatMatch): string | null {
  if (match.kind !== 'partial') return null;
  const unmatched = match.files.filter((file) => !file.matched_by).length;
  if (unmatched === 0) return null;
  return 'A file that differs is often an audio track read without offset correction, or a different revision of the disc.';
}

/** One file of a matched game: what of the artifact matched it, or that nothing did. */
export function matchedByLabel(file: DatMatchFile): string {
  return file.matched_by ? `matched by ${file.matched_by}` : 'not matched';
}

/** The library list's word for an artifact's best match. */
export function bestMatchLabel(best: BestMatch | null | undefined): string | null {
  if (!best) return null;
  return best.kind === 'exact' ? best.game_name : `${best.game_name} (partly)`;
}

/** A DAT's line in a list: its version and what it holds. */
export function datSummary(dat: Dat): string {
  const games = `${dat.game_count} game${dat.game_count === 1 ? '' : 's'}`;
  const matched =
    dat.matched_artifacts === 0
      ? 'identifies nothing in the library yet'
      : `identifies ${dat.matched_artifacts} artifact${dat.matched_artifacts === 1 ? '' : 's'}`;
  return `${games}; ${matched}`;
}

/** What loading a DAT came to, in one sentence. */
export function loadOutcome(load: DatLoad): string {
  if (load.unchanged)
    return `${load.dat.name} was already loaded from this very file; nothing changed.`;
  const verb = load.replaced ? 'replaced the DAT of the same name' : 'was loaded';
  const skipped =
    load.warning_count === 0
      ? ''
      : ` ${load.warning_count} entr${load.warning_count === 1 ? 'y was' : 'ies were'} left out.`;
  return `${load.dat.name} ${verb}: ${datSummary(load.dat)}.${skipped}`;
}
