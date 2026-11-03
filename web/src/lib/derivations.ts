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
    case 'decrypt_css':
      return 'Decrypted copy of a DVD, made as it was ripped';
    case 'cue_bin_from_ccd':
      return 'CUE/BIN of a CloneCD image';
    case 'cue_bin_from_mds':
      return 'CUE/BIN of an Alcohol 120% image';
    case 'iso_from_mds':
      return 'ISO of an Alcohol 120% DVD image';
    default:
      return transformation;
  }
}

/** How the page offers the derivative that suits an artifact. */
export type DerivativeOffer = {
  /** The button. */
  action: string;
  /** What it makes, and what it costs. */
  explanation: string;
  /** What to say when it was made already. */
  exists: string;
};

/** The offer for a suggested transformation. */
export function derivativeOffer(transformation: string): DerivativeOffer {
  if (transformation === 'cue_bin_from_ccd') {
    return {
      action: 'Make a CUE/BIN',
      explanation:
        'Makes a CUE/BIN of this CloneCD image, which can be burned, made into a CHD and sent ' +
        'to RomM. It drops the subchannel and the raw table of contents, and says so. The BIN ' +
        'is the same stored bytes as the IMG, so no space is used, and the original is not ' +
        'changed.',
      exists: 'A CUE/BIN of this already exists.',
    };
  }
  if (transformation === 'cue_bin_from_mds') {
    return {
      action: 'Make a CUE/BIN',
      explanation:
        'Makes a CUE/BIN of this Alcohol 120% image, which can be burned, made into a CHD and ' +
        'sent to RomM. It leaves out the MDS descriptor and any subchannel, and says so. The BIN ' +
        'is the same stored bytes as the MDF, so no space is used, unless the MDF holds ' +
        'subchannel after every sector: then it is a new copy without it. The original is not ' +
        'changed.',
      exists: 'A CUE/BIN of this already exists.',
    };
  }
  if (transformation === 'iso_from_mds') {
    return {
      action: 'Make an ISO',
      explanation:
        'Makes an ISO of this Alcohol 120% DVD image, which can be burned, made into a CHD and ' +
        'sent to RomM. It leaves out the MDS descriptor, and says so. The ISO is the same stored ' +
        'bytes as the MDF, so no space is used, and the original is not changed.',
      exists: 'An ISO of this already exists.',
    };
  }
  return {
    action: 'Make a CHD',
    explanation:
      'Makes a new, compressed copy that RomM and its emulators read. The original is not ' +
      'changed. The copy is kept only if it extracts back identical to this one, track by track.',
    exists: 'A CHD of this already exists.',
  };
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
