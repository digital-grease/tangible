// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

/**
 * The drive bay (docs/plans/ui-overhaul.md, "The shell"): each drive's
 * state, in words, a tone and a drawing, from what the server says it is
 * doing (`DriveView.activity`, API.md 8.6).
 *
 * Pure: DriveBay.svelte shows what this returns, and the tests check it.
 */
import { jobStateLabel } from './burns';
import type { DiscShape } from './disc';
import { ripStateLabel } from './rips';
import { erasureStateLabel, workerStatusLabel, type DriveView } from './workers';
import type { Tone } from './components/ui/tone';

/** How the drawing beside a drive is drawn. */
export interface BayDisc {
  mode: 'image' | 'burn' | 'rip' | 'empty';
  shape: DiscShape | null;
  family: string | null;
  capacity: number | null;
  progress: number | null;
}

export interface BayEntry {
  id: string;
  name: string;
  /**
   * The drive's worker, and for an administrator its device:
   * "worker-name · /dev/disc-block". Host device paths are not shown to
   * anybody else (UX.md 11).
   */
  where: string;
  tone: Tone;
  /** The state, in words. */
  state: string;
  /** What it is working on, in words, or nothing. */
  title: string | null;
  /** Where to read more about it. */
  href: string | null;
  /** Whether a progress bar belongs here at all. */
  showsProgress: boolean;
  progress: number | null;
  /** Whether a drive is at work on a disc now (the laser colour). */
  atWork: boolean;
  disc: BayDisc;
}

/** Burn states in which the drive is writing or reading the disc. */
const BURN_AT_WORK = new Set(['writing', 'finalizing', 'verifying']);
/** Rip states in which there is progress to show. */
const RIP_AT_WORK = new Set(['reading', 'uploading']);

/** A medium profile's family, such as `cd` for `CD-ROM`. */
export function profileFamily(profile: string | null | undefined): string | null {
  const name = profile?.toUpperCase() ?? '';
  if (name.startsWith('CD')) return 'cd';
  if (name.startsWith('DVD')) return 'dvd';
  if (name.startsWith('BD')) return 'bluray';
  return null;
}

/**
 * One drive as the bay shows it. `device` adds the host's device path to
 * where it is, for an administrator.
 */
export function bayEntry(
  drive: DriveView,
  { device = false }: { device?: boolean } = {},
): BayEntry {
  const base = {
    id: drive.id,
    name: drive.name,
    where: device ? `${drive.worker_name} · ${drive.device}` : drive.worker_name,
  };
  const activity = drive.activity;
  const empty: BayDisc = {
    mode: 'empty',
    shape: null,
    family: null,
    capacity: null,
    progress: null,
  };

  if (!activity) {
    const online = drive.worker_status === 'online';
    return {
      ...base,
      tone: 'idle',
      state: online ? 'Idle' : workerStatusLabel(drive.worker_status),
      title: null,
      href: null,
      showsProgress: false,
      progress: null,
      atWork: false,
      disc: empty,
    };
  }

  const progress = activity.progress ?? null;
  const capacity = activity.medium?.capacity_blocks ?? null;

  if (activity.kind === 'burn') {
    const atWork = BURN_AT_WORK.has(activity.state);
    const waiting = activity.state === 'waiting_for_media';
    return {
      ...base,
      tone: waiting ? 'warn' : 'busy',
      state: jobStateLabel(activity.state),
      title: activity.title
        ? `${activity.title}${activity.disc_number ? `, disc ${activity.disc_number}` : ''}`
        : 'A burn',
      href: `/burns/${activity.id}`,
      showsProgress: atWork,
      progress,
      atWork,
      disc: waiting
        ? empty
        : {
            mode: 'burn',
            shape: activity.disc_shape ?? null,
            family: activity.disc_shape?.media_family ?? null,
            capacity,
            progress: atWork ? progress : 0,
          },
    };
  }

  if (activity.kind === 'rip') {
    const atWork = RIP_AT_WORK.has(activity.state);
    return {
      ...base,
      tone: 'busy',
      state: ripStateLabel(activity.state),
      title: 'Ripping the disc in it',
      href: `/rips/${activity.id}`,
      showsProgress: atWork,
      progress,
      atWork,
      disc: {
        mode: 'rip',
        shape: null,
        family: profileFamily(activity.medium?.profile),
        capacity,
        progress: atWork ? progress : 0,
      },
    };
  }

  // An erasure: no progress is reported, so none is drawn.
  return {
    ...base,
    tone: 'busy',
    state: erasureStateLabel(activity.state),
    title: 'Erasing the disc in it',
    href: null,
    showsProgress: false,
    progress: null,
    atWork: activity.state === 'erasing',
    disc: { mode: 'burn', shape: null, family: null, capacity: null, progress: null },
  };
}

/** Whether any drive has work open on it. */
export function anyBusy(drives: readonly DriveView[]): boolean {
  return drives.some((drive) => drive.activity);
}

/** The bay in one line, for when it is folded. */
export function baySummary(drives: readonly DriveView[]): string {
  if (drives.length === 0) return 'No drives yet';
  const busy = drives.filter((drive) => drive.activity).length;
  const noun = drives.length === 1 ? 'drive' : 'drives';
  if (busy === 0)
    return drives.length === 1 ? '1 drive, idle' : `${drives.length} drives, all idle`;
  return `${busy} of ${drives.length} ${noun} at work`;
}

/**
 * What changed since the bay last looked, for its live region: one line per
 * drive whose state changed. Nothing on the first look, nor for progress
 * alone, so a screen reader is told about changes, not every percent.
 */
export function announcement(
  previous: ReadonlyMap<string, string> | null,
  entries: readonly BayEntry[],
): string {
  if (!previous) return '';
  return entries
    .filter((entry) => previous.get(entry.id) !== entry.state)
    .map((entry) => `${entry.name}: ${entry.state}`)
    .join('. ');
}

/** Each drive's state, to compare the next look with. */
export function statesOf(entries: readonly BayEntry[]): Map<string, string> {
  return new Map(entries.map((entry) => [entry.id, entry.state]));
}
