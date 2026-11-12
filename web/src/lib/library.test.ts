// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import { describe, expect, it } from 'vitest';
import {
  archiveLabel,
  deletionLabel,
  isOriginal,
  keepArtifact,
  requestDeletion,
  formatBytes,
  formatLabel,
  filtersNothing,
  keepsSubchannel,
  libraryQuery,
  integrityShort,
  integrityTone,
  originLabel,
  validationTone,
  notBurnedAsItIs,
  loadArtifact,
  loadLibrary,
  validationLabel,
  validationMark,
  componentUrl,
  msf,
  trackFlagLabel,
  trackModeLabel,
  discSummary,
  trackDetails,
  type DiscLayout,
  type TrackView,
} from './library';

function stubFetch(routes: Record<string, { status: number; body: unknown }>): typeof fetch {
  return (async (input: string | URL | Request) => {
    const path = typeof input === 'string' ? input : input.toString();
    const route = routes[path];
    if (!route) throw new Error(`unexpected request to ${path}`);
    return {
      ok: route.status >= 200 && route.status < 300,
      status: route.status,
      json: async () => route.body,
    } as Response;
  }) as typeof fetch;
}

const page = (items: unknown[], next: string | null = null) => ({
  items,
  next_cursor: next,
});

const artifact = {
  id: 'abc',
  format: 'iso',
  artifact_kind: 'single_file_image',
  validation_state: 'valid',
  component_count: 1,
  total_bytes: 2048,
  source_filename: 'disc.iso',
  has_warnings: false,
};

describe('loadLibrary', () => {
  it('reports a populated library as ready', async () => {
    const view = await loadLibrary(
      stubFetch({ '/api/v1/artifacts': { status: 200, body: page([artifact]) } }),
    );
    expect(view.kind).toBe('ready');
  });

  it('distinguishes empty from ready', async () => {
    // An empty library needs its own copy telling the operator what to do
    // next, so it cannot be a zero-length success.
    const view = await loadLibrary(
      stubFetch({ '/api/v1/artifacts': { status: 200, body: page([]) } }),
    );
    expect(view.kind).toBe('empty');
  });

  it('surfaces a problem document as an error', async () => {
    const view = await loadLibrary(
      stubFetch({
        '/api/v1/artifacts': {
          status: 503,
          body: {
            type: 'about:blank',
            title: 'Storage is unavailable',
            status: 503,
            code: 'STORAGE_UNAVAILABLE',
            detail: 'the library could not be read',
          },
        },
      }),
    );
    expect(view.kind).toBe('error');
    if (view.kind === 'error') expect(view.problem.code).toBe('STORAGE_UNAVAILABLE');
  });

  it('turns a transport failure into the same shape as a problem', async () => {
    // A dead server returns no document, but error rendering must have one
    // path rather than two.
    const view = await loadLibrary((() => {
      throw new Error('connection refused');
    }) as unknown as typeof fetch);
    expect(view.kind).toBe('error');
    if (view.kind === 'error') {
      expect(view.problem.code).toBe('TRANSPORT_FAILURE');
      expect(view.problem.detail).toContain('connection refused');
    }
  });

  it('does not mistake a non-problem error body for a problem', async () => {
    const view = await loadLibrary(
      stubFetch({ '/api/v1/artifacts': { status: 500, body: { oops: true } } }),
    );
    expect(view.kind).toBe('error');
    if (view.kind === 'error') expect(view.problem.code).toBe('TRANSPORT_FAILURE');
  });

  it('passes the cursor through when paging', async () => {
    const view = await loadLibrary(
      stubFetch({
        '/api/v1/artifacts?cursor=abc%3D': { status: 200, body: page([artifact]) },
      }),
      'abc=',
    );
    expect(view.kind).toBe('ready');
  });
});

describe('loadArtifact', () => {
  it('reports a missing artifact as an error carrying its code', async () => {
    const view = await loadArtifact(
      'missing',
      stubFetch({
        '/api/v1/artifacts/missing': {
          status: 404,
          body: {
            type: 'about:blank',
            title: 'Resource not found',
            status: 404,
            code: 'NOT_FOUND',
            detail: 'no artifact with id missing',
          },
        },
      }),
    );
    expect(view.kind).toBe('error');
    if (view.kind === 'error') expect(view.problem.code).toBe('NOT_FOUND');
  });

  it('encodes the identifier in the path', async () => {
    // A caller-supplied id must not be able to alter the request path.
    const view = await loadArtifact(
      'a/b',
      stubFetch({ '/api/v1/artifacts/a%2Fb': { status: 200, body: { ...artifact, id: 'a/b' } } }),
    );
    expect(view.kind).toBe('ready');
  });
});

