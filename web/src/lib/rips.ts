// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// Ripping the disc in a drive into the library (ADR 0013): a CD into a table
// of contents and its data, a DVD into an image. The server and
// the drive's worker do the work; this asks for it, shows how it is going,
// and lets a person withdraw it while reading.

import type { components } from './api-types';
import { apiCall, apiFetch, type Result } from './session';

export type RipView = components['schemas']['RipView'];
export type RipPage = components['schemas']['RipPage'];
export type RipEventView = components['schemas']['RipEventView'];
export type RipRequest = components['schemas']['RequestRip'];
export type DecryptionView = components['schemas']['DecryptionView'];
export type RippedView = components['schemas']['RippedView'];

/** The original a decrypting rip keeps (ADR 0013). */
export type KeepChoice = 'encrypted_original' | 'decrypted_original';

/** The tool a CD is read with (ADR 0015). */
export type CdEngineChoice = 'redumper' | 'cdrdao';

/**
 * Ask for the disc in a drive to be ripped. Without options it rips without
 * decryption; the server refuses `decrypt` where it is not allowed.
 */
export function requestRip(
  driveId: string,
  options: RipRequest = {},
  fetcher: typeof fetch = apiFetch,
): Promise<Result<RipView>> {
  return apiCall(fetcher, `/api/v1/drives/${encodeURIComponent(driveId)}/rips`, 'POST', options);
}

/**
 * What a rip asks for: decryption only when asked, with the original chosen;
 * cdrdao only when chosen. redumper is the worker's own default where it has
 * it, so choosing it asks for nothing, and a worker rebuilt without it still
 * rips the CD rather than refusing.
 */
export function ripOptions(
  decrypt: boolean,
  keep: KeepChoice,
  cdEngine: CdEngineChoice = 'redumper',
): RipRequest {
  return {
    ...(decrypt ? { decrypt: true, keep } : {}),
    ...(cdEngine === 'cdrdao' ? { cd_engine: 'cdrdao' } : {}),
  };
}

/** The tool a rip asked to read a CD with, in words. */
export function cdEngineLabel(cdEngine: string | null | undefined): string {
  switch (cdEngine) {
    case 'redumper':
      return 'redumper, read offset corrected';
    case 'cdrdao':
      return 'cdrdao, no offset correction';
    default:
      return "The worker's best";
  }
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
      return 'Only CDs and DVDs can be ripped so far. Blu-rays come later.';
    case 'RELOAD_DISC':
      return 'The system has not caught up with the disc in the drive (this happens straight after a burn). Take the disc out, put it back, and ask again.';
    case 'ENCRYPTED':
      return 'The disc is encrypted, and this rip did not decrypt it. Nothing that was read was kept. Where a drive offers "Decrypt an encrypted disc", ask again with it ticked.';
    case 'CD_ENGINE_UNAVAILABLE':
      return "This drive's worker no longer has redumper. Ask again; choose cdrdao if the drive offers the choice.";
    case 'DECRYPTION_FAILED':
      return 'The disc is encrypted, and decrypting it failed. Nothing that was read was kept. Clean the disc and ask again; the technical detail says where it stopped.';
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

/** What a worker found on a disc, in words. */
export function encryptionLabel(encryption: string | null | undefined): string {
  switch (encryption) {
    case null:
    case undefined:
      return 'Not read yet';
    case 'none':
      return 'Not encrypted';
    case 'css':
      return 'Encrypted (CSS, a DVD)';
    case 'aacs':
      return 'Encrypted (AACS, a Blu-ray)';
    case 'unknown':
      return 'Encrypted, with a system not recognised';
    default:
      return encryption;
  }
}

/** The original a decrypting rip asked to keep, in words. */
export function keepLabel(keep: string | null | undefined): string {
  switch (keep) {
    case 'encrypted_original':
      return 'The encrypted image, and a decrypted copy';
    case 'decrypted_original':
      return 'Only a decrypted image (not a faithful copy of the disc)';
    default:
      return '—';
  }
}

/** Which library and tool decrypted a disc. Versions only; never a key. */
export function decryptionSummary(decryption: DecryptionView): string {
  return `${decryption.library} ${decryption.library_version}, through ${decryption.tool} ${decryption.tool_version}`;
}

/** The drive a disc was ripped in, as it described itself. */
export function rippedDrive(ripped: RippedView): string {
  const name = [ripped.drive_vendor, ripped.drive_model].filter(Boolean).join(' ');
  const parts = [name || 'A drive that did not say what it is'];
  if (ripped.drive_firmware) parts.push(`firmware ${ripped.drive_firmware}`);
  if (ripped.drive_connection && ripped.drive_connection !== 'unknown') {
    parts.push(ripped.drive_connection.toUpperCase());
  }
  return parts.join(', ');
}
