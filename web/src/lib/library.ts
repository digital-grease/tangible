// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import type { components } from './api-types';
import { apiFetch } from './session';

/**
 * Wire types come from the generated OpenAPI client, never hand-written.
 * A drift between server and browser then becomes a type error rather than a
 * runtime surprise.
 */
export type ArtifactSummary = components['schemas']['ArtifactSummary'];
export type ArtifactDetail = components['schemas']['ArtifactDetail'];
export type ArtifactSummaryPage = components['schemas']['ArtifactSummaryPage'];
export type ComponentView = components['schemas']['ComponentView'];
export type Problem = components['schemas']['Problem'];

/**
 * What a view can be showing.
 *
 * Every asynchronous view represents loading, empty, error and success. They
 * are modelled as one union so a template cannot accidentally render two at
 * once, or forget one.
 */
export type LoadState<T> =
  | { kind: 'loading' }
  | { kind: 'empty' }
  | { kind: 'ready'; data: T }
  | { kind: 'error'; problem: Problem };

/**
 * A problem to show when the server did not return one.
 *
 * A transport failure has no document, but the view still needs something with
 * the same shape so error rendering has a single path.
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

async function request<T>(fetcher: typeof fetch, path: string): Promise<T | Problem> {
  let response: Response;
  try {
    response = await fetcher(path);
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
    // Problem documents carry a `code`; anything else from a non-OK response
    // is not one and gets wrapped so callers never see two error shapes.
    const problem = body as Partial<Problem>;
    return typeof problem?.code === 'string'
      ? (body as Problem)
      : transportProblem(`the server returned ${response.status}`);
  }
  return body as T;
}

function isProblem(value: unknown): value is Problem {
  return typeof value === 'object' && value !== null && 'code' in value && 'detail' in value;
}

/** Fetch one page of artifacts. */
export async function loadLibrary(
  fetcher: typeof fetch = apiFetch,
  cursor?: string,
): Promise<LoadState<ArtifactSummaryPage>> {
  const query = cursor ? `?cursor=${encodeURIComponent(cursor)}` : '';
  const result = await request<ArtifactSummaryPage>(fetcher, `/api/v1/artifacts${query}`);

  if (isProblem(result)) return { kind: 'error', problem: result };
  // Empty is a distinct state, not a zero-length success: it needs its own
  // copy telling the operator what to do next.
  if (result.items.length === 0) return { kind: 'empty' };
  return { kind: 'ready', data: result };
}

/** Fetch one artifact. */
export async function loadArtifact(
  id: string,
  fetcher: typeof fetch = apiFetch,
): Promise<LoadState<ArtifactDetail>> {
  const result = await request<ArtifactDetail>(
    fetcher,
    `/api/v1/artifacts/${encodeURIComponent(id)}`,
  );
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/** Render a byte count for a human. */
export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KiB', 'MiB', 'GiB', 'TiB'];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  // One decimal place: enough to distinguish 4.7 GiB from 4.4 GiB, which is
  // the difference between fitting on a DVD and not.
  return `${value.toFixed(1)} ${units[unit]}`;
}

/**
 * Plain-language validation label.
 *
 * Deliberately never "perfect copy" or "will work". The tool says what it
 * checked, not what it hopes.
 */
export function validationLabel(state: string): string {
  switch (state) {
    case 'valid':
      return 'Structurally valid';
    case 'valid_with_warnings':
      return 'Structurally valid, with findings';
    case 'invalid':
      return 'Structurally invalid';
    case 'unsupported':
      return 'Format not supported';
    case 'quarantined':
      return 'Quarantined';
    case 'pending':
      return 'Not yet validated';
    default:
      return state;
  }
}

/** A short symbol accompanying validation state, so status is never colour alone. */
export function validationMark(state: string): string {
  switch (state) {
    case 'valid':
      return '✓';
    case 'valid_with_warnings':
      return '!';
    case 'invalid':
    case 'quarantined':
      return '✕';
    default:
      return '·';
  }
}

/** Human-readable format name. */
export function formatLabel(format: string): string {
  switch (format) {
    case 'iso':
      return 'ISO';
    case 'cue_bin':
      return 'CUE/BIN';
    case 'toc_bin':
      return 'TOC/BIN';
    case 'ccd_img_sub':
      return 'CCD/IMG/SUB';
    case 'mds_mdf':
      return 'MDS/MDF';
    case 'chd':
      return 'CHD';
    case 'bdmv_directory':
      return 'BDMV';
    case 'video_ts_directory':
      return 'VIDEO_TS';
    case 'raw':
      return 'Raw';
    case 'unknown':
      return 'Unrecognised';
    default:
      return format;
  }
}

export type DiscLayout = components['schemas']['DiscLayout'];
export type TrackView = components['schemas']['TrackView'];

/** Sectors, or frames, in one second of a CD. */
const FRAMES_PER_SECOND = 75;

/** A sector count as `MM:SS:FF`, the way a CD player and a TOC count. */
export function msf(sectors: number): string {
  const frames = sectors % FRAMES_PER_SECOND;
  const seconds = Math.floor(sectors / FRAMES_PER_SECOND) % 60;
  const minutes = Math.floor(sectors / FRAMES_PER_SECOND / 60);
  const two = (n: number) => String(n).padStart(2, '0');
  return `${two(minutes)}:${two(seconds)}:${two(frames)}`;
}

/** A track's mode in words. */
export function trackModeLabel(mode: string): string {
  switch (mode.toUpperCase()) {
    case 'AUDIO':
      return 'Audio';
    case 'MODE1/2048':
      return 'Data, mode 1 (2048-byte sectors)';
    case 'MODE1/2352':
      return 'Data, mode 1 (raw sectors)';
    case 'MODE2/2048':
      return 'Data, mode 2 form 1';
    case 'MODE2/2324':
      return 'Data, mode 2 form 2';
    case 'MODE2/2336':
      return 'Data, mode 2 (2336-byte sectors)';
    case 'MODE2/2352':
      return 'Data, mode 2 (raw sectors)';
    default:
      return mode;
  }
}

/** A track flag in words. */
export function trackFlagLabel(flag: string): string {
  switch (flag) {
    case 'DCP':
      return 'copying permitted';
    case '4CH':
      return 'four-channel audio';
    case 'PRE':
      return 'pre-emphasis';
    case 'SCMS':
      return 'serial copy management';
    default:
      return flag;
  }
}

/** Where to download one component of an artifact. */
export function componentUrl(artifactId: string, componentId: string): string {
  return `/api/v1/artifacts/${encodeURIComponent(artifactId)}/components/${encodeURIComponent(componentId)}/content`;
}

/** One line saying what a disc of tracks is. */
export function discSummary(disc: DiscLayout): string {
  const tracks = `${disc.tracks.length} track${disc.tracks.length === 1 ? '' : 's'}`;
  const sessions = disc.session_count > 1 ? ` in ${disc.session_count} sessions` : '';
  const catalog = disc.catalog ? `, catalogue number ${disc.catalog}` : '';
  return `${tracks}${sessions}${catalog}.`;
}

/** What else there is to say about a track: its code and its flags. */
export function trackDetails(track: TrackView): string {
  const parts = [];
  if (track.isrc) parts.push(`ISRC ${track.isrc}`);
  parts.push(...track.flags.map(trackFlagLabel));
  return parts.join('; ');
}
