// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import type { components } from './api-types';
import { apiCall, apiFetch, type Result } from './session';

export type Lineage = components['schemas']['LineageView'];
export type DerivationJob = components['schemas']['DerivationJobView'];
export type Derivation = components['schemas']['DerivationView'];
export type DerivationRequest = components['schemas']['DerivationRequestView'];

/** What an artifact was made from, what was made from it, and its jobs. */
export function loadLineage(
  artifactId: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<Lineage>> {
  return apiCall(fetcher, `/api/v1/artifacts/${encodeURIComponent(artifactId)}/lineage`);
}

/** Ask for a derivative. The server picks the transformation that suits. */
export function requestDerivative(
  artifactId: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<DerivationRequest>> {
  return apiCall(
    fetcher,
    `/api/v1/artifacts/${encodeURIComponent(artifactId)}/derivatives`,
    'POST',
    {},
  );
}

/** One derivation job. */
export function loadDerivationJob(
  jobId: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<DerivationJob>> {
  return apiCall(fetcher, `/api/v1/derivation-jobs/${encodeURIComponent(jobId)}`);
}

/** A transformation, in words. */
export function transformationLabel(transformation: string): string {
  switch (transformation) {
    case 'chd_create_cd':
      return 'CHD of a CD';
    case 'chd_create_dvd':
      return 'CHD of a DVD';
    default:
      return transformation;
  }
}

/**
 * What a derivative was shown to preserve, in words an operator can act on.
 * The two checked outcomes say so; anything else says it was not shown.
 */
export function lossLabel(loss: string): string {
  switch (loss) {
    case 'bit_exact_repack':
      return 'Checked: extracts back to exactly the original';
    case 'structurally_equivalent':
      return 'Checked: every track extracts back identical';
    case 'semantically_equivalent':
      return 'Same content, not the same bytes';
    case 'lossy':
      return 'Lossy: does not hold everything the original did';
    default:
      return 'Not checked against the original';
  }
}

/** Whether a derivative was shown to hold its original's data. */
export function isChecked(loss: string): boolean {
  return loss === 'bit_exact_repack' || loss === 'structurally_equivalent';
}

/** Whether a job is still going. */
export function isOpen(job: DerivationJob): boolean {
  return job.state === 'queued' || job.state === 'running' || job.state === 'failed_retryable';
}

/** How a job stands, in words. Never colour alone. */
export function jobStateLabel(job: DerivationJob): string {
  switch (job.state) {
    case 'queued':
      return 'Waiting to start';
    case 'running':
      return 'Being made';
    case 'complete':
      return 'Done';
    case 'failed_retryable':
      return 'Failed, will be tried again';
    case 'failed_terminal':
      return 'Failed';
    case 'canceled':
      return 'Withdrawn';
    default:
      return job.state;
  }
}
