// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import { describe, expect, it } from 'vitest';
import {
  canCancel,
  canRetry,
  cancelImport,
  importFromUrl,
  importFromWatchedRoot,
  importStateLabel,
  importStateMark,
  loadImportSources,
  loadImports,
  retryImport,
  sendTorrent,
  sourceLabel,
  toBase64,
  torrentOutcome,
  torrentRequest,
  MAX_TORRENT_BYTES,
  transferFraction,
  uploadInParts,
  uploadKey,
  urlImportRequest,
  type UploadMemory,
  type UploadView,
  type ImportView,
} from './imports';

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
      body: typeof init?.body === 'string' ? JSON.parse(init.body) : init?.body,
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

const job = (overrides: Partial<ImportView> = {}): ImportView => ({
  id: 'import-1',
  source_kind: 'upload',
  source_filename: 'disc.iso',
  state: 'requested',
  is_settled: false,
  bytes_expected: 1000,
  bytes_received: 0,
  artifact_id: null,
  error_code: null,
  error_detail: null,
  resumes_from: null,
  warnings: [],
  created_by: 'operator',
  created_at: '2026-01-01T00:00:00Z',
  updated_at: '2026-01-01T00:00:00Z',
  completed_at: null,
  ...overrides,
});

describe('loadImports', () => {
  it('distinguishes an empty list from a populated one', async () => {
    const empty = await loadImports(
      stubFetch({ '/api/v1/imports': { status: 200, body: { items: [], next_cursor: null } } }),
    );
    expect(empty.kind).toBe('empty');

    const populated = await loadImports(
      stubFetch({
        '/api/v1/imports': { status: 200, body: { items: [job()], next_cursor: null } },
      }),
    );
    expect(populated.kind).toBe('ready');
  });

  it('surfaces a problem document as an error', async () => {
    const view = await loadImports(
      stubFetch({
        '/api/v1/imports': {
          status: 503,
          body: { type: 'about:blank', title: 't', status: 503, code: 'X', detail: 'd' },
        },
      }),
    );
    expect(view.kind).toBe('error');
  });
});

describe('loadImportSources', () => {
  it('reads the configured roots and the upload limit', async () => {
    const view = await loadImportSources(
      stubFetch({
        '/api/v1/import-sources': {
          status: 200,
          body: { watch_roots: ['incoming'], max_upload_bytes: 1024 },
        },
      }),
    );
    expect(view.kind).toBe('ready');
    if (view.kind === 'ready') {
      expect(view.data.watch_roots).toEqual(['incoming']);
      expect(view.data.max_upload_bytes).toBe(1024);
    }
  });
});

describe('importFromWatchedRoot', () => {
  it('sends the root identifier and a relative path, never a host path', async () => {
    const recorded: Recorded[] = [];
    await importFromWatchedRoot(
      { path_id: 'incoming', relative_path: 'Example Disc/disc.iso' },
      stubFetch({ '/api/v1/imports': { status: 202, body: job() } }, recorded),
    );
    expect(recorded[0]?.method).toBe('POST');
    expect(recorded[0]?.body).toEqual({
      path_id: 'incoming',
      relative_path: 'Example Disc/disc.iso',
    });
  });

  it('returns a refusal rather than throwing', async () => {
    const result = await importFromWatchedRoot(
      { path_id: 'incoming', relative_path: '../escape' },
      stubFetch({
        '/api/v1/imports': {
          status: 422,
          body: {
            type: 'about:blank',
            title: 'Request is not valid',
            status: 422,
            code: 'VALIDATION_FAILED',
            detail: 'it contains a .. component',
          },
        },
      }),
    );
    expect(result.kind).toBe('error');
    if (result.kind === 'error') expect(result.problem.code).toBe('VALIDATION_FAILED');
  });
});

