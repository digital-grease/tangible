// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  MAX_ISSUE_URL_LENGTH,
  SHORTENED_MARKER,
  copyText,
  draftFromReport,
  errorReportRequest,
  forgetReportSettings,
  issueHost,
  issueLink,
  loadBurnReport,
  loadReportSettings,
  prepareErrorReport,
  removedSummary,
  reportMarkdown,
  reportSettingsOnce,
  routeTemplate,
  suggestsDefect,
  type ReportDraft,
} from './report';

const ISSUES = 'https://github.com/digital-grease/tangible/issues/new';

function draft(log: string, extra: Partial<ReportDraft> = {}): ReportDraft {
  return {
    template: 'burn_report.yml',
    fields: [
      { id: 'version', label: 'Tangible version', value: 'v0.1.0 (76fa515)' },
      { id: 'connection', label: 'Connection', value: '' },
      { id: 'worker_log', label: 'Worker log', value: log },
    ],
    longField: 'worker_log',
    removed: {},
    truncated: false,
    ...extra,
  };
}

function params(url: string): URLSearchParams {
  return new URL(url).searchParams;
}

describe('loadReportSettings', () => {
  it('asks the report settings route', async () => {
    const fetcher = vi.fn(async () => ({
      ok: true,
      status: 200,
      json: async () => ({ enabled: false, version: '0.1.0', build: '0.1.0, built locally' }),
    })) as unknown as typeof fetch;
    const result = await loadReportSettings(fetcher);
    expect(fetcher).toHaveBeenCalledWith('/api/v1/report-settings', expect.anything());
    expect(result.kind).toBe('ok');
  });
});

describe('issueLink', () => {
  it('fills in every field with a value and leaves empty ones out', () => {
    const link = issueLink(ISSUES, draft('line one\nline two\n', { title: 'Burn: write failed' }));
    expect(link.kind).toBe('embedded');
    const query = params(link.url);
    expect(query.get('template')).toBe('burn_report.yml');
    expect(query.get('title')).toBe('Burn: write failed');
    expect(query.get('version')).toBe('v0.1.0 (76fa515)');
    expect(query.has('connection')).toBe(false);
    expect(query.get('worker_log')).toBe('line one\nline two\n');
  });

  it('encodes spaces and symbols so the form receives them unchanged', () => {
    const link = issueLink(ISSUES, draft('a & b = c? #1 100%'));
    expect(link.url).not.toContain(' ');
    expect(params(link.url).get('worker_log')).toBe('a & b = c? #1 100%');
  });

  it('keeps the end of a long log, from a line start, within the limit', () => {
    const lines = Array.from({ length: 2000 }, (_, i) => `event ${i}: stage writing`);
    const log = `${lines.join('\n')}\nENGINE_FAILED: the drive stopped\n`;
    const link = issueLink(ISSUES, draft(log));
    expect(link.kind).toBe('shortened');
    expect(link.url.length).toBeLessThanOrEqual(MAX_ISSUE_URL_LENGTH);
    const value = params(link.url).get('worker_log') ?? '';
    expect(value.startsWith(SHORTENED_MARKER)).toBe(true);
    expect(value.endsWith('ENGINE_FAILED: the drive stopped\n')).toBe(true);
    const kept = value.slice(SHORTENED_MARKER.length);
    expect(kept.startsWith('event ')).toBe(true);
    expect(log.endsWith(kept)).toBe(true);
    if (link.kind === 'shortened') expect(link.field.value).toBe(log);
  });

  it('cuts a single enormous line in the middle when it must', () => {
    const log = 'x'.repeat(20000);
    const link = issueLink(ISSUES, draft(log));
    expect(link.kind).toBe('shortened');
    expect(link.url.length).toBeLessThanOrEqual(MAX_ISSUE_URL_LENGTH);
    const kept = (params(link.url).get('worker_log') ?? '').slice(SHORTENED_MARKER.length);
    expect(kept.length).toBeGreaterThan(1000);
    expect(log.endsWith(kept)).toBe(true);
  });

  it('never splits a character in two', () => {
    // An emoji is two UTF-16 units; encoding half of one throws.
    const log = '💿'.repeat(5000);
    for (const max of [3000, 3001, 3002, 3003]) {
      const link = issueLink(ISSUES, draft(log), max);
      expect(link.url.length).toBeLessThanOrEqual(max);
      const kept = (params(link.url).get('worker_log') ?? '').slice(SHORTENED_MARKER.length);
      expect(kept).toMatch(/^(💿)*$/u);
    }
  });

  it('leaves the log out when not even the marker fits', () => {
    const tight = issueLink(ISSUES, draft('a log\n'.repeat(10)), 150);
    expect(tight.kind).toBe('shortened');
    expect(params(tight.url).has('worker_log')).toBe(false);
  });

  it('builds a plain link when there is no long field', () => {
    const link = issueLink(ISSUES, { ...draft('ignored'), longField: undefined });
    expect(link.kind).toBe('embedded');
    expect(params(link.url).get('worker_log')).toBe('ignored');
  });
});