describe('formatBytes', () => {
  it('keeps small sizes exact', () => {
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(512)).toBe('512 B');
  });

  it('scales to binary units', () => {
    expect(formatBytes(2048)).toBe('2.0 KiB');
    expect(formatBytes(1024 ** 3)).toBe('1.0 GiB');
  });

  it('keeps one decimal, which is the difference between fitting a disc and not', () => {
    // 4.7 GiB versus 4.4 GiB decides whether a burn is possible.
    expect(formatBytes(Math.round(4.7 * 1024 ** 3))).toBe('4.7 GiB');
  });
});

describe('labels', () => {
  it('never promises more than was checked', () => {
    // "Structurally valid" is a claim about parsing, not about playback.
    expect(validationLabel('valid')).toBe('Structurally valid');
    expect(validationLabel('valid_with_warnings')).toBe('Structurally valid, with findings');
    for (const state of ['valid', 'invalid', 'quarantined', 'pending']) {
      expect(validationLabel(state).toLowerCase()).not.toContain('perfect');
      expect(validationLabel(state).toLowerCase()).not.toContain('will work');
    }
  });

  it('pairs every validation state with a non-colour mark', () => {
    // Status must never be communicated by colour alone.
    for (const state of ['valid', 'valid_with_warnings', 'invalid', 'quarantined', 'pending']) {
      expect(validationMark(state).length).toBeGreaterThan(0);
    }
    expect(validationMark('valid')).not.toBe(validationMark('invalid'));
  });

  it('falls back to the raw value rather than hiding an unknown state', () => {
    expect(validationLabel('something_new')).toBe('something_new');
    expect(formatLabel('something_new')).toBe('something_new');
  });

  it('renders known formats readably', () => {
    expect(formatLabel('iso')).toBe('ISO');
    expect(formatLabel('cue_bin')).toBe('CUE/BIN');
    expect(formatLabel('unknown')).toBe('Unrecognised');
    expect(formatLabel('ccd_img_sub')).toBe('CloneCD (CCD/IMG/SUB)');
    expect(formatLabel('mds_mdf')).toBe('Alcohol 120% (MDS/MDF)');
  });
});

describe('disc tracks', () => {
  it('counts sectors as a CD does', () => {
    expect(msf(0)).toBe('00:00:00');
    expect(msf(150)).toBe('00:02:00');
    expect(msf(74)).toBe('00:00:74');
    expect(msf(75 * 60 * 79 + 75 * 59 + 74)).toBe('79:59:74');
  });

  it('says what a mode and a flag are', () => {
    expect(trackModeLabel('AUDIO')).toBe('Audio');
    expect(trackModeLabel('mode1/2048')).toContain('mode 1');
    expect(trackModeLabel('CDG')).toBe('CDG');
    expect(trackFlagLabel('PRE')).toBe('pre-emphasis');
    expect(trackFlagLabel('XYZ')).toBe('XYZ');
  });

  it('builds a download address that cannot be steered elsewhere', () => {
    expect(componentUrl('a/b', 'c?d')).toBe('/api/v1/artifacts/a%2Fb/components/c%3Fd/content');
  });
});

describe('describing a disc', () => {
  const track = (overrides: Partial<TrackView>): TrackView =>
    ({ isrc: null, flags: [], ...overrides }) as TrackView;

  it('joins a code and flags without stray spaces', () => {
    expect(trackDetails(track({ isrc: 'USRC17607839', flags: ['DCP', 'PRE'] }))).toBe(
      'ISRC USRC17607839; copying permitted; pre-emphasis',
    );
    expect(trackDetails(track({ flags: ['4CH'] }))).toBe('four-channel audio');
    expect(trackDetails(track({}))).toBe('');
  });

  it('summarises tracks, sessions and catalogue', () => {
    const tracks = [track({}), track({})];
    expect(discSummary({ tracks, session_count: 1, catalog: null } as DiscLayout)).toBe(
      '2 tracks.',
    );
    expect(
      discSummary({
        tracks: [track({})],
        session_count: 2,
        catalog: '1234567890123',
      } as DiscLayout),
    ).toBe('1 track in 2 sessions, catalogue number 1234567890123.');
  });
});

describe('subchannel', () => {
  const component = (role: string) => ({
    id: role,
    logical_path: `disc.${role}`,
    role,
    length_bytes: 1,
    sha256: 'a'.repeat(64),
  });

  it('is kept only by an artifact with a subchannel component', () => {
    expect(
      keepsSubchannel({
        components: [component('descriptor'), component('track_data'), component('subchannel')],
      } as never),
    ).toBe(true);
    expect(
      keepsSubchannel({ components: [component('descriptor'), component('track_data')] } as never),
    ).toBe(false);
  });

  it('says why an image is not burned as it is, and what to burn instead', () => {
    const plain = [component('descriptor'), component('track_data')];
    expect(
      notBurnedAsItIs({
        format: 'ccd_img_sub',
        components: [...plain, component('subchannel')],
      } as never),
    ).toContain('Make a CUE/BIN of it');
    const mds = notBurnedAsItIs({ format: 'mds_mdf', components: plain } as never);
    expect(mds).toContain('Alcohol 120%');
    expect(mds).toContain('or an ISO');
    expect(notBurnedAsItIs({ format: 'chd', components: plain } as never)).toContain(
      'compressed bytes',
    );
    expect(notBurnedAsItIs({ format: 'cue_bin', components: plain } as never)).toBeNull();
  });
});

