// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// Requests for items (ADR 0021, docs/plans/requests.md): somebody asks for
// an item they own, attesting to it on every request; an operator or
// administrator approves or declines. The server decides who may do what to
// a request and says so in each one's `can_*` fields; this only shows it.

import type { Tone } from './components/ui/tone';
import type { components } from './api-types';
import { apiCall, apiFetch, type Result } from './session';

export type RequestView = components['schemas']['RequestView'];
export type RequestPage = components['schemas']['RequestPage'];
export type RequestRef = components['schemas']['RequestRefView'];
export type CreateItemRequest = components['schemas']['CreateItemRequest'];
export type MetadataSearchView = components['schemas']['MetadataSearchView'];
export type MetadataCandidate = components['schemas']['MetadataCandidateView'];
export type MetadataSource = components['schemas']['MetadataSourceView'];
export type ReleaseSearchView = components['schemas']['ReleaseSearchView'];
export type ReleaseView = components['schemas']['ReleaseView'];
export type GrabView = components['schemas']['GrabView'];
export type DownloadClientChoice = components['schemas']['DownloadClientChoiceView'];
export type ImportView = components['schemas']['ImportView'];

/** One choice of a select, as `Select` takes it. */
export interface Choice {
  value: string;
  label: string;
}

/** The words beside the checkbox, as the server states them (ADR 0021). */
export const ATTESTATION = 'I legally own this item and want a digital image of it.';

/** The kinds of item a request may be for, as the form offers them. */
export const KINDS: readonly Choice[] = [
  { value: 'game', label: 'Game' },
  { value: 'music', label: 'Music' },
  { value: 'movie', label: 'Film' },
  { value: 'television', label: 'Television' },
  { value: 'software', label: 'Software' },
  { value: 'other', label: 'Something else' },
];

/** A kind, in words. */
export function kindLabel(kind: string): string {
  return KINDS.find((choice) => choice.value === kind)?.label ?? kind;
}

/** The states a list may be narrowed to, in the order a person thinks of them. */
export const STATE_FILTERS: readonly Choice[] = [
  { value: '', label: 'Every request' },
  { value: 'pending_approval', label: 'Waiting for approval' },
  { value: 'approved', label: 'Approved' },
  { value: 'downloading', label: 'Downloading' },
  { value: 'fulfilled', label: 'In the library' },
  { value: 'failed', label: 'Download failed' },
  { value: 'declined', label: 'Declined' },
  { value: 'canceled', label: 'Withdrawn' },
];

/** A request's state, in words. Never colour alone. */
export function requestStateLabel(state: string): string {
  return STATE_FILTERS.find((choice) => choice.value === state)?.label ?? state;
}

/** A request's state as a tone, for a status mark. */
export function requestStateTone(state: string): Tone {
  switch (state) {
    case 'pending_approval':
      return 'warn';
    case 'downloading':
      return 'busy';
    case 'fulfilled':
      return 'ok';
    case 'declined':
    case 'failed':
      return 'bad';
    default:
      return 'idle';
  }
}

/** What happens next to a request in this state, said to whoever looks at it. */
export function requestNext(request: Pick<RequestView, 'state'>): string {
  switch (request.state) {
    case 'pending_approval':
      return 'An operator or administrator looks at it next. Nothing is searched for or downloaded until one approves it.';
    case 'approved':
      return 'Approved. An operator or administrator finds a release for it next.';
    case 'downloading':
      return 'A download client is fetching it. It is imported into the library when the download finishes.';
    case 'fulfilled':
      return 'It is in the library.';
    case 'failed':
      return 'The download did not end in the library. An operator or administrator may find another release.';
    case 'declined':
      return 'Declined. Nothing was searched for or downloaded.';
    case 'canceled':
      return 'Withdrawn. Nothing was searched for or downloaded after it was withdrawn.';
    default:
      return '';
  }
}