describe('reportMarkdown', () => {
  it('fences the log so backticks inside cannot close it', () => {
    const text = reportMarkdown(draft('a ``` b\nlast line\n'));
    expect(text).toContain('### Tangible version\n\nv0.1.0 (76fa515)');
    expect(text).toContain('````text\na ``` b\nlast line\n````');
    expect(text).not.toContain('### Connection');
  });
});

describe('removedSummary', () => {
  it('says what was replaced, in words', () => {
    expect(removedSummary({})).toBe('Nothing needed removing.');
    expect(removedSummary({ path: 1 })).toBe('Replaced before you saw it: 1 path.');
    expect(removedSummary({ path: 2, credential: 1, serial: 1 })).toBe(
      'Replaced before you saw it: 2 paths, 1 credential and 1 serial number.',
    );
    expect(removedSummary({ something_new: 3 })).toBe(
      'Replaced before you saw it: 3 something news.',
    );
  });
});

describe('issueHost', () => {
  it('names where the link goes', () => {
    expect(issueHost(ISSUES)).toBe('github.com');
    expect(issueHost('not a url')).toBe('not a url');
  });
});

describe('copyText', () => {
  it('uses the clipboard in a secure context', async () => {
    const writeText = vi.fn(async () => {});
    expect(await copyText('hello', { clipboard: { writeText }, isSecureContext: true })).toBe(true);
    expect(writeText).toHaveBeenCalledWith('hello');
  });

  it('falls back to selecting a hidden field over plain HTTP', async () => {
    const appended: { value: string; removed: boolean }[] = [];
    const doc = {
      body: { appendChild: (field: { value: string; removed: boolean }) => appended.push(field) },
      createElement: () => {
        const field = {
          value: '',
          removed: false,
          style: {},
          setAttribute: () => {},
          select: () => {},
          remove: () => {
            field.removed = true;
          },
        };
        return field;
      },
      execCommand: vi.fn(() => true),
    } as unknown as Document;
    const writeText = vi.fn(async () => {});
    expect(
      await copyText('hello', { clipboard: { writeText }, isSecureContext: false, document: doc }),
    ).toBe(true);
    expect(writeText).not.toHaveBeenCalled();
    expect(appended[0]?.value).toBe('hello');
    expect(appended[0]?.removed).toBe(true);
  });

  it('reports failure when nothing can copy', async () => {
    expect(await copyText('hello', { isSecureContext: false })).toBe(false);
  });
});

describe('burn reports from the server', () => {
  it('asks for the kind of report wanted', async () => {
    const fetcher = vi.fn(async () => ({
      ok: true,
      status: 200,
      json: async () => ({}),
    })) as unknown as typeof fetch;
    await loadBurnReport('a1', 'worked', fetcher);
    expect(fetcher).toHaveBeenCalledWith(
      '/api/v1/burn-attempts/a1/report?kind=worked',
      expect.anything(),
    );
  });

  it('becomes the draft the review dialog takes', () => {
    const draft = draftFromReport({
      template: 'burn_report.yml',
      title: 'Burn: write failed',
      fields: [{ id: 'version', label: 'Tangible version', value: 'v0.1.0 (76fa515)' }],
      long_field: 'worker_log',
      removed: { path: 2 },
      truncated: false,
    });
    expect(draft).toEqual({
      template: 'burn_report.yml',
      title: 'Burn: write failed',
      fields: [{ id: 'version', label: 'Tangible version', value: 'v0.1.0 (76fa515)' }],
      longField: 'worker_log',
      removed: { path: 2 },
      truncated: false,
    });
    const bare = draftFromReport({
      template: 'hardware_report.yml',
      title: null,
      fields: [],
      long_field: null,
      removed: {},
      truncated: false,
    });
    expect(bare.title).toBeUndefined();
    expect(bare.longField).toBeUndefined();
  });
});

