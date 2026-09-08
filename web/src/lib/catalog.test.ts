// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import { describe, expect, it } from 'vitest';
import {
  compatibilityLabel,
  createDisc,
  createTitle,
  discName,
  isComplete,
  linkArtifact,
  loadDiscs,
  loadTitles,
  mediaFamilyLabel,
  titleKindLabel,
  type DiscSetView,
  type DiscView,
} from './catalog';

interface Recorded {
  path: string;
  method: string;
  body: unknown;
}

function stubFetch(
  routes: Record<string, { status: number; body: unknown }>,
  recorded: Recorded[] = [],
): typeof fetch {
  return (async (input: string | URL | Request, init?: RequestInit) => {
    const path = typeof input === 'string' ? input : input.toString();
    recorded.push({
      path,
      method: init?.method ?? 'GET',
      body: typeof init?.body === 'string' ? JSON.parse(init.body) : undefined,
    });
    const route = routes[path];
    if (!route) throw new Error(`unexpected request to ${path}`);
    return {
      ok: route.status >= 200 && route.status < 300,
      status: route.status,
      json: async () => route.body,
    } as Response;
  }) as typeof fetch;
}

const disc = (overrides: Partial<DiscView> = {}): DiscView => ({
  id: 'disc-1',
  disc_set_id: 'set-1',
  sequence_number: 1,
  display_name: null,
  media_family: 'dvd',
  region: null,
  volume_label: null,
  compatibility_claim: 'unknown',
  artifact_count: 0,
  copy_count: 0,
  created_at: '2026-01-01T00:00:00Z',
  ...overrides,
});

const set = (overrides: Partial<DiscSetView> = {}): DiscSetView => ({
  id: 'set-1',
  edition_id: 'edition-1',
  name: 'Two-disc set',
  set_kind: 'multi_disc',
  disc_count_expected: 2,
  disc_count: 1,
  created_at: '2026-01-01T00:00:00Z',
  ...overrides,
});

describe('loadTitles', () => {
  it('distinguishes an empty catalog from a populated one', async () => {
    const empty = await loadTitles(
      stubFetch({ '/api/v1/titles': { status: 200, body: { items: [], next_after: null } } }),
    );
    expect(empty.kind).toBe('empty');
  });

  it('passes a search through', async () => {
    const recorded: Recorded[] = [];
    await loadTitles(
      stubFetch(
        {
          '/api/v1/titles?search=example': {
            status: 200,
            body: { items: [], next_after: null },
          },
        },
        recorded,
      ),
      'example',
    );
    expect(recorded[0]?.path).toBe('/api/v1/titles?search=example');
  });
});

describe('creating the chain', () => {
  it('posts a title with what the operator typed', async () => {
    const recorded: Recorded[] = [];
    await createTitle(
      { display_title: 'Example', kind: 'movie' },
      stubFetch({ '/api/v1/titles': { status: 201, body: {} } }, recorded),
    );
    expect(recorded[0]?.method).toBe('POST');
    expect(recorded[0]?.body).toEqual({ display_title: 'Example', kind: 'movie' });
  });

  it('posts a disc under its set', async () => {
    const recorded: Recorded[] = [];
    await createDisc(
      'set-1',
      { sequence_number: 2 },
      stubFetch({ '/api/v1/disc-sets/set-1/discs': { status: 201, body: disc() } }, recorded),
    );
    expect(recorded[0]?.path).toBe('/api/v1/disc-sets/set-1/discs');
  });

  it('reports a refused create rather than throwing', async () => {
    const result = await createDisc(
      'set-1',
      { sequence_number: 1 },
      stubFetch({
        '/api/v1/disc-sets/set-1/discs': {
          status: 409,
          body: {
            type: 'about:blank',
            title: 'Conflicting request',
            status: 409,
            code: 'CONFLICT',
            detail: 'that disc number is already in this set',
          },
        },
      }),
    );
    expect(result.kind).toBe('error');
    if (result.kind === 'error') expect(result.problem.code).toBe('CONFLICT');
  });

  it('links an artifact to a disc', async () => {
    const recorded: Recorded[] = [];
    await linkArtifact(
      'disc-1',
      'artifact-1',
      stubFetch({ '/api/v1/discs/disc-1/artifact-links': { status: 200, body: [] } }, recorded),
    );
    expect(recorded[0]?.body).toEqual({ artifact_id: 'artifact-1' });
  });

  it('encodes identifiers into paths', async () => {
    const recorded: Recorded[] = [];
    await loadDiscs(
      'a/b',
      stubFetch({ '/api/v1/disc-sets/a%2Fb/discs': { status: 200, body: [] } }, recorded),
    );
    expect(recorded[0]?.path).toBe('/api/v1/disc-sets/a%2Fb/discs');
  });
});

describe('describing what is catalogued', () => {
  it('names a disc by its own name, or by its number', () => {
    expect(discName(disc({ display_name: 'Feature' }))).toBe('Feature');
    expect(discName(disc({ sequence_number: 2 }))).toBe('Disc 2');
  });

  it('knows when a set is missing discs', () => {
    expect(isComplete(set({ disc_count: 1, disc_count_expected: 2 }))).toBe(false);
    expect(isComplete(set({ disc_count: 2, disc_count_expected: 2 }))).toBe(true);
    // A set that never said how many it shipped with cannot be incomplete.
    expect(isComplete(set({ disc_count: 1, disc_count_expected: null }))).toBe(true);
  });

  it('never promises that a burned disc will be accepted anywhere', () => {
    // The claim the project refuses to make. `unknown` is the honest default
    // and says so in words.
    expect(compatibilityLabel('unknown')).toContain('unknown');
    expect(compatibilityLabel('player_compatibility_expected')).toContain('Expected');
  });

  it('passes unknown values through rather than inventing labels', () => {
    expect(titleKindLabel('interpretive_dance')).toBe('interpretive_dance');
    expect(mediaFamilyLabel('laserdisc')).toBe('laserdisc');
    expect(compatibilityLabel('definitely_works')).toBe('definitely_works');
  });

  it('spells out the media families an operator recognises', () => {
    expect(mediaFamilyLabel('bluray')).toBe('Blu-ray');
    expect(mediaFamilyLabel('uhd_bluray')).toBe('UHD Blu-ray');
    expect(titleKindLabel('movie')).toBe('Film');
  });
});