describe('commands', () => {
  it('posts to the cancel and retry routes', async () => {
    const recorded: Recorded[] = [];
    await cancelImport(
      'import-1',
      stubFetch({ '/api/v1/imports/import-1/cancel': { status: 200, body: job() } }, recorded),
    );
    await retryImport(
      'import-1',
      stubFetch({ '/api/v1/imports/import-1/retry': { status: 200, body: job() } }, recorded),
    );
    expect(recorded.map((entry) => entry.path)).toEqual([
      '/api/v1/imports/import-1/cancel',
      '/api/v1/imports/import-1/retry',
    ]);
    expect(recorded.every((entry) => entry.method === 'POST')).toBe(true);
  });
});

describe('offering actions', () => {
  it('offers cancelling only while the import is running', () => {
    expect(canCancel(job({ state: 'hashing' }))).toBe(true);
    expect(canCancel(job({ state: 'complete', is_settled: true }))).toBe(false);
  });

  it('offers a retry for anything that failed or was stopped', () => {
    for (const state of ['failed_terminal', 'failed_retryable', 'canceled', 'quarantined']) {
      expect(canRetry(job({ state, is_settled: true }))).toBe(true);
    }
  });

  it('does not offer a retry for a completed import', () => {
    // Its artifact is already in the library; importing the same bytes again
    // is a new import.
    expect(canRetry(job({ state: 'complete', is_settled: true }))).toBe(false);
  });
});

describe('labels', () => {
  it('translates pipeline states into words an operator uses', () => {
    expect(importStateLabel('registering')).toBe('Filing in the library');
    expect(importStateLabel('complete')).toBe('Imported');
  });

  it('distinguishes a failure that will be retried from one that will not', () => {
    expect(importStateLabel('failed_retryable')).toContain('try again');
    expect(importStateLabel('failed_terminal')).toBe('Failed');
  });

  it('passes an unknown state through rather than inventing one', () => {
    expect(importStateLabel('melting')).toBe('melting');
  });

  it('marks every state with a symbol, so status is never colour alone', () => {
    for (const state of ['requested', 'complete', 'failed_terminal', 'canceled', 'anything']) {
      expect(importStateMark(state).length).toBeGreaterThan(0);
    }
  });
});

describe('progress', () => {
  it('reports a fraction only while bytes are moving', () => {
    // Hashing and inspecting report nothing, and inventing a percentage for
    // them would be a progress bar that lies.
    expect(transferFraction(job({ state: 'acquiring', bytes_received: 500 }))).toBe(0.5);
    expect(transferFraction(job({ state: 'hashing', bytes_received: 500 }))).toBeNull();
    expect(transferFraction(job({ state: 'complete', is_settled: true }))).toBeNull();
  });

  it('reports nothing when the source never said how big it was', () => {
    expect(transferFraction(job({ state: 'acquiring', bytes_expected: null }))).toBeNull();
    expect(transferFraction(job({ state: 'acquiring', bytes_expected: 0 }))).toBeNull();
  });

  it('never reports more than complete', () => {
    expect(
      transferFraction(job({ state: 'acquiring', bytes_expected: 100, bytes_received: 250 })),
    ).toBe(1);
  });

  it('asks the server to fetch a URL, sending a digest only when one was typed', async () => {
    const recorded: Recorded[] = [];
    const fetcher = stubFetch(
      { '/api/v1/imports/url': { status: 202, body: { id: 'i1', state: 'requested' } } },
      recorded,
    );
    const result = await importFromUrl(
      urlImportRequest('  https://example.org/d.iso ', ''),
      fetcher,
    );
    expect(result.kind).toBe('ready');
    expect(recorded[0]).toMatchObject({
      path: '/api/v1/imports/url',
      method: 'POST',
      body: { url: 'https://example.org/d.iso' },
    });
    expect(urlImportRequest('https://x/y', ' AB CD ')).toEqual({
      url: 'https://x/y',
      expected_sha256: 'abcd',
    });
  });

  it('says where an import came from, and that a URL is downloading', () => {
    expect(sourceLabel('http_url')).toBe('URL');
    expect(sourceLabel('watch_folder')).toBe('Watched folder');
    expect(sourceLabel('something_new')).toBe('something new');
    expect(importStateLabel('acquiring', 'http_url')).toBe('Downloading');
    expect(importStateLabel('acquiring', 'watch_folder')).toBe('Copying in');
  });
});

