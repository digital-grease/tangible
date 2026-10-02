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
