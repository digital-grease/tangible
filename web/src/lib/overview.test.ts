// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
import { describe, expect, it } from 'vitest';
import type { Dashboard } from './dashboard';
import { burnNames, eventLine, jobTone, outcomeTone, tiles, when } from './overview';

const board = (overrides: Partial<Dashboard> = {}): Dashboard => ({
  imports: { active: 2, failed: 0 },
  burns: { queued: 3, waiting_for_media: 1, active: 1, needing_attention: 0 },
  workers: { online: 2, silent: 0, draining: 0, pending: 0, incompatible: 0 },
  integrity: { damaged: 0, unchecked: 0 },
  storage: { objects: 412, stored_bytes: 2_000_000_000, free_bytes: 6e11, total_bytes: 1e12 },
  recent_burns: [],
  attention: [],
  ...overrides,
});

describe('the overview', () => {
  it('shows five tiles, each linking to where its figures come from', () => {
    const shown = tiles(board());
    expect(shown.map((tile) => [tile.label, tile.href])).toEqual([
      ['Burns', '/burns'],
      ['Imports', '/imports'],
      ['Workers', '/workers'],
      ['Integrity', '/storage'],
      ['Storage', '/storage'],
    ]);
    const burns = shown[0]!;
    expect(burns.figure).toBe('5');
    expect(burns.summary).toBe('1 waiting for a disc, 1 under way, 3 queued');
    expect(burns.mark).toEqual({ tone: 'warn', label: '1 waiting for a disc' });
    expect(shown[2]).toMatchObject({ figure: '2', unit: 'of 2 online', mark: undefined });
    expect(shown[3]?.mark).toEqual({ tone: 'ok', label: 'Checked and intact' });
    expect(shown[4]?.fill).toBeCloseTo(0.4);
  });

  it('marks what asks for somebody, worst first', () => {
    const shown = tiles(
      board({
        burns: { queued: 0, waiting_for_media: 1, active: 0, needing_attention: 2 },
        imports: { active: 0, failed: 1 },
        workers: { online: 1, silent: 1, draining: 0, pending: 0, incompatible: 0 },
        integrity: { damaged: 3, unchecked: 5 },
        storage: { objects: 0, stored_bytes: 0, free_bytes: null, total_bytes: null },
      }),
    );
    expect(shown.map((tile) => tile.mark?.tone)).toEqual(['bad', 'bad', 'warn', 'bad', undefined]);
    expect(shown[4]?.fill).toBeUndefined();
  });

  it('gives burn states and outcomes a tone', () => {
    expect(jobTone('needs_attention')).toBe('bad');
    expect(jobTone('waiting_for_media')).toBe('warn');
    expect(jobTone('writing')).toBe('busy');
    expect(jobTone('complete')).toBe('ok');
    expect(outcomeTone('verified')).toBe('ok');
    expect(outcomeTone('written')).toBe('warn');
    expect(outcomeTone('verification_failed')).toBe('bad');
    expect(outcomeTone('canceled')).toBe('idle');
  });

  it('puts an event into words, naming a burn where the page knows it', () => {
    const names = new Map([['j1', 'Debian 13.1 netinst, disc 1']]);
    expect(
      eventLine(
        { type: 'burn_job.changed', data: { id: 'j1', state: 'waiting_for_media' } },
        names,
      ),
    ).toEqual({
      topic: 'Burn',
      words: 'Debian 13.1 netinst, disc 1: Waiting for a disc',
      tone: 'warn',
      href: '/burns/j1',
    });
    expect(
      eventLine({
        type: 'burn_attempt.changed',
        data: { id: 'a1', burn_job_id: 'j2', state: 'verifying' },
      }),
    ).toMatchObject({ words: 'A burn attempt: Reading back', href: '/burns/j2' });
    expect(eventLine({ type: 'rip.changed', data: { id: 'r1', state: 'refused' } })).toMatchObject({
      topic: 'Rip',
      tone: 'bad',
      href: '/rips/r1',
    });
    expect(
      eventLine({ type: 'worker.changed', data: { id: 'w1', status: 'offline' } }),
    ).toMatchObject({
      words: 'A worker: Not heard from lately',
      tone: 'warn',
    });
    expect(eventLine({ type: 'stream.reset', data: {} })?.topic).toBe('Stream');
  });

  it('leaves out progress and an artifact’s own changes, which are many', () => {
    expect(
      eventLine({ type: 'burn.progress', data: { burn_job_id: 'j1', progress: 0.5 } }),
    ).toBeNull();
    expect(eventLine({ type: 'rip.progress', data: { id: 'r1', progress: 0.5 } })).toBeNull();
    expect(eventLine({ type: 'artifact.changed', data: { id: 'a1' } })).toBeNull();
  });

  it('names burns from what the dashboard lists', () => {
    const burn = {
      burn_job_id: 'j1',
      disc_id: 'd1',
      title: 'Knoppix 9.1',
      disc_number: 1,
      state: 'verified',
      error_code: null,
      at: '2026-11-11T12:00:00Z',
    };
    expect(burnNames(board({ recent_burns: [burn] })).get('j1')).toBe('Knoppix 9.1, disc 1');
    expect(burnNames(null).size).toBe(0);
  });

  it('says a time as near as it needs to be', () => {
    const now = new Date(2026, 10, 11, 15, 0);
    expect(when(new Date(2026, 10, 11, 14, 31), now)).not.toMatch(/Yesterday|Nov/);
    expect(when(new Date(2026, 10, 10, 21, 14), now)).toMatch(/^Yesterday /);
    expect(when(new Date(2026, 10, 8, 12, 3), now)).toMatch(/8/);
    expect(when(new Date(2025, 10, 8, 12, 3), now)).toMatch(/2025/);
    expect(when('not a time', now)).toBe('');
  });
});
