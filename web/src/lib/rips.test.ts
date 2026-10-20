// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
import { describe, expect, it, vi } from 'vitest';
import {
  cancelRip,
  loadRip,
  loadRipEvents,
  loadRips,
  requestRip,
  ripProgress,
  ripRefusal,
  ripStateLabel,
  type RipEventView,
} from './rips';

function answering() {
  return vi.fn(async () => ({
    ok: true,
    status: 200,
    json: async () => ({}),
  })) as unknown as typeof fetch;
}

function event(progress: number | null): RipEventView {
  return { sequence: 0, event_type: 'progress', code: 'RIP_PROGRESS', progress, worker_time: '' };
}

describe('rips', () => {
  it('asks the rip routes', async () => {
    const fetcher = answering();
    await requestRip('d/1', fetcher);
    expect(fetcher).toHaveBeenCalledWith(
      '/api/v1/drives/d%2F1/rips',
      expect.objectContaining({ method: 'POST', body: '{}' }),
    );
    await loadRips(fetcher);
    expect(fetcher).toHaveBeenCalledWith('/api/v1/rips?limit=20', expect.anything());
    await loadRip('r1', fetcher);
    expect(fetcher).toHaveBeenCalledWith('/api/v1/rips/r1', expect.anything());
    await loadRipEvents('r1', fetcher);
    expect(fetcher).toHaveBeenCalledWith('/api/v1/rips/r1/events', expect.anything());
    await cancelRip('r1', fetcher);
    expect(fetcher).toHaveBeenCalledWith(
      '/api/v1/rips/r1/cancel',
      expect.objectContaining({ method: 'POST' }),
    );
  });

  it('says every state in words', () => {
    for (const state of [
      'queued',
      'reading',
      'uploading',
      'registering',
      'complete',
      'canceled',
      'refused',
      'failed',
    ]) {
      expect(ripStateLabel(state)).not.toBe(state);
    }
    expect(ripStateLabel('something_new')).toBe('something_new');
  });

  it('explains the refusals an operator can act on', () => {
    expect(ripRefusal('NO_MEDIUM')).toContain('no disc');
    expect(ripRefusal('BLANK_MEDIUM')).toContain('blank');
    expect(ripRefusal('MEDIUM_UNSUPPORTED')).toContain('Only CDs');
    expect(ripRefusal('READ_FAILED')).toBeNull();
    expect(ripRefusal(null)).toBeNull();
  });

  it('takes progress from the newest event that has some', () => {
    expect(ripProgress([])).toBeNull();
    expect(ripProgress([event(0.25), event(0.5), event(null)])).toBe(50);
    expect(ripProgress([event(1.7)])).toBe(100);
  });
});
