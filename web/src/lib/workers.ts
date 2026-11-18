// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import type { components } from './api-types';
import type { Tone } from './components/ui/tone';
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

/** Where a new worker's drive is: in the server's machine, or another. */
export type WorkerPlace = 'here' | 'elsewhere';

/** A worker's name as `.env` can carry it: no spaces, quotes or `#`. */
export function workerEnvName(name: string): string {
  const safe = name
    .trim()
    .replace(/\s+/g, '-')
    .replace(/[^A-Za-z0-9._-]/g, '')
    .slice(0, 64);
  return safe || 'drive-1';
}

/**
 * The lines a new worker's `.env` needs, its enrollment token among them
 * (deploy/README.md, "Adding a real drive"). In the server's machine they go
 * in the `.env` beside `compose.yaml`, and `COMPOSE_FILE` makes plain `docker
 * compose` include `compose.hardware.yaml`; in another, in the `.env` beside
 * `compose.worker.yaml`, which reaches the server at `serverUrl`. The drive
 * and its group are left to fill in, each with the command that finds it.
 */
export function workerEnvLines(
  place: WorkerPlace,
  token: string,
  name: string,
  serverUrl: string,
): string {
  const lines: string[] = [];
  if (place === 'here') {
    lines.push('COMPOSE_FILE=compose.yaml:compose.hardware.yaml');
  } else {
    lines.push(`TANGIBLE_SERVER_URL=${serverUrl}`);
    if (serverUrl.startsWith('http://')) {
      lines.push(
        '# Plain http: the worker refuses it across the network unless this is true, and it should be only on a network you trust.',
        '# TANGIBLE_WORKER_PLAIN_HTTP=true',
      );
    }
  }
  lines.push(
    `TANGIBLE_WORKER_NAME=${workerEnvName(name)}`,
    `TANGIBLE_ENROLLMENT_TOKEN=${token}`,
    "# The drive, from: ls -l /dev/disk/by-id/ | grep -E 'sr[0-9]+$'",
    'TANGIBLE_BLOCK_DEVICE=/dev/disk/by-id/NAME',
    '# The group that owns it, from: stat -L -c %g /dev/disk/by-id/NAME',
    'TANGIBLE_OPTICAL_GID=',
  );
  return `${lines.join('\n')}\n`;
}

export type WorkerView = components['schemas']['WorkerView'];

/** How many workers the page asks for at once: more than a deployment has. */
export const WORKER_PAGE = 200;

/** The workers, oldest first. */
export function loadWorkers(
  fetcher: typeof fetch = apiFetch,
): Promise<Result<components['schemas']['WorkerList']>> {
  return apiCall(fetcher, `/api/v1/workers?limit=${WORKER_PAGE}`);
}

/** What an administrator can do to a worker. */
export type WorkerAction = 'drain' | 'enable' | 'revoke' | 'rotate-credential';