describe('library filters', () => {
  it('sends only the filters set, and the cursor after them', () => {
    expect(libraryQuery({})).toBe('');
    expect(libraryQuery({ format: '', linked: null })).toBe('');
    expect(libraryQuery({ format: 'iso', linked: false }, 'c1')).toBe(
      '?format=iso&linked=false&cursor=c1',
    );
    expect(libraryQuery({ integrity_state: 'damaged', linked: true })).toBe(
      '?integrity_state=damaged&linked=true',
    );
    expect(filtersNothing({ origin: '' })).toBe(true);
    expect(filtersNothing({ origin: 'derived' })).toBe(false);
  });

  it('lists the deleted only when asked', () => {
    expect(libraryQuery({ deleted: false })).toBe('');
    expect(libraryQuery({ deleted: true })).toBe('?deleted=true');
    expect(filtersNothing({ deleted: true })).toBe(false);
  });

  it('searches by name, trimmed, and treats blank as no search', () => {
    expect(libraryQuery({ name: '  vol 2 ' })).toBe('?name=vol+2');
    expect(libraryQuery({ name: '100%' })).toBe('?name=100%25');
    expect(libraryQuery({ name: '   ' })).toBe('');
    expect(filtersNothing({ name: '' })).toBe(true);
    expect(filtersNothing({ name: 'iso' })).toBe(false);
  });
});

describe('deleting an artifact', () => {
  it('knows an original, which needs saying so', () => {
    expect(isOriginal('imported_original')).toBe(true);
    expect(isOriginal('physical_dump')).toBe(true);
    expect(isOriginal('derived')).toBe(false);
  });

  it('says a deletion in words', () => {
    expect(deletionLabel(null)).toBeNull();
    expect(
      deletionLabel({
        state: 'deleted',
        requested_at: '2026-11-02T12:00:00Z',
        requested_by: 'admin',
        delete_after: null,
        deleted_at: '2026-11-09T12:00:00Z',
      }),
    ).toBe('Deleted');
    expect(
      deletionLabel({
        state: 'pending',
        requested_at: '2026-11-02T12:00:00Z',
        requested_by: 'admin',
        delete_after: '2026-11-09T12:00:00Z',
        deleted_at: null,
      }),
    ).toMatch(/^Pending deletion, after /);
  });

  it('asks and keeps on the deletion-requests route', async () => {
    const seen: { path: string; method: string; body: unknown }[] = [];
    const fetcher = (async (input: string | URL | Request, init?: RequestInit) => {
      seen.push({
        path: String(input),
        method: init?.method ?? 'GET',
        body: typeof init?.body === 'string' ? JSON.parse(init.body) : undefined,
      });
      return {
        ok: true,
        status: init?.method === 'DELETE' ? 204 : 202,
        headers: new Headers(),
        json: async () => ({ state: 'pending' }),
      } as Response;
    }) as typeof fetch;
    await requestDeletion('a 1', true, fetcher);
    await keepArtifact('a 1', fetcher);
    expect(seen).toEqual([
      {
        path: '/api/v1/artifacts/a%201/deletion-requests',
        method: 'POST',
        body: { confirm_original: true },
      },
      { path: '/api/v1/artifacts/a%201/deletion-requests', method: 'DELETE', body: undefined },
    ]);
  });
});

describe('archive formats', () => {
  it('are named as people know them', () => {
    expect(archiveLabel('zip')).toBe('ZIP');
    expect(archiveLabel('tar_xz')).toBe('xz-compressed tar');
    expect(archiveLabel('something')).toBe('something');
  });
});

describe('library states, for status marks', () => {
  it('gives validation and integrity a tone and short words', () => {
    expect(validationTone('valid')).toBe('ok');
    expect(validationTone('valid_with_warnings')).toBe('warn');
    expect(validationTone('quarantined')).toBe('bad');
    expect(validationTone('pending')).toBe('idle');
    expect(integrityShort('damaged')).toBe('Damaged');
    expect(integrityTone('damaged')).toBe('bad');
    expect(integrityTone('intact')).toBe('ok');
    expect(integrityTone('unchecked')).toBe('idle');
  });

  it('says where an artifact came from', () => {
    expect(originLabel('imported_original')).toBe('Imported original');
    expect(originLabel('physical_dump')).toBe('Ripped from a disc');
    expect(originLabel('derived')).toBe('Made here from another');
  });
});
