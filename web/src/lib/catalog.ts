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
export type TitleView = components['schemas']['TitleView'];
export type TitlePage = components['schemas']['TitlePage'];
export type EditionView = components['schemas']['EditionView'];
export type DiscSetView = components['schemas']['DiscSetView'];
export type DiscView = components['schemas']['DiscView'];
export type DiscArtifactView = components['schemas']['DiscArtifactView'];
export type CreateTitleRequest = components['schemas']['CreateTitleRequest'];
export type CreateEditionRequest = components['schemas']['CreateEditionRequest'];
export type CreateDiscSetRequest = components['schemas']['CreateDiscSetRequest'];
export type CreateDiscRequest = components['schemas']['CreateDiscRequest'];
export type EditTitleRequest = components['schemas']['EditTitleRequest'];
export type EditEditionRequest = components['schemas']['EditEditionRequest'];
export type EditDiscSetRequest = components['schemas']['EditDiscSetRequest'];
export type EditDiscRequest = components['schemas']['EditDiscRequest'];
export type CatalogDiscView = components['schemas']['CatalogDiscView'];
export type CatalogDiscPage = components['schemas']['CatalogDiscPage'];
export type CatalogueDiscRequest = components['schemas']['CatalogueDiscRequest'];

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

async function post<T>(
  fetcher: typeof fetch,
  path: string,
  body: unknown,
): Promise<CommandResult<T>> {
  const result = await request<T>(fetcher, path, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/**
 * Change a catalog row as it was read: `If-Match` names the version it was
 * read at, so a change somebody made since is refused rather than lost.
 */
async function patch<T>(
  fetcher: typeof fetch,
  path: string,
  version: number,
  body: unknown,
): Promise<CommandResult<T>> {
  const result = await request<T>(fetcher, path, {
    method: 'PATCH',
    headers: { 'content-type': 'application/json', 'if-match': `"${version}"` },
    body: JSON.stringify(body),
  });
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/** Change a title, as read at `version`. */
export function editTitle(
  id: string,
  version: number,
  body: EditTitleRequest,
  fetcher: typeof fetch = apiFetch,
): Promise<CommandResult<TitleView>> {
  return patch<TitleView>(fetcher, `/api/v1/titles/${encodeURIComponent(id)}`, version, body);
}

/** Change an edition, as read at `version`. */
export function editEdition(
  id: string,
  version: number,
  body: EditEditionRequest,
  fetcher: typeof fetch = apiFetch,
): Promise<CommandResult<EditionView>> {
  return patch<EditionView>(fetcher, `/api/v1/editions/${encodeURIComponent(id)}`, version, body);
}

/** Change a disc set, as read at `version`. */
export function editDiscSet(
  id: string,
  version: number,
  body: EditDiscSetRequest,
  fetcher: typeof fetch = apiFetch,
): Promise<CommandResult<DiscSetView>> {
  return patch<DiscSetView>(fetcher, `/api/v1/disc-sets/${encodeURIComponent(id)}`, version, body);
}

/** Change a disc, as read at `version`. */
export function editDisc(
  id: string,
  version: number,
  body: EditDiscRequest,
  fetcher: typeof fetch = apiFetch,
): Promise<CommandResult<DiscView>> {
  return patch<DiscView>(fetcher, `/api/v1/discs/${encodeURIComponent(id)}`, version, body);
}

/** Whether a refusal means somebody changed it since it was read. */
export function isStale(problem: Problem): boolean {
  return problem.code === 'RESOURCE_VERSION_CONFLICT';
}

/**
 * The fields of `form` that differ from `current`, for a PATCH: text
 * trimmed, and a blank one sent as null to clear it.
 */
export function changedFields(
  current: Record<string, unknown>,
  form: Record<string, string | number | null>,
): Record<string, string | number | null> {
  const changed: Record<string, string | number | null> = {};
  for (const [key, raw] of Object.entries(form)) {
    // A number input left empty may read as undefined: that is null too.
    const value = typeof raw === 'string' ? raw.trim() || null : (raw ?? null);
    if (value !== (current[key] ?? null)) changed[key] = value;
  }
  return changed;
}

/** Fetch titles in sort order, optionally filtered by name. */
export async function loadTitles(
  fetcher: typeof fetch = apiFetch,
  search?: string,
): Promise<LoadState<TitlePage>> {
  const query = search ? `?search=${encodeURIComponent(search)}` : '';
  const result = await request<TitlePage>(fetcher, `/api/v1/titles${query}`);
  if (isProblem(result)) return { kind: 'error', problem: result };
  // Empty is a distinct state, not a zero-length success: it needs its own
  // copy telling the operator what to do next.
  if (result.items.length === 0) return { kind: 'empty' };
  return { kind: 'ready', data: result };
}

/** The editions of a title. */
export async function loadEditions(
  titleId: string,
  fetcher: typeof fetch = apiFetch,
): Promise<CommandResult<EditionView[]>> {
  const result = await request<EditionView[]>(
    fetcher,
    `/api/v1/titles/${encodeURIComponent(titleId)}/editions`,
  );
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/** The disc sets of an edition. */
export async function loadDiscSets(
  editionId: string,
  fetcher: typeof fetch = apiFetch,
): Promise<CommandResult<DiscSetView[]>> {
  const result = await request<DiscSetView[]>(
    fetcher,
    `/api/v1/editions/${encodeURIComponent(editionId)}/disc-sets`,
  );
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/** The discs in a set. */
export async function loadDiscs(
  setId: string,
  fetcher: typeof fetch = apiFetch,
): Promise<CommandResult<DiscView[]>> {
  const result = await request<DiscView[]>(
    fetcher,
    `/api/v1/disc-sets/${encodeURIComponent(setId)}/discs`,
  );
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/** Record a title. */
export async function createTitle(
  body: CreateTitleRequest,
  fetcher: typeof fetch = apiFetch,
): Promise<CommandResult<TitleView>> {
  return post<TitleView>(fetcher, '/api/v1/titles', body);
}

/** Record an edition of a title. */
export async function createEdition(
  titleId: string,
  body: CreateEditionRequest,
  fetcher: typeof fetch = apiFetch,
): Promise<CommandResult<EditionView>> {
  return post<EditionView>(fetcher, `/api/v1/titles/${encodeURIComponent(titleId)}/editions`, body);
}

/** Record what an edition shipped as. */
export async function createDiscSet(
  editionId: string,
  body: CreateDiscSetRequest,
  fetcher: typeof fetch = apiFetch,
): Promise<CommandResult<DiscSetView>> {
  return post<DiscSetView>(
    fetcher,
    `/api/v1/editions/${encodeURIComponent(editionId)}/disc-sets`,
    body,
  );
}

/** Record one disc in a set. */
export async function createDisc(
  setId: string,
  body: CreateDiscRequest,
  fetcher: typeof fetch = apiFetch,
): Promise<CommandResult<DiscView>> {
  return post<DiscView>(fetcher, `/api/v1/disc-sets/${encodeURIComponent(setId)}/discs`, body);
}

/** Link an artifact to the disc it represents. */
export async function linkArtifact(
  discId: string,
  artifactId: string,
  fetcher: typeof fetch = apiFetch,
): Promise<CommandResult<DiscArtifactView[]>> {
  return post<DiscArtifactView[]>(
    fetcher,
    `/api/v1/discs/${encodeURIComponent(discId)}/artifact-links`,
    { artifact_id: artifactId },
  );
}

/**
 * Find discs to burn to: each word of `search` must be in where a disc
 * sits (its title, edition, set, name, label or year). Discs linked to
 * `artifact` come first; `id` finds only that disc.
 */
export async function findDiscs(
  query: { search?: string; artifact?: string; id?: string; limit?: number },
  fetcher: typeof fetch = apiFetch,
): Promise<CommandResult<CatalogDiscPage>> {
  const params = new URLSearchParams();
  if (query.search?.trim()) params.set('search', query.search.trim());
  if (query.artifact) params.set('artifact', query.artifact);
  if (query.id) params.set('id', query.id);
  if (query.limit) params.set('limit', String(query.limit));
  const result = await request<CatalogDiscPage>(fetcher, `/api/v1/discs?${params.toString()}`);
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/** The artifacts linked to a disc. */
export async function discArtifacts(
  discId: string,
  fetcher: typeof fetch = apiFetch,
): Promise<CommandResult<DiscArtifactView[]>> {
  const result = await request<DiscArtifactView[]>(
    fetcher,
    `/api/v1/discs/${encodeURIComponent(discId)}/artifacts`,
  );
  if (isProblem(result)) return { kind: 'error', problem: result };
  return { kind: 'ready', data: result };
}

/**
 * Catalogue a single-disc release in one step, linked to the artifact a
 * burn is for: the title (or the one already catalogued by that name, kind
 * and year), a new edition, a set of one disc, and disc 1.
 */
export async function catalogueDisc(
  body: CatalogueDiscRequest,
  fetcher: typeof fetch = apiFetch,
): Promise<CommandResult<CatalogDiscView>> {
  return post<CatalogDiscView>(fetcher, '/api/v1/discs', body);
}

/** The kinds of work, as a form offers them. */
export const TITLE_KINDS: readonly { value: string; label: string }[] = [
  'movie',
  'television',
  'game',
  'software',
  'operating_system',
  'music',
  'data_archive',
  'training',
  'unknown',
].map((value) => ({ value, label: titleKindLabel(value) }));

/** The media a disc can be, as a form offers them. */
export const MEDIA_FAMILIES: readonly { value: string; label: string }[] = [
  'cd',
  'dvd',
  'bluray',
  'uhd_bluray',
  'gd_rom',
  'unknown',
].map((value) => ({ value, label: mediaFamilyLabel(value) }));

/** Where a disc sits, in a line: "Hackers (1995) · NTSC DVD · Disc 1". */
export function discPlace(found: CatalogDiscView): string {
  const title = found.release_year ? `${found.title} (${found.release_year})` : found.title;
  // A set of one disc says nothing its edition does not.
  const set = found.disc_count_expected === 1 ? [] : [found.disc_set];
  return [title, found.edition, ...set, discName(found.disc)].join(' · ');
}

/** The kind of work a request's kind of item is. */
export function titleKindOfRequest(kind: string | null | undefined): string {
  switch (kind) {
    case 'game':
    case 'music':
    case 'movie':
    case 'television':
    case 'software':
      return kind;
    default:
      return 'unknown';
  }
}

/** The largest image a CD holds, about 807 MiB. */
const CD_BYTES = 846_364_848;
/** The largest image a dual-layer DVD holds. */
const DVD_DL_BYTES = 8_547_991_552;

/**
 * The medium an artifact most likely was, to start a new disc with: what its
 * manifest says, else a CD for a layout of tracks, else by its size. The
 * person cataloguing it sees it and can change it.
 */
export function likelyMedium(artifact: {
  format: string;
  total_bytes: number;
  disc_shape?: { media_family?: string | null } | null;
}): string {
  const said = artifact.disc_shape?.media_family;
  if (said && said !== 'unknown') return said;
  if (['cue_bin', 'toc_bin', 'ccd_img_sub'].includes(artifact.format)) return 'cd';
  if (artifact.format === 'bdmv_directory') return 'bluray';
  if (artifact.format === 'video_ts_directory') return 'dvd';
  if (artifact.total_bytes <= CD_BYTES) return 'cd';
  return artifact.total_bytes <= DVD_DL_BYTES ? 'dvd' : 'bluray';
}

// --- labels ---------------------------------------------------------------

/** Plain-language label for what kind of work a title is. */
export function titleKindLabel(kind: string): string {
  switch (kind) {
    case 'movie':
      return 'Film';
    case 'television':
      return 'Television';
    case 'game':
      return 'Game';
    case 'software':
      return 'Software';
    case 'operating_system':
      return 'Operating system';
    case 'music':
      return 'Music';
    case 'data_archive':
      return 'Data archive';
    case 'training':
      return 'Training';
    case 'custom':
      return 'Other';
    case 'unknown':
      return 'Unspecified';
    default:
      return kind;
  }
}

/** Plain-language label for what a medium is. */
export function mediaFamilyLabel(family: string): string {
  switch (family) {
    case 'cd':
      return 'CD';
    case 'dvd':
      return 'DVD';
    case 'bluray':
      return 'Blu-ray';
    case 'uhd_bluray':
      return 'UHD Blu-ray';
    case 'gd_rom':
      return 'GD-ROM';
    case 'proprietary_optical':
      return 'Proprietary optical';
    case 'unknown':
      return 'Unspecified';
    default:
      return family;
  }
}

/**
 * What reproduction of this disc is expected to achieve.
 *
 * Every one of these is a claim, and `unknown` is the honest default. The
 * project does not promise that a burned disc will satisfy console or player
 * authentication, so nothing here says it will.
 */
export function compatibilityLabel(claim: string): string {
  switch (claim) {
    case 'data_reproduction_expected':
      return 'The same bytes are expected to be reproducible';
    case 'player_compatibility_expected':
      return 'Expected to play in a compatible player';
    case 'emulator_compatibility_expected':
      return 'Expected to work in an emulator';
    case 'original_hardware_compatibility_expected':
      return 'Expected to work on original hardware';
    case 'known_not_reproducible':
      return 'Known not to be reproducible';
    case 'unknown':
      return 'Target compatibility unknown';
    default:
      return claim;
  }
}

/** How a disc is best described in a list. */
export function discName(disc: DiscView): string {
  return disc.display_name ?? `Disc ${disc.sequence_number}`;
}

/** Whether a set has as many discs as it says it shipped with. */
export function isComplete(set: DiscSetView): boolean {
  if (set.disc_count_expected === null || set.disc_count_expected === undefined) return true;
  return set.disc_count >= set.disc_count_expected;
}
