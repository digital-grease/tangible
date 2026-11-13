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

/** A detail's name, in words. */
export function detailLabel(name: string): string {
  return DETAIL_FIELDS.find((field) => field.name === name)?.label ?? name;
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