describe('uploads sent in parts', () => {
  const file = new File([new Uint8Array(10).fill(7)], 'disc.iso', { lastModified: 1 });

  function view(received: number[]): UploadView {
    return {
      id: 'u1',
      filename: 'disc.iso',
      total_bytes: 10,
      part_size: 4,
      part_count: 3,
      received_parts: received,
      bytes_received: received.reduce((sum, part) => sum + (part === 2 ? 2 : 4), 0),
      state: 'open',
      expires_at: '2026-10-26T00:00:00Z',
    };
  }

  function memory(initial: Record<string, string> = {}): UploadMemory & {
    entries: Record<string, string>;
  } {
    const entries = { ...initial };
    return {
      entries,
      get: (key) => entries[key] ?? null,
      set: (key, value) => {
        entries[key] = value;
      },
      remove: (key) => {
        delete entries[key];
      },
    };
  }

  /** A server that remembers which parts arrived. */
  function server(start: number[], failPart?: number) {
    const received = [...start];
    const asked: { method: string; path: string; headers: Headers }[] = [];
    const fetcher = (async (input: string | URL | Request, init?: RequestInit) => {
      const path = String(input);
      const method = init?.method ?? 'GET';
      asked.push({ method, path, headers: new Headers(init?.headers) });
      const reply = (status: number, body: unknown) =>
        ({ ok: status < 300, status, json: async () => body }) as Response;
      if (method === 'GET' && path === '/api/v1/uploads/u1') return reply(200, view(received));
      if (method === 'GET') return reply(404, { code: 'NOT_FOUND', detail: 'no such upload' });
      if (method === 'POST' && path === '/api/v1/uploads') return reply(201, view([]));
      const part = /\/parts\/(\d+)$/.exec(path);
      if (method === 'PUT' && part) {
        const number = Number(part[1]);
        if (number === failPart) {
          return reply(503, {
            code: 'STORAGE_UNAVAILABLE',
            detail: 'the part could not be staged',
          });
        }
        if (!received.includes(number)) received.push(number);
        return reply(200, view(received));
      }
      if (method === 'POST' && path === '/api/v1/uploads/u1/complete') {
        return reply(202, { ...job(), id: 'u1', state: 'staged' });
      }
      throw new Error(`unexpected ${method} ${path}`);
    }) as typeof fetch;
    return { fetcher, asked };
  }

  it('starts an upload, sends every part with its digest, and completes it', async () => {
    const { fetcher, asked } = server([]);
    const remembered = memory();
    const progress: number[] = [];
    const result = await uploadInParts(file, (sent) => progress.push(sent), fetcher, remembered);
    expect(result.kind).toBe('ready');
    expect(asked.map((call) => `${call.method} ${call.path}`)).toEqual([
      'POST /api/v1/uploads',
      'PUT /api/v1/uploads/u1/parts/0',
      'PUT /api/v1/uploads/u1/parts/1',
      'PUT /api/v1/uploads/u1/parts/2',
      'POST /api/v1/uploads/u1/complete',
    ]);
    expect(asked[1]?.headers.get('x-content-sha256')).toMatch(/^[0-9a-f]{64}$/);
    expect(progress).toEqual([0, 4, 8, 10]);
    // Forgotten once complete.
    expect(remembered.entries).toEqual({});
  });

  it('carries on with the upload the page started before, sending only what is missing', async () => {
    const { fetcher, asked } = server([0, 1]);
    const result = await uploadInParts(
      file,
      () => {},
      fetcher,
      memory({ [uploadKey(file)]: 'u1' }),
    );
    expect(result.kind).toBe('ready');
    expect(asked.map((call) => `${call.method} ${call.path}`)).toEqual([
      'GET /api/v1/uploads/u1',
      'PUT /api/v1/uploads/u1/parts/2',
      'POST /api/v1/uploads/u1/complete',
    ]);
  });

  it('starts again when the remembered upload is gone', async () => {
    const { fetcher, asked } = server([]);
    const remembered = memory({ [uploadKey(file)]: 'gone' });
    await uploadInParts(file, () => {}, fetcher, remembered);
    expect(asked[0]?.path).toBe('/api/v1/uploads/gone');
    expect(asked[1]).toMatchObject({ method: 'POST', path: '/api/v1/uploads' });
  });

  it('tries a failing part three times, then stops and keeps the upload to resume', async () => {
    const { fetcher, asked } = server([], 1);
    const remembered = memory();
    const result = await uploadInParts(file, () => {}, fetcher, remembered);
    expect(result.kind).toBe('error');
    expect(asked.filter((call) => call.path.endsWith('/parts/1'))).toHaveLength(3);
    expect(remembered.entries[uploadKey(file)]).toBe('u1');
  });
});

