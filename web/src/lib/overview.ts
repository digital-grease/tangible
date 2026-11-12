// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

/**
 * The Overview page (docs/plans/ui-overhaul.md, "Pages"; UX.md 3): the
 * dashboard's tiles, tones for its burns, and the live events panel's lines
 * in words. Pure; the page shows what this returns.
 */
import type { Tone } from './components/ui/tone';
import type { IconName } from './components/ui/icons';
import { attemptOutcomeLabel, jobStateLabel } from './burns';
import {
  burnsSummary,
  importsSummary,
  integritySummary,
  storageSummary,
  workersSummary,
  type Dashboard,
} from './dashboard';
import type { StreamEvent } from './events';
import { importStateLabel } from './imports';
import { formatBytes } from './library';
import { ripStateLabel } from './rips';
import { erasureStateLabel, workerStatusLabel } from './workers';

export interface Tile {
  key: string;
  label: string;
  href: string;
  icon: IconName;
  /** The figure, large. */
  figure: string;
  /** Words after the figure, small. */
  unit?: string;
  /** The tile's sentence, from the dashboard's own summaries. */
  summary: string;
  /** A mark when something here asks for somebody. */
  mark?: { tone: Tone; label: string };
  /** For storage, how full the volume is, from 0 to 1. */
  fill?: number;
}

/** The five tiles, each linking to where its numbers come from. */
export function tiles(board: Dashboard): Tile[] {
  const { burns, imports, workers, integrity, storage } = board;
  const workersKnown =
    workers.online + workers.silent + workers.draining + workers.pending + workers.incompatible;
  const used =
    storage.total_bytes && storage.free_bytes != null && storage.total_bytes > 0
      ? (storage.total_bytes - storage.free_bytes) / storage.total_bytes
      : undefined;
  return [
    {
      key: 'burns',
      label: 'Burns',
      href: '/burns',
      icon: 'burn',
      figure: String(burns.queued + burns.waiting_for_media + burns.active),
      unit: 'open',
      summary: burnsSummary(burns),
      mark:
        burns.needing_attention > 0
          ? { tone: 'bad', label: `${burns.needing_attention} need attention` }
          : burns.waiting_for_media > 0
            ? { tone: 'warn', label: `${burns.waiting_for_media} waiting for a disc` }
            : undefined,
    },
    {
      key: 'imports',
      label: 'Imports',
      href: '/imports',
      icon: 'import',
      figure: String(imports.active),
      unit: 'under way',
      summary: importsSummary(imports),
      mark: imports.failed > 0 ? { tone: 'bad', label: `${imports.failed} failed` } : undefined,
    },
    {
      key: 'workers',
      label: 'Workers',
      href: '/workers',
      icon: 'worker',
      figure: String(workers.online),
      unit: `of ${workersKnown} online`,
      summary: workersSummary(workers),
      mark:
        workers.silent + workers.incompatible > 0
          ? { tone: 'warn', label: `${workers.silent + workers.incompatible} need a look` }
          : undefined,
    },
    {
      key: 'integrity',
      label: 'Integrity',
      href: '/storage',
      icon: 'dat',
      figure: String(integrity.damaged),
      unit: 'damaged',
      summary: integritySummary(integrity),
      mark:
        integrity.damaged > 0
          ? { tone: 'bad', label: 'Damage found' }
          : integrity.unchecked > 0
            ? { tone: 'idle', label: 'Not all checked' }
            : { tone: 'ok', label: 'Checked and intact' },
    },
    {
      key: 'storage',
      label: 'Storage',
      href: '/storage',
      icon: 'storage',
      figure: formatBytes(storage.stored_bytes),
      unit: 'stored',
      summary: storageSummary(storage),
      fill: used,
    },
  ];
}

/** A burn job's state as a tone. */
export function jobTone(state: string): Tone {
  switch (state) {
    case 'complete':
      return 'ok';
    case 'failed':
    case 'needs_attention':
      return 'bad';
    case 'waiting_for_media':
      return 'warn';
    case 'queued':
    case 'canceled':
      return 'idle';
    default:
      return 'busy';
  }
}

/** How a burn attempt ended, as a tone. */
export function outcomeTone(state: string): Tone {
  switch (state) {
    case 'verified':
      return 'ok';
    case 'written':
      return 'warn';
    case 'verification_failed':
    case 'write_failed':
    case 'failed_before_write':
    case 'interrupted':
      return 'bad';
    case 'canceled':
      return 'idle';
    default:
      return 'busy';
  }
}

