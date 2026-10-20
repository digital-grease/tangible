// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// Ripping the disc in a drive into the library (ADR 0013). The server and
// the drive's worker do the work; this asks for it, shows how it is going,
// and lets a person withdraw it while reading.

import type { components } from './api-types';
import { apiCall, apiFetch, type Result } from './session';

export type RipView = components['schemas']['RipView'];
export type RipPage = components['schemas']['RipPage'];
export type RipEventView = components['schemas']['RipEventView'];

/** Ask for the disc in a drive to be ripped. */
export function requestRip(
  driveId: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<RipView>> {
  return apiCall(fetcher, `/api/v1/drives/${encodeURIComponent(driveId)}/rips`, 'POST', {});
}

/** Recent rips, newest first. */
export function loadRips(fetcher: typeof fetch = apiFetch): Promise<Result<RipPage>> {
  return apiCall(fetcher, '/api/v1/rips?limit=20');
}

/** One rip. */
export function loadRip(id: string, fetcher: typeof fetch = apiFetch): Promise<Result<RipView>> {
  return apiCall(fetcher, `/api/v1/rips/${encodeURIComponent(id)}`);
}

/** What the worker said while ripping. */
export function loadRipEvents(
  id: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<components['schemas']['RipEventPage']>> {
  return apiCall(fetcher, `/api/v1/rips/${encodeURIComponent(id)}/events`);
}

/** Withdraw a rip before its import starts. */
export function cancelRip(id: string, fetcher: typeof fetch = apiFetch): Promise<Result<RipView>> {
  return apiCall(fetcher, `/api/v1/rips/${encodeURIComponent(id)}/cancel`, 'POST');
}

/** A rip's state, in words. Never colour alone. */
export function ripStateLabel(state: string): string {
  switch (state) {
    case 'queued':
      return 'Waiting for the drive';
    case 'reading':
      return 'Reading the disc';
    case 'uploading':
      return 'Sending what was read';
    case 'registering':
      return 'Importing';
    case 'complete':
      return 'In the library';
    case 'canceled':
      return 'Withdrawn';
    case 'refused':
      return 'Not ripped';
    case 'failed':
      return 'Failed';
    default:
      return state;
  }
}

/** Why a rip did not happen, in words an operator can act on. */
export function ripRefusal(code: string | null | undefined): string | null {
  switch (code) {
    case 'NO_MEDIUM':
    case 'UNSUITABLE_MEDIUM':
      return 'There was no disc in the drive. Put one in and ask again.';
    case 'BLANK_MEDIUM':
      return 'The disc in the drive is blank; there is nothing on it to rip.';
    case 'MEDIUM_UNSUPPORTED':
      return 'Only CDs can be ripped so far. DVDs and Blu-rays come later.';
    case 'WORKER_RESTARTED':
      return 'The worker restarted while ripping. Ask again; reading does not harm the disc.';
    case 'IMPORT_FAILED':
      return 'The disc was read, but importing what was read failed.';
    default:
      return null;
  }
}

/** Whether a rip is still worth watching. */
export function ripIsLive(rip: Pick<RipView, 'is_open'>): boolean {
  return rip.is_open;
}

/** How far a rip has got, from its newest event with progress, as a whole percent. */
export function ripProgress(events: RipEventView[]): number | null {
  for (let i = events.length - 1; i >= 0; i--) {
    const progress = events[i]?.progress;
    if (progress !== null && progress !== undefined) {
      return Math.round(Math.min(1, Math.max(0, progress)) * 100);
    }
  }
  return null;
}