/** The detail fields the form offers, by the name the server stores them under. */
export const DETAIL_FIELDS = [
  { name: 'platform', label: 'Platform or system', hint: 'Such as Sony PlayStation 2.' },
  { name: 'year', label: 'Year', hint: 'When it came out.' },
  { name: 'region', label: 'Region', hint: 'Such as USA, Europe or Japan.' },
  { name: 'edition', label: 'Edition', hint: "Such as Greatest Hits, or Director's Cut." },
] as const;

export type DetailName = (typeof DETAIL_FIELDS)[number]['name'];

/** The details filled in, without the empty ones. */
export function detailsOf(values: Partial<Record<DetailName, string>>): Record<string, string> {
  const details: Record<string, string> = {};
  for (const { name } of DETAIL_FIELDS) {
    const value = values[name]?.trim();
    if (value) details[name] = value;
  }
  return details;
}

/** Details a provider names that the form has no field for, in words. */
const OTHER_DETAILS: Record<string, string> = {
  artist: 'Artist',
  format: 'Format',
  catalog_number: 'Catalogue number',
  barcode: 'Barcode',
  original_title: 'Original title',
  description: 'Description',
};

/** A detail's name, in words. */
export function detailLabel(name: string): string {
  return (
    DETAIL_FIELDS.find((field) => field.name === name)?.label ??
    OTHER_DETAILS[name] ??
    name.replace(/_/g, ' ')
  );
}

/** Whether the form has a field for a detail. */
export function isFormDetail(name: string): name is DetailName {
  return DETAIL_FIELDS.some((field) => field.name === name);
}

/** A source of metadata, in words. */
export function providerLabel(provider: string): string {
  switch (provider) {
    case 'dat':
      return 'DATs';
    case 'musicbrainz':
      return 'MusicBrainz';
    case 'tmdb':
      return 'TMDB';
    case 'igdb':
      return 'IGDB';
    case 'imdb':
      return 'IMDb';
    default:
      return provider;
  }
}

/** What to say about a source that did not answer; nothing for one that did. */
export function sourceNote(source: MetadataSource): string | null {
  if (source.state === 'answered') return null;
  const name = providerLabel(source.provider);
  switch (source.state) {
    case 'unconfigured':
      return source.detail ?? `${name} is not configured.`;
    case 'off':
      return `${name} is turned off.`;
    default:
      return `${name} did not answer: ${source.detail ?? source.error_code ?? 'no reason given'}`;
  }
}

/** A candidate's details, in words, for a line under its title. */
export function candidateLine(candidate: MetadataCandidate): string {
  const order = ['artist', 'platform', 'year', 'region', 'format', 'catalog_number'];
  const shown = order
    .map((name) => candidate.details[name])
    .filter((value): value is string => !!value);
  return [providerLabel(candidate.provider), ...shown].join(' · ');
}

