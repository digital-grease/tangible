// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
import { describe, expect, it, vi } from 'vitest';
import {
  ATTESTATION,
  KINDS,
  STATE_FILTERS,
  candidateLine,
  createRequest,
  declineRequest,
  detailsOf,
  imdbId,
  kindLabel,
  loadRequests,
  providerLabel,
  refLabel,
  requestFields,
  requestNext,
  requestRefusal,
  requestStateLabel,
  requestStateTone,
  searchMetadata,
  sourceNote,
} from './requests';

function recorder() {
  const asked: { url: string; method: string; body?: string }[] = [];
  const fetcher = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
    asked.push({
      url: String(input),
      method: init?.method ?? 'GET',
      ...(typeof init?.body === 'string' ? { body: init.body } : {}),
    });
    return { ok: true, status: 200, json: async () => ({ items: [] }), headers: new Headers() };
  }) as unknown as typeof fetch;
  return { asked, fetcher };
}

describe('requests', () => {
  it('states the attestation as the server does', () => {
    // The server's refusal quotes these words; the box must show the same.
    expect(ATTESTATION).toBe('I legally own this item and want a digital image of it.');
  });

  it('names every kind and state the server knows', () => {
    expect(KINDS.map((kind) => kind.value)).toEqual([
      'game',
      'music',
      'movie',
      'television',
      'software',
      'other',
    ]);
    expect(kindLabel('movie')).toBe('Film');
    const states = [
      'pending_approval',
      'approved',
      'declined',
      'canceled',
      'downloading',
      'fulfilled',
      'failed',
    ];
    for (const state of states) {
      expect(requestStateLabel(state)).not.toBe(state);
      expect(requestNext({ state })).not.toBe('');
    }
    expect(STATE_FILTERS[0]).toEqual({ value: '', label: 'Every request' });
    expect(requestStateTone('pending_approval')).toBe('warn');
    expect(requestStateTone('fulfilled')).toBe('ok');
    expect(requestStateTone('declined')).toBe('bad');
    expect(requestStateTone('downloading')).toBe('busy');
    expect(requestStateTone('canceled')).toBe('idle');
  });

  it('sends only the details filled in', () => {
    expect(detailsOf({ platform: ' Sony PlayStation ', year: '', region: '  ' })).toEqual({
      platform: 'Sony PlayStation',
    });
  });

  it('finds an IMDb title in an ID or an IMDb address, and nowhere else', () => {
    expect(imdbId('')).toBeNull();
    expect(imdbId(' tt0083658 ')).toBe('tt0083658');
    expect(imdbId('https://www.imdb.com/title/tt0083658/')).toBe('tt0083658');
    expect(imdbId('https://m.imdb.com/title/tt0083658/?ref_=nv')).toBe('tt0083658');
    expect(imdbId('https://imdb.com/title/tt12345678')).toBe('tt12345678');
    expect(imdbId('tt12')).toBeUndefined();
    expect(imdbId('https://example.com/title/tt0083658/')).toBeUndefined();
    expect(imdbId('https://imdb.com.example.com/title/tt0083658/')).toBeUndefined();
    expect(imdbId('https://www.imdb.com/name/nm0000148/')).toBeUndefined();
    expect(imdbId('Blade Runner')).toBeUndefined();
  });

  it('links a reference out only where it is safe to', () => {
    expect(refLabel({ provider: 'imdb', external_id: 'tt0083658' })).toEqual({
      label: 'IMDb tt0083658',
      href: 'https://www.imdb.com/title/tt0083658/',
    });
    const musicbrainz = refLabel({ provider: 'musicbrainz', external_id: 'a/../b' });
    expect(musicbrainz.href).toBe('https://musicbrainz.org/release/a%2F..%2Fb');
    expect(refLabel({ provider: 'dat', external_id: 'Some Game (USA)' }).href).toBeNull();
  });

  it('asks the server with the filter and the cursor', async () => {
    const { asked, fetcher } = recorder();
    await loadRequests({}, fetcher);
    await loadRequests({ state: 'pending_approval', cursor: 'abc' }, fetcher);
    expect(asked.map((call) => call.url)).toEqual([
      '/api/v1/requests?limit=50',
      '/api/v1/requests?limit=50&state=pending_approval&cursor=abc',
    ]);
  });

  it('sends the attestation and a decline reason in the body', async () => {
    const { asked, fetcher } = recorder();
    await createRequest(
      { kind: 'game', title: 'A', details: {}, refs: [], note: null, attests_ownership: true },
      fetcher,
    );
    await declineRequest('r/1', 'no', fetcher);
    expect(asked[0]).toMatchObject({ url: '/api/v1/requests', method: 'POST' });
    expect(JSON.parse(asked[0]?.body ?? '{}')).toMatchObject({ attests_ownership: true });
    expect(asked[1]).toEqual({
      url: '/api/v1/requests/r%2F1/decline',
      method: 'POST',
      body: '{"reason":"no"}',
    });
  });

  it('says what to do about a refusal it knows', () => {
    expect(requestRefusal('ATTESTATION_REQUIRED')).toMatch(/Tick the box/);
    expect(requestRefusal('RATE_LIMITED')).toMatch(/Try again later/);
    expect(requestRefusal('CONFLICT')).toBeNull();
  });

  it('searches with the words and the kind, and without a kind for none', async () => {
    const { asked, fetcher } = recorder();
    await searchMetadata('blade runner', 'movie', fetcher);
    await searchMetadata('tt0083658', null, fetcher);
    expect(asked.map((call) => call.url)).toEqual([
      '/api/v1/metadata/search?q=blade+runner&kind=movie',
      '/api/v1/metadata/search?q=tt0083658',
    ]);
  });

  it('says what a source that did not answer means', () => {
    const base = { provider: 'igdb', count: 0 };
    expect(sourceNote({ ...base, state: 'answered', count: 3 })).toBeNull();
    expect(sourceNote({ ...base, state: 'unconfigured', detail: 'add it' })).toBe('add it');
    expect(sourceNote({ ...base, state: 'off' })).toBe('IGDB is turned off.');
    expect(
      sourceNote({ ...base, state: 'failed', error_code: 'INTEGRATION_AUTH_FAILED', detail: 'no' }),
    ).toBe('IGDB did not answer: no');
    expect(providerLabel('dat')).toBe('DATs');
  });

  it('puts what a candidate says in a line', () => {
    expect(
      candidateLine({
        provider: 'musicbrainz',
        kind: 'music',
        title: 'Dark Side of the Moon',
        details: { artist: 'Pink Floyd', year: '1994', region: 'GB', format: 'CD', barcode: '1' },
        refs: [],
      }),
    ).toBe('MusicBrainz · Pink Floyd · 1994 · GB · CD');
  });

  it('makes a request from what was picked, the form over it', () => {
    const picked = {
      details: { artist: 'Pink Floyd', year: '1994', format: 'CD' },
      refs: [{ provider: 'musicbrainz', external_id: 'a' }],
    };
    // The year changed in the form; the artist has no field and is kept.
    expect(requestFields(picked, { year: '1973', region: '' }, null)).toEqual({
      details: { artist: 'Pink Floyd', format: 'CD', year: '1973' },
      refs: [{ provider: 'musicbrainz', external_id: 'a' }],
    });
    // A year cleared in the form is cleared.
    expect(requestFields(picked, { year: '' }, null).details.year).toBeUndefined();
    // An IMDb ID typed is added once.
    const film = { details: {}, refs: [{ provider: 'imdb', external_id: 'tt0083658' }] };
    expect(requestFields(film, {}, 'tt0083658').refs).toHaveLength(1);
    expect(requestFields(null, {}, 'tt0083658').refs).toEqual([
      { provider: 'imdb', external_id: 'tt0083658' },
    ]);
  });
});