describe('suggestsDefect', () => {
  it('offers a report for codes that mean the software went wrong', () => {
    for (const code of [
      'INTERNAL',
      'STORAGE_UNAVAILABLE',
      'MANIFEST_INVALID',
      'UNSUPPORTED_PROTOCOL',
    ]) {
      expect(suggestsDefect(code)).toBe(true);
    }
  });

  it('offers a report for a failure code', () => {
    expect(suggestsDefect('IMPORT_STORAGE_FAILED')).toBe(true);
    expect(suggestsDefect('ENGINE_FAILED')).toBe(true);
  });

  it('offers none for a request that was refused for its own reasons', () => {
    for (const code of [
      'VALIDATION_FAILED',
      'FORBIDDEN',
      'NOT_FOUND',
      'RATE_LIMITED',
      'CSRF_TOKEN_INVALID',
      'CONFLICT',
      'UNAUTHENTICATED',
      'TRANSPORT_FAILURE',
      'WRITE_IN_PROGRESS',
      'SOMETHING_NEW',
    ]) {
      expect(suggestsDefect(code)).toBe(false);
    }
  });
});

describe('routeTemplate', () => {
  it('replaces identifiers and drops the query', () => {
    expect(
      routeTemplate('/api/v1/burn-jobs/0190f5a2-7c1e-7b3d-9a4f-1c2d3e4f5a6b/retry?force=1#x'),
    ).toBe('/api/v1/burn-jobs/{id}/retry');
    expect(routeTemplate('/api/v1/artifacts')).toBe('/api/v1/artifacts');
  });
});

const PROBLEM = {
  type: 'about:blank',
  title: 'Internal server error',
  status: 500,
  code: 'INTERNAL',
  detail: 'the catalogue could not be read',
  instance: '/api/v1/artifacts/0190f5a2-7c1e-7b3d-9a4f-1c2d3e4f5a6b',
  request_id: '0190f5a2-7c1e-7b3d-9a4f-000000000001',
};

describe('errorReportRequest', () => {
  it('sends what the page showed, with the route in place of the path', () => {
    expect(errorReportRequest(PROBLEM, 'library')).toEqual({
      code: 'INTERNAL',
      title: 'Internal server error',
      detail: 'the catalogue could not be read',
      status: 500,
      request_id: '0190f5a2-7c1e-7b3d-9a4f-000000000001',
      endpoint: '/api/v1/artifacts/{id}',
      area: 'library',
    });
  });

  it('prefers the endpoint the page names, and leaves out a status of 0', () => {
    const request = errorReportRequest(
      { ...PROBLEM, status: 0, request_id: null },
      'library',
      'GET /api/v1/artifacts/{id}',
    );
    expect(request.endpoint).toBe('GET /api/v1/artifacts/{id}');
    expect(request.status).toBeNull();
    expect(request.request_id).toBeNull();
  });

  it('keeps a long detail within what the server accepts', () => {
    const request = errorReportRequest({ ...PROBLEM, detail: '\u{1F4BF}'.repeat(5000) }, 'other');
    expect(Array.from(request.detail)).toHaveLength(4000);
  });
});

describe('prepareErrorReport', () => {
  it('posts to the error report route', async () => {
    const fetcher = vi.fn(async () => ({
      ok: true,
      status: 200,
      json: async () => ({ template: 'bug_report.yml', fields: [], removed: {}, truncated: false }),
    })) as unknown as typeof fetch;
    const result = await prepareErrorReport(errorReportRequest(PROBLEM, 'library'), fetcher);
    expect(fetcher).toHaveBeenCalledWith(
      '/api/v1/error-reports',
      expect.objectContaining({ method: 'POST' }),
    );
    expect(result.kind).toBe('ok');
  });
});

describe('reportSettingsOnce', () => {
  afterEach(() => forgetReportSettings());

  const settings = { enabled: true, version: '0.1.0', build: '0.1.0, built locally' };

  it('asks once and shares the answer', async () => {
    const fetcher = vi.fn(async () => ({
      ok: true,
      status: 200,
      json: async () => settings,
    })) as unknown as typeof fetch;
    const [first, second] = await Promise.all([
      reportSettingsOnce(fetcher),
      reportSettingsOnce(fetcher),
    ]);
    expect(first).toEqual(settings);
    expect(second).toEqual(settings);
    expect(await reportSettingsOnce(fetcher)).toEqual(settings);
    expect(fetcher).toHaveBeenCalledTimes(1);
  });

  it('does not remember a failure', async () => {
    const failing = vi.fn(async () => {
      throw new Error('offline');
    }) as unknown as typeof fetch;
    expect(await reportSettingsOnce(failing)).toBeNull();
    await Promise.resolve();
    const working = vi.fn(async () => ({
      ok: true,
      status: 200,
      json: async () => settings,
    })) as unknown as typeof fetch;
    expect(await reportSettingsOnce(working)).toEqual(settings);
  });
});
