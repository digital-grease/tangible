// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import { describe, expect, it } from 'vitest';
import {
  burnsSummary,
  importsSummary,
  integritySummary,
  needsSomebody,
  storageSummary,
  workersSummary,
  type Dashboard,
} from './dashboard';

const quiet: Dashboard = {
  imports: { active: 0, failed: 0 },
  burns: { queued: 0, waiting_for_media: 0, active: 0, needing_attention: 0 },
  workers: { online: 1, silent: 0, draining: 0, pending: 0, incompatible: 0 },
  integrity: { damaged: 0, unchecked: 0 },
  storage: { objects: 1, stored_bytes: 2048, free_bytes: null, total_bytes: null },
  recent_burns: [],
  attention: [],
};

describe('the dashboard in words', () => {
  it('says when there is nothing to do', () => {
    expect(importsSummary(quiet.imports)).toBe(
      'Nothing being imported, and nothing failed this week',
    );
    expect(burnsSummary(quiet.burns)).toBe('No burns waiting or under way');
    expect(workersSummary(quiet.workers)).toBe('1 worker online');
    expect(integritySummary(quiet.integrity)).toBe('Every artifact checked and intact');
    expect(needsSomebody(quiet)).toBe(false);
  });

  it('puts what needs somebody first and leaves out what is nothing', () => {
    const burns = { queued: 2, waiting_for_media: 1, active: 0, needing_attention: 1 };
    expect(burnsSummary(burns)).toBe('1 burn needing attention, 1 waiting for a disc, 2 queued');
    expect(importsSummary({ active: 3, failed: 1 })).toBe(
      '3 imports under way, 1 failed this week',
    );
    expect(workersSummary({ online: 0, silent: 1, draining: 1, pending: 0, incompatible: 0 })).toBe(
      '0 workers online, 1 not heard from lately, 1 draining',
    );
    expect(integritySummary({ damaged: 2, unchecked: 5 })).toBe(
      '2 artifacts damaged, 5 not checked yet',
    );
    expect(needsSomebody({ ...quiet, burns })).toBe(true);
    expect(needsSomebody({ ...quiet, workers: { ...quiet.workers, silent: 1 } })).toBe(true);
  });

  it('says how much is stored, and the room left when it is known', () => {
    expect(storageSummary(quiet.storage)).toMatch(/^2(\.0)? KiB in 1 object$/);
    expect(storageSummary({ ...quiet.storage, free_bytes: 1024, total_bytes: 4096 })).toMatch(
      /free of/,
    );
  });
});