describe('torrents for a download client', () => {
  it('names the source and the waiting in words', () => {
    expect(sourceLabel('qbittorrent_job')).toBe('qBittorrent');
    expect(importStateLabel('acquiring', 'qbittorrent_job')).toBe('Downloading in qBittorrent');
    expect(importStateLabel('acquiring', 'qbittorrent_job', 'INTEGRATION_OFF')).toBe(
      'Waiting for qBittorrent',
    );
    expect(importStateLabel('acquiring', 'http_url', 'X')).toBe('Downloading');
  });

  it('writes bytes as base64, however many', () => {
    expect(toBase64(new Uint8Array([]))).toBe('');
    expect(toBase64(new Uint8Array([100, 101, 0, 255]))).toBe('ZGUA/w==');
    const big = new Uint8Array(100_000).map((_, i) => i % 256);
    expect(atob(toBase64(big)).length).toBe(100_000);
  });

  it('sends the file when there is one, else the magnet, else nothing', async () => {
    const file = new File([new Uint8Array([1, 2, 3])], 'x.torrent');
    expect(await torrentRequest('magnet:?xt=x', file)).toEqual({ torrent: 'AQID' });
    expect(await torrentRequest('  magnet:?xt=x ', null)).toEqual({ magnet: 'magnet:?xt=x' });
    expect(await torrentRequest('   ', undefined)).toBeNull();
    const huge = { size: MAX_TORRENT_BYTES + 1 } as File;
    expect(await torrentRequest('', huge)).toBeNull();
  });

  it('says what handing over came to', () => {
    const job = {
      integration_id: 'q1',
      identity: 'h',
      client_job_id: 'h',
      import_id: 'i',
      outcome: 'created',
      artifact_id: null,
    };
    expect(torrentOutcome(job, 'NAS')).toMatch(/^Handed to NAS\./);
    expect(torrentOutcome({ ...job, outcome: 'already_active' }, 'NAS')).toMatch(/under way/);
    expect(torrentOutcome({ ...job, outcome: 'already_imported' }, 'NAS')).toMatch(/library/);
    expect(torrentOutcome({ ...job, outcome: 'resubmitted' }, 'NAS')).toMatch(/again/);
  });

  it('posts to the integration, escaped', async () => {
    const seen: string[] = [];
    const fetcher = (async (input: string | URL | Request, init?: RequestInit) => {
      seen.push(`${init?.method} ${String(input)} ${String(init?.body)}`);
      return {
        ok: true,
        status: 202,
        headers: new Headers(),
        json: async () => ({ outcome: 'created' }),
      } as Response;
    }) as typeof fetch;
    const result = await sendTorrent('a/b', { magnet: 'magnet:?xt=x' }, fetcher);
    expect(result.kind).toBe('ready');
    expect(seen).toEqual(['POST /api/v1/integrations/a%2Fb/jobs {"magnet":"magnet:?xt=x"}']);
  });
});