/** One line of the events panel. */
export interface EventLine {
  /** What it is about: "Burn", "Import", "Rip", and so on. */
  topic: string;
  /** What happened, in words. */
  words: string;
  tone: Tone;
  href: string | null;
}

const text = (value: unknown): string => (typeof value === 'string' ? value : '');

/**
 * An event in words, or `null` for one the panel leaves out: progress,
 * which the drive bay shows, and an artifact's own changes, which are many
 * and say nothing without reading the artifact. `names` gives a burn job its
 * title where the page already knows it; events carry no names
 * (docs/plans/event-stream.md).
 */
export function eventLine(
  event: StreamEvent,
  names: ReadonlyMap<string, string> = new Map(),
): EventLine | null {
  const id = text(event.data.id);
  const state = text(event.data.state);
  switch (event.type) {
    case 'burn_job.changed':
      return {
        topic: 'Burn',
        words: `${names.get(id) ?? 'A burn'}: ${jobStateLabel(state)}`,
        tone: jobTone(state),
        href: id ? `/burns/${id}` : null,
      };
    case 'burn_attempt.changed': {
      const job = text(event.data.burn_job_id);
      return {
        topic: 'Burn',
        words: `${names.get(job) ?? 'A burn attempt'}: ${attemptOutcomeLabel(state)}`,
        tone: outcomeTone(state),
        href: job ? `/burns/${job}` : null,
      };
    }
    case 'import.changed':
      return {
        topic: 'Import',
        words: `An import: ${importStateLabel(state)}`,
        tone: state === 'complete' ? 'ok' : state.startsWith('failed') ? 'bad' : 'busy',
        href: '/imports',
      };
    case 'rip.changed':
      return {
        topic: 'Rip',
        words: `A rip: ${ripStateLabel(state)}`,
        tone:
          state === 'complete'
            ? 'ok'
            : state === 'failed' || state === 'refused'
              ? 'bad'
              : state === 'canceled'
                ? 'idle'
                : 'busy',
        href: id ? `/rips/${id}` : null,
      };
    case 'erasure.changed':
      return {
        topic: 'Erasure',
        words: `An erasure: ${erasureStateLabel(state)}`,
        tone:
          state === 'erased' || state === 'already_blank'
            ? 'ok'
            : state === 'failed' || state === 'refused'
              ? 'bad'
              : 'busy',
        href: null,
      };
    case 'worker.changed': {
      const status = text(event.data.status);
      return {
        topic: 'Worker',
        words: `A worker: ${workerStatusLabel(status)}`,
        tone: status === 'online' ? 'ok' : status === 'pending' ? 'idle' : 'warn',
        href: '/workers',
      };
    }
    case 'derivation.changed':
      return {
        topic: 'Derivative',
        words: `A derivative: ${state.replaceAll('_', ' ')}`,
        tone: state === 'complete' ? 'ok' : state === 'failed' ? 'bad' : 'busy',
        href: null,
      };
    case 'stream.reset':
      return {
        topic: 'Stream',
        words: 'Caught up after a gap in the event stream',
        tone: 'idle',
        href: null,
      };
    default:
      return null;
  }
}

/** A burn job's name, by its identifier, from what the dashboard lists. */
export function burnNames(board: Dashboard | null): Map<string, string> {
  const names = new Map<string, string>();
  for (const burn of [...(board?.recent_burns ?? []), ...(board?.attention ?? [])]) {
    names.set(burn.burn_job_id, `${burn.title}, disc ${burn.disc_number}`);
  }
  return names;
}

/** A time, as near as it needs to be: "14:31", "Yesterday 21:14", "8 Nov 12:03". */
export function when(value: string | Date, now: Date = new Date()): string {
  const at = value instanceof Date ? value : new Date(value);
  if (Number.isNaN(at.getTime())) return '';
  const time = at.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
  const day = (date: Date) =>
    new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime();
  const days = Math.round((day(now) - day(at)) / 86_400_000);
  if (days === 0) return time;
  if (days === 1) return `Yesterday ${time}`;
  const date = at.toLocaleDateString(undefined, {
    day: 'numeric',
    month: 'short',
    ...(at.getFullYear() === now.getFullYear() ? {} : { year: 'numeric' }),
  });
  return `${date} ${time}`;
}
