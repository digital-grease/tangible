// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import { describe, expect, it } from 'vitest';
import {
  canCancel,
  canRetry,
  cancelImport,
  importFromWatchedRoot,
  importStateLabel,
  importStateMark,
  loadImportSources,
  loadImports,
  retryImport,
  transferFraction,
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
});
