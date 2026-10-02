// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import type { components } from './api-types';
import { apiFetch } from './session';
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