/** Search metadata for an item, by words or an IMDb ID or address. */
export function searchMetadata(
  query: string,
  kind: string | null,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<MetadataSearchView>> {
  const params = new URLSearchParams({ q: query });
  if (kind) params.set('kind', kind);
  return apiCall(fetcher, `/api/v1/metadata/search?${params.toString()}`);
}

/**
 * The details and references a request is made with: the picked
 * candidate's, what the form's fields say over them, and an IMDb ID typed.
 */
export function requestFields(
  picked: Pick<MetadataCandidate, 'details' | 'refs'> | null,
  form: Partial<Record<DetailName, string>>,
  imdb: string | null,
): { details: Record<string, string>; refs: RequestRef[] } {
  const details: Record<string, string> = {};
  for (const [name, value] of Object.entries(picked?.details ?? {})) {
    if (!isFormDetail(name)) details[name] = value;
  }
  Object.assign(details, detailsOf(form));
  const refs: RequestRef[] = [...(picked?.refs ?? [])];
  if (imdb && !refs.some((ref) => ref.provider === 'imdb' && ref.external_id === imdb)) {
    refs.push({ provider: 'imdb', external_id: imdb });
  }
  return { details, refs };
}

/**
 * The IMDb title ID in what a person typed: an ID (`tt0083658`) or an IMDb
 * address. `null` for nothing, `undefined` for something that is neither.
 */
export function imdbId(text: string): string | null | undefined {
  const trimmed = text.trim();
  if (!trimmed) return null;
  if (/^tt\d{7,10}$/.test(trimmed)) return trimmed;
  let url: URL;
  try {
    url = new URL(trimmed);
  } catch {
    return undefined;
  }
  const host = url.hostname.toLowerCase();
  if (host !== 'imdb.com' && !host.endsWith('.imdb.com')) return undefined;
  return /\/title\/(tt\d{7,10})(?:\/|$)/.exec(url.pathname)?.[1] ?? undefined;
}

/** A reference, in words, with where it leads when it leads anywhere. */
export function refLabel(ref: RequestRef): { label: string; href: string | null } {
  switch (ref.provider) {
    case 'imdb':
      return {
        label: `IMDb ${ref.external_id}`,
        href: `https://www.imdb.com/title/${ref.external_id}/`,
      };
    case 'tmdb':
      return { label: `TMDB ${ref.external_id}`, href: null };
    case 'musicbrainz':
      return {
        label: `MusicBrainz ${ref.external_id}`,
        href: `https://musicbrainz.org/release/${encodeURIComponent(ref.external_id)}`,
      };
    case 'igdb':
      return { label: `IGDB ${ref.external_id}`, href: null };
    case 'dat':
      return { label: `DAT: ${ref.external_id}`, href: null };
    default:
      return { label: `${ref.provider} ${ref.external_id}`, href: null };
  }
}

/** Requests, newest first: a viewer's own, or everybody's for a decider. */
export function loadRequests(
  options: { state?: string; cursor?: string | null; limit?: number } = {},
  fetcher: typeof fetch = apiFetch,
): Promise<Result<RequestPage>> {
  const query = new URLSearchParams({ limit: String(options.limit ?? 50) });
  if (options.state) query.set('state', options.state);
  if (options.cursor) query.set('cursor', options.cursor);
  return apiCall(fetcher, `/api/v1/requests?${query.toString()}`);
}

/** One request. */
export function loadRequest(
  id: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<RequestView>> {
  return apiCall(fetcher, `/api/v1/requests/${encodeURIComponent(id)}`);
}

/** Ask for an item. */
export function createRequest(
  request: CreateItemRequest,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<RequestView>> {
  return apiCall(fetcher, '/api/v1/requests', 'POST', request);
}

/** Approve a request that waits. */
export function approveRequest(
  id: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<RequestView>> {
  return apiCall(fetcher, `/api/v1/requests/${encodeURIComponent(id)}/approve`, 'POST', {});
}

/** Decline a request, saying why. */
export function declineRequest(
  id: string,
  reason: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<RequestView>> {
  return apiCall(fetcher, `/api/v1/requests/${encodeURIComponent(id)}/decline`, 'POST', {
    reason,
  });
}

/** Withdraw a request. */
export function cancelRequest(
  id: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<RequestView>> {
  return apiCall(fetcher, `/api/v1/requests/${encodeURIComponent(id)}/cancel`, 'POST', {});
}

/** Why a request was refused, in words a requester can act on, where the detail will not do. */
export function requestRefusal(code: string): string | null {
  switch (code) {
    case 'ATTESTATION_REQUIRED':
      return 'Tick the box to say you own this item. A request cannot be made without it.';
    case 'RATE_LIMITED':
      return 'This account has made many requests in the last hour. Try again later.';
    default:
      return null;
  }
}

// --- finding a release (ADR 0021, docs/plans/requests.md Q3 and Q4) ---------

/**
 * Search Prowlarr for an approved request's releases: by its title, or by
 * `words` when a person gives them.
 */
export function searchReleases(
  id: string,
  words: string | null,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<ReleaseSearchView>> {
  const trimmed = words?.trim();
  return apiCall(
    fetcher,
    `/api/v1/requests/${encodeURIComponent(id)}/search`,
    'POST',
    trimmed ? { query: trimmed } : {},
  );
}

/** The search held for a request, for an hour after it was made. */
export function loadReleaseSearch(
  id: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<ReleaseSearchView>> {
  return apiCall(fetcher, `/api/v1/requests/${encodeURIComponent(id)}/search`);
}

/** Grab a release from the search held, for the client chosen or the only one. */
export function grabRelease(
  id: string,
  resultId: string,
  integrationId: string | null,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<GrabView>> {
  return apiCall(fetcher, `/api/v1/requests/${encodeURIComponent(id)}/grab`, 'POST', {
    result_id: resultId,
    ...(integrationId ? { integration_id: integrationId } : {}),
  });
}

/** The import a request's download is followed by. */
export function loadRequestImport(
  importId: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<ImportView>> {
  return apiCall(fetcher, `/api/v1/imports/${encodeURIComponent(importId)}`);
}

/** How a release is fetched, in words. */
export function protocolLabel(protocol: string): string {
  switch (protocol) {
    case 'torrent':
      return 'Torrent';
    case 'usenet':
      return 'Usenet';
    default:
      return protocol;
  }
}

/** How old something posted at `published` is, in words: days, months or years. */
export function releaseAge(published: string | null | undefined, now = Date.now()): string {
  if (!published) return 'Not said';
  const at = Date.parse(published);
  if (Number.isNaN(at)) return 'Not said';
  const days = Math.max(0, Math.floor((now - at) / 86_400_000));
  if (days === 0) return 'Today';
  if (days === 1) return '1 day';
  if (days < 60) return `${days} days`;
  const months = Math.floor(days / 30.44);
  if (months < 24) return `${months} months`;
  return `${Math.floor(days / 365.25)} years`;
}

/** Who has a release: a torrent's seeders and leechers, or an NZB's grabs. */
export function releasePeers(release: ReleaseView): string {
  if (release.protocol === 'torrent') {
    if (release.seeders == null) return 'Not said';
    const leeching = release.leechers == null ? '' : `, ${release.leechers} leeching`;
    return `${release.seeders} seeding${leeching}`;
  }
  return release.grabs == null ? 'Not said' : `${release.grabs} grabs`;
}

/** The download clients that take `protocol`. */
export function clientsFor(
  search: Pick<ReleaseSearchView, 'clients'>,
  protocol: string,
): DownloadClientChoice[] {
  return search.clients.filter((client) => client.protocol === protocol);
}

/** What was searched for, in words. */
export function searchedFor(search: Pick<ReleaseSearchView, 'searched_by' | 'query'>): string {
  return search.searched_by === 'imdb_id'
    ? `Searched by IMDb ID ${search.query}`
    : `Searched for “${search.query}”`;
}

/** What a grab did, in words, naming the client it went to. */
export function grabOutcome(grab: Pick<GrabView, 'outcome'>, client: string): string {
  switch (grab.outcome) {
    case 'resubmitted':
      return `Handed to ${client} again: the earlier import of the same download ended without reaching the library.`;
    case 'already_active':
      return `${client} already has this download; the request follows it.`;
    case 'already_imported':
      return 'The library already holds what this release downloads, so the request is fulfilled.';
    default:
      return `Handed to ${client}. The request follows the download, and is fulfilled once it is in the library.`;
  }
}

/** Why a search or grab was refused, in words to act on, where the detail will not do. */
export function releaseRefusal(code: string): string | null {
  switch (code) {
    case 'SEARCH_EXPIRED':
      return 'That search is over an hour old, or the server has restarted since. Search again, then pick a release.';
    case 'RATE_LIMITED':
      return 'This account has searched many times in the last hour. Try again later.';
    case 'INTEGRATION_AUTH_FAILED':
      return "Prowlarr refused the API key, or an indexer refused the release's link. An administrator can check the key on the Integrations page.";
    default:
      return null;
  }
}
