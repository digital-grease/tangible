// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import type { components } from './api-types';
import { apiCall, apiFetch, type Result } from './session';

export type IssuedEnrollment = components['schemas']['IssuedEnrollment'];

/** Shortest and longest lifetimes the server accepts for a token, in minutes. */
export const ENROLLMENT_MINUTES = { min: 1, default: 15, max: 60 } as const;

/** Issue a one-use token a new burn worker exchanges for its credential. */
export function issueEnrollment(
  minutes: number,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<IssuedEnrollment>> {
  return apiCall(fetcher, '/api/v1/worker-enrollments', 'POST', { expires_in_minutes: minutes });
}

export type DriveView = components['schemas']['DriveView'];
export type ErasureView = components['schemas']['ErasureView'];
export type ErasureMode = 'quick' | 'full';

/** Every drive, with its worker. */
export function loadDrives(
  fetcher: typeof fetch = apiFetch,
): Promise<Result<components['schemas']['DriveList']>> {
  return apiCall(fetcher, '/api/v1/drives');
}

/** Recent erasures, newest first. */
export function loadErasures(
  fetcher: typeof fetch = apiFetch,
): Promise<Result<components['schemas']['ErasureList']>> {
  return apiCall(fetcher, '/api/v1/erasures?limit=20');
}

/**
 * Ask for the disc in a drive to be erased.
 *
 * The confirmation is sent as given rather than forced to true here: the page
 * passes what the person ticked, so a request that was not confirmed is one
 * the server refuses rather than one this function quietly fixed.
 */
export function requestErasure(
  driveId: string,
  mode: ErasureMode,
  confirmDataLoss: boolean,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<ErasureView>> {
  return apiCall(fetcher, `/api/v1/drives/${encodeURIComponent(driveId)}/erasures`, 'POST', {
    mode,
    confirm_data_loss: confirmDataLoss,
  });
}

/** Withdraw an erasure the worker has not started. */
export function cancelErasure(
  erasureId: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<ErasureView>> {
  return apiCall(fetcher, `/api/v1/erasures/${encodeURIComponent(erasureId)}/cancel`, 'POST');
}

/** An erasure's state, in words. Never colour alone. */
export function erasureStateLabel(state: string): string {
  switch (state) {
    case 'queued':
      return 'Waiting for the worker';
    case 'erasing':
      return 'Erasing';
    case 'erased':
      return 'Erased';
    case 'already_blank':
      return 'Already blank, nothing done';
    case 'refused':
      return 'Not erased';
    case 'failed':
      return 'Failed';
    case 'canceled':
      return 'Withdrawn';
    default:
      return state;
  }
}

/** Whether any erasure in a list is still going, so the page keeps checking. */
export function anyOpen(erasures: ErasureView[]): boolean {
  return erasures.some((erasure) => erasure.is_open);
}
