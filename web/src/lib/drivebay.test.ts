// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
import { describe, expect, it } from 'vitest';
import { announcement, anyBusy, bayEntry, baySummary, profileFamily, statesOf } from './drivebay';
import type { DriveView } from './workers';

const medium = {
  profile: 'CD-R',
  blank: true,
  writable: true,
  rewritable: false,
  erasable: false,
  capacity_blocks: 359_844,
  free_blocks: 359_844,
  block_size: 2048,
  sessions: 0,
  manufacturer: null,
  warnings: [],
};

function drive(activity: DriveView['activity'] = null, worker_status = 'online'): DriveView {
  return {
    id: 'd1',
    name: 'Slimtype DS8A8SH',
    device: '/dev/disc-block',
    vendor: null,
    model: null,
    firmware: null,
    connection: 'usb',
    status: 'ready',
    last_seen_at: null,
    worker_id: 'w1',
    worker_name: 'bench-1',
    worker_status,
    worker_last_seen_at: null,
    can_rip: true,
    can_decrypt_rips: false,
    can_rip_with_redumper: false,
    activity,
  };
}

const burning = (state: string, progress: number | null = 0.43): DriveView['activity'] => ({
  kind: 'burn',
  id: 'j1',
  state,
  title: 'Debian 13.1 netinst',
  disc_number: 1,
  progress,
  medium,
  disc_shape: { media_family: 'cd', sector_count: 300_000, tracks: [] },
  started_at: '2026-11-11T12:00:00Z',
});

describe('the drive bay', () => {
  it('says an idle drive is idle, or what its worker is doing instead', () => {
    const idle = bayEntry(drive());
    expect(idle).toMatchObject({ tone: 'idle', state: 'Idle', title: null, showsProgress: false });
    expect(idle.where).toBe('bench-1');
    expect(bayEntry(drive(), { device: true }).where).toBe('bench-1 · /dev/disc-block');
    expect(idle.disc.mode).toBe('empty');
    expect(bayEntry(drive(null, 'offline')).state).toBe('Not heard from lately');
  });

  it('shows a burn writing with its progress, its title and the written area', () => {
    const entry = bayEntry(drive(burning('writing')));
    expect(entry).toMatchObject({
      tone: 'busy',
      state: 'Writing',
      title: 'Debian 13.1 netinst, disc 1',
      href: '/burns/j1',
      showsProgress: true,
      progress: 0.43,
      atWork: true,
    });
    expect(entry.disc).toMatchObject({ mode: 'burn', capacity: 359_844, progress: 0.43 });
    expect(entry.disc.shape?.sector_count).toBe(300_000);
  });

  it('asks for a disc, as a warning, with an empty tray drawn', () => {
    const entry = bayEntry(drive(burning('waiting_for_media', null)));
    expect(entry).toMatchObject({
      tone: 'warn',
      state: 'Waiting for a disc',
      showsProgress: false,
    });
    expect(entry.disc.mode).toBe('empty');
  });

  it('draws nothing written before the drive is writing', () => {
    const entry = bayEntry(drive(burning('staging', null)));
    expect(entry).toMatchObject({ state: 'Copying to the worker', showsProgress: false });
    expect(entry.disc.progress).toBe(0);
  });

  it('shows a rip reading, with the disc’s family from its profile', () => {
    const entry = bayEntry(
      drive({
        kind: 'rip',
        id: 'r1',
        state: 'reading',
        title: null,
        disc_number: null,
        progress: 0.5,
        medium: { ...medium, profile: 'DVD-ROM' },
        disc_shape: null,
        started_at: '2026-11-11T12:00:00Z',
      }),
    );
    expect(entry).toMatchObject({
      state: 'Reading the disc',
      href: '/rips/r1',
      showsProgress: true,
    });
    expect(entry.disc).toMatchObject({ mode: 'rip', family: 'dvd', progress: 0.5 });
  });

  it('shows an erasure with no progress, as none is reported', () => {
    const entry = bayEntry(
      drive({
        kind: 'erasure',
        id: 'e1',
        state: 'erasing',
        title: null,
        disc_number: null,
        progress: null,
        medium: null,
        disc_shape: null,
        started_at: '2026-11-11T12:00:00Z',
      }),
    );
    expect(entry).toMatchObject({
      state: 'Erasing',
      showsProgress: false,
      atWork: true,
      href: null,
    });
  });

  it('knows a family from a profile, and says nothing for one it does not know', () => {
    expect(profileFamily('CD-R')).toBe('cd');
    expect(profileFamily('DVD+RW')).toBe('dvd');
    expect(profileFamily('BD-R sequential recording')).toBe('bluray');
    expect(profileFamily('HD DVD-R')).toBeNull();
    expect(profileFamily(null)).toBeNull();
  });

  it('sums the bay up in one line', () => {
    expect(baySummary([])).toBe('No drives yet');
    expect(baySummary([drive()])).toBe('1 drive, idle');
    expect(baySummary([drive(), { ...drive(), id: 'd2' }])).toBe('2 drives, all idle');
    expect(baySummary([drive(burning('writing')), { ...drive(), id: 'd2' }])).toBe(
      '1 of 2 drives at work',
    );
    expect(anyBusy([drive()])).toBe(false);
    expect(anyBusy([drive(burning('writing'))])).toBe(true);
  });

  it('announces a change of state, not the first look, nor progress alone', () => {
    const first = [bayEntry(drive(burning('writing', 0.1)))];
    expect(announcement(null, first)).toBe('');
    const seen = statesOf(first);
    expect(announcement(seen, [bayEntry(drive(burning('writing', 0.6)))])).toBe('');
    expect(announcement(seen, [bayEntry(drive(burning('verifying', 0.1)))])).toBe(
      'Slimtype DS8A8SH: Reading back',
    );
  });
});
