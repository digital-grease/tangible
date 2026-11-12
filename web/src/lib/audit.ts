// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import type { components } from './api-types';
import type { Tone } from './components/ui/tone';
import { apiCall, apiFetch, type Result } from './session';

export type AuditEvent = components['schemas']['AuditEventView'];
export type AuditEventList = components['schemas']['AuditEventList'];

/** Which events to show; empty fields are not filtered on. */
export type AuditFilter = {
  /** An action, or a kind of action ending in `.`. */
  action?: string;
  actorId?: string;
  targetId?: string;
  outcome?: string;
};

/** Events shown at a time. */
export const AUDIT_PAGE = 50;

/** A page of events, newest first, older than `cursor`. */
export function loadAuditEvents(
  filter: AuditFilter,
  cursor: string | null = null,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<AuditEventList>> {
  const query = new URLSearchParams({ limit: String(AUDIT_PAGE) });
  const set = (name: string, value: string | undefined) => {
    const trimmed = value?.trim();
    if (trimmed) query.set(name, trimmed);
  };
  set('action', filter.action);
  set('actor_id', filter.actorId);
  set('target_id', filter.targetId);
  set('outcome', filter.outcome);
  if (cursor) query.set('cursor', cursor);
  return apiCall(fetcher, `/api/v1/audit-events?${query.toString()}`);
}

/** The kinds of action the page filters by, in words. */
export const ACTION_KINDS: { value: string; label: string }[] = [
  { value: '', label: 'Everything' },
  { value: 'session.', label: 'Signing in and out' },
  { value: 'user.', label: 'Accounts' },
  { value: 'worker.', label: 'Workers' },
  { value: 'worker_enrollment.', label: 'Worker enrollment' },
  { value: 'media_erasure.', label: 'Erasing discs' },
  { value: 'rip.', label: 'Rips' },
  { value: 'derivation.', label: 'Derivatives' },
  { value: 'import.', label: 'Imports from URLs and S3' },
  { value: 'integration.', label: 'Integrations' },
  { value: 'dat.', label: 'DATs' },
  { value: 'artifact.', label: 'Artifacts' },
  { value: 'integrity.', label: 'Integrity checks' },
  { value: 'romm.', label: 'RomM' },
  { value: 'edition.', label: 'Editions' },
];

const ACTIONS: Record<string, string> = {
  'session.started': 'Signed in',
  'session.ended': 'Signed out',
  'session.refused': 'Sign-in refused',
  'user.created': 'Account created',
  'user.role_changed': 'Account role changed',
  'worker_enrollment.issued': 'Enrollment token issued',
  'worker.drained': 'Worker drained',
  'worker.enabled': 'Worker enabled',
  'worker.revoked': 'Worker revoked',
  'media_erasure.requested': 'Erasure asked for',
  'media_erasure.started': 'Erasure started',
  'media_erasure.completed': 'Erasure finished',
  'media_erasure.canceled': 'Erasure withdrawn',
  'media_erasure.abandoned': 'Erasure abandoned by its worker',
  'rip.requested': 'Rip asked for',
  'rip.started': 'Rip started',
  'rip.read': 'Rip read the disc',
  'rip.completed': 'Rip finished',
  'rip.failed': 'Rip failed',
  'rip.refused': 'Rip refused',
  'rip.canceled': 'Rip withdrawn',
  'rip.abandoned': 'Rip abandoned by its worker',
  'rip.copy_queued': 'Rip copy queued',
  'rip.import_failed': 'Rip import failed',
  'derivation.requested': 'Derivative asked for',
  'derivation.completed': 'Derivative made',
  'derivation.canceled': 'Derivative withdrawn',
  'import.url_requested': 'Import from a URL asked for',
  'import.url_refused': 'Import from a URL refused',
  'import.s3_requested': 'Import from S3 asked for',
  'integration.created': 'Integration added',
  'integration.updated': 'Integration changed',
  'integration.secret_changed': 'Integration secret changed',
  'integration.deleted': 'Integration removed',
  'integration.job_submitted': 'Sent to a download client',
  'integration.source_deleted': 'Download deleted once imported',
  'dat.loaded': 'DAT loaded',
  'dat.removed': 'DAT removed',
  'artifact.rehash_requested': 'Hashes asked for again',
  'artifact.recatalogued': 'Artifact catalogued again after a restore',
  'integrity.damage_found': 'Damage found by an integrity check',
  'romm.scan_requested': 'RomM scan asked for',
  'edition.romm_export_set': 'RomM export changed',
};

/** An action in words, or as stored when it is not one known here. */
export function actionLabel(action: string): string {
  return ACTIONS[action] ?? action;
}

/**
 * How an action came out, as a tone beside its words: a refusal is the
 * server doing its job, so a warning rather than a failure.
 */
export function outcomeTone(outcome: string): Tone {
  switch (outcome) {
    case 'success':
      return 'ok';
    case 'failure':
      return 'bad';
    case 'denied':
      return 'warn';
    default:
      return 'idle';
  }
}

/** How an action came out, in words. Never colour alone. */
export function outcomeLabel(outcome: string): string {
  switch (outcome) {
    case 'success':
      return 'Done';
    case 'failure':
      return 'Failed';
    case 'denied':
      return 'Refused';
    default:
      return outcome;
  }
}

/** Who did it, in words: a person by name, anything else by kind. */
export function actorLabel(event: Pick<AuditEvent, 'actor_type' | 'actor_id'>): string {
  switch (event.actor_type) {
    case 'user':
      return event.actor_id;
    case 'worker':
      return `Worker ${event.actor_id}`;
    case 'integration':
      return `Integration ${event.actor_id}`;
    case 'system':
      return `Tangible (${event.actor_id})`;
    default:
      return `${event.actor_type} ${event.actor_id}`;
  }
}

/** What else was kept, as text, or nothing when nothing was. */
export function metadataText(metadata: unknown): string {
  if (
    metadata == null ||
    (typeof metadata === 'object' && Object.keys(metadata as object).length === 0)
  ) {
    return '';
  }
  return JSON.stringify(metadata);
}
