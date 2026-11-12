// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import { describe, expect, it } from 'vitest';
import {
  ACTION_KINDS,
  actionLabel,
  actorLabel,
  loadAuditEvents,
  metadataText,
  outcomeLabel,
  outcomeTone,
} from './audit';

describe('reading the audit log', () => {
  it('asks with only the filters given, trimmed, and the cursor', async () => {
    const seen: string[] = [];
    const fetcher = (async (input: string | URL | Request) => {
      seen.push(String(input));
      return {
        ok: true,
        status: 200,
        headers: new Headers(),
        json: async () => ({ items: [], next_cursor: null }),
      } as Response;
    }) as typeof fetch;
    await loadAuditEvents({}, null, fetcher);
    await loadAuditEvents({ action: 'worker.', actorId: ' alice ', targetId: '' }, 'c1', fetcher);
    expect(seen).toEqual([
      '/api/v1/audit-events?limit=50',
      '/api/v1/audit-events?limit=50&action=worker.&actor_id=alice&cursor=c1',
    ]);
  });

  it('says actions, outcomes and actors in words, and the rest as stored', () => {
    expect(actionLabel('worker.revoked')).toBe('Worker revoked');
    expect(actionLabel('something.new')).toBe('something.new');
    expect(outcomeLabel('denied')).toBe('Refused');
    expect(outcomeTone('success')).toBe('ok');
    expect(outcomeTone('failure')).toBe('bad');
    expect(outcomeTone('denied')).toBe('warn');
    expect(actorLabel({ actor_type: 'user', actor_id: 'alice' })).toBe('alice');
    expect(actorLabel({ actor_type: 'system', actor_id: 'restore-check' })).toBe(
      'Tangible (restore-check)',
    );
    expect(metadataText({})).toBe('');
    expect(metadataText({ from: 'online' })).toBe('{"from":"online"}');
  });

  it('filters by kinds that end in a dot, and everything', () => {
    expect(ACTION_KINDS[0]).toEqual({ value: '', label: 'Everything' });
    expect(ACTION_KINDS.slice(1).every((kind) => kind.value.endsWith('.'))).toBe(true);
  });
});