/** Drain, enable, revoke or ask a worker to rotate its credential, answering it as it now is. */
export function changeWorker(
  workerId: string,
  action: WorkerAction,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<WorkerView>> {
  return apiCall(fetcher, `/api/v1/workers/${encodeURIComponent(workerId)}/${action}`, 'POST');
}

/** A worker's status, in words. Never colour alone. */
export function workerStatusLabel(status: string): string {
  switch (status) {
    case 'pending':
      return 'Enrolled, not heard from yet';
    case 'online':
      return 'Taking work';
    case 'offline':
      return 'Not heard from lately';
    case 'draining':
      return 'Draining: finishing what it holds, taking nothing new';
    case 'revoked':
      return 'Revoked: its credential is refused';
    case 'incompatible':
      return 'Incompatible with this server';
    default:
      return status;
  }
}

/**
 * A worker's status as a tone, beside its words: taking work is fine; not
 * heard from, or draining and so taking nothing new, wants a look; one this
 * server cannot work with is a problem. A revoked worker is retired, not
 * failing, and a pending one has only not started yet.
 */
export function workerStatusTone(status: string): Tone {
  switch (status) {
    case 'online':
      return 'ok';
    case 'offline':
    case 'draining':
      return 'warn';
    case 'incompatible':
      return 'bad';
    default:
      return 'idle';
  }
}

/**
 * What can be done to a worker in this status, as the server allows.
 *
 * Rotation is not offered while one is already asked for, nor to an
 * incompatible worker, which cannot take part in it.
 */
export function workerActions(status: string, rotationAsked = false): WorkerAction[] {
  const rotate: WorkerAction[] = rotationAsked ? [] : ['rotate-credential'];
  switch (status) {
    case 'pending':
    case 'online':
    case 'offline':
      return ['drain', ...rotate, 'revoke'];
    case 'draining':
      return ['enable', ...rotate, 'revoke'];
    case 'incompatible':
      return ['revoke'];
    default:
      return [];
  }
}

/** How old a worker's credential is, and whether it is to be rotated, in words. */
export function credentialLabel(
  worker: Pick<
    WorkerView,
    | 'status'
    | 'credential_issued_at'
    | 'credential_rotation_requested_at'
    | 'credential_rotation_due'
  >,
  now: Date = new Date(),
): string {
  if (worker.status === 'revoked') return 'Refused';
  if (worker.credential_rotation_requested_at) {
    return 'Rotation asked for: the worker rotates it between jobs';
  }
  if (worker.credential_rotation_due) {
    return 'Due for rotation: the worker rotates it between jobs';
  }
  const days = Math.floor(
    (now.getTime() - new Date(worker.credential_issued_at).getTime()) / 86_400_000,
  );
  if (!Number.isFinite(days) || days < 1) return 'Issued today';
  return days === 1 ? 'Issued yesterday' : `Issued ${days} days ago`;
}

/**
 * A credential's tone, where it has one worth showing: a rotation due wants
 * a look, one asked for is under way, and a revoked worker's is refused.
 * `null` for an ordinary credential, whose age is said in plain words.
 */
export function credentialTone(
  worker: Pick<
    WorkerView,
    'status' | 'credential_rotation_requested_at' | 'credential_rotation_due'
  >,
): Tone | null {
  if (worker.status === 'revoked') return 'idle';
  if (worker.credential_rotation_requested_at) return 'busy';
  if (worker.credential_rotation_due) return 'warn';
  return null;
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

/**
 * An erasure's state as a tone, beside its words. A refusal is a warning,
 * not a failure: the worker looked and found a disc that cannot be erased,
 * such as a write-once one, and left it alone.
 */
export function erasureStateTone(state: string): Tone {
  switch (state) {
    case 'erased':
    case 'already_blank':
      return 'ok';
    case 'refused':
      return 'warn';
    case 'failed':
      return 'bad';
    case 'canceled':
      return 'idle';
    default:
      return 'busy';
  }
}

/** Whether any erasure in a list is still going, so the page keeps checking. */
export function anyOpen(erasures: ErasureView[]): boolean {
  return erasures.some((erasure) => erasure.is_open);
}

/**
 * A drive's vendor and model, "Slimtype DVD A DS8A8SH", or null when it gave
 * neither. This names the hardware; its device path, which is the one inside
 * its worker's container, does not (it is /dev/sr0 whichever drive it is).
 */
export function driveHardware(drive: Pick<DriveView, 'vendor' | 'model'>): string | null {
  return [drive.vendor, drive.model].filter(Boolean).join(' ') || null;
}

/** What a drive says it is, in one line: "Slimtype DVD A DS8A8SH, firmware KS21". */
export function driveModel(drive: Pick<DriveView, 'vendor' | 'model' | 'firmware'>): string {
  const name = driveHardware(drive);
  if (!name) return drive.firmware ? `Firmware ${drive.firmware}` : 'Not reported';
  return drive.firmware ? `${name}, firmware ${drive.firmware}` : name;
}

/** How a drive is attached, in words. */
export function connectionLabel(connection: string): string {
  switch (connection) {
    case 'usb':
      return 'USB';
    case 'sata':
      return 'SATA';
    default:
      return 'Not known';
  }
}
