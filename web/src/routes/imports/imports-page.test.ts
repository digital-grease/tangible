// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { forgetReportSettings } from '$lib/report';

const { default: ImportsPage } = await import('./+page.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
  forgetReportSettings();
});

type Reply = { status: number; body: unknown };
type Asked = { url: string; method: string; body?: string };

function server(routes: Record<string, Reply>): Asked[] {
  const asked: Asked[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const url = String(input);
      const method = init?.method ?? 'GET';
      asked.push({ url, method, ...(typeof init?.body === 'string' ? { body: init.body } : {}) });
      const reply = routes[`${method} ${url}`] ?? routes[url] ?? { status: 404, body: {} };
      return {
        ok: reply.status >= 200 && reply.status < 300,
        status: reply.status,
        json: async () => reply.body,
        headers: new Headers(),
      } as Response;
    }),
  );
  return asked;
}

function sources(urlImports: boolean) {
  return {
    status: 200,
    body: {
      watch_roots: [],
      max_upload_bytes: 1024,
      url_imports: urlImports,
      url_imports_allow_http: false,
    },
  };
}

const SETTINGS = { status: 200, body: { enabled: false, version: '0.1.0', build: '0.1.0' } };

const DOWNLOADING = {
  id: 'i1',
  source_kind: 'http_url',
  source_filename: 'disc.iso',
  source_reference: 'https://example.org/files/disc.iso',
  state: 'acquiring',
  is_settled: false,
  bytes_expected: 4096,
  bytes_received: 1024,
  warnings: [],
  created_by: 'operator',
  created_at: '2026-10-24T12:00:00Z',
  updated_at: '2026-10-24T12:00:00Z',
};

describe('Imports page, importing from a URL', () => {
  it('says how to turn it on where it is off', async () => {
    server({
      '/api/v1/import-sources': sources(false),
      '/api/v1/imports': { status: 200, body: { items: [] } },
      '/api/v1/report-settings': SETTINGS,
    });
    render(ImportsPage);
    expect(await screen.findByText(/turned off on this server/)).toBeTruthy();
    expect(screen.queryByLabelText('Address of the file')).toBeNull();
  });

  it('sends the URL and a typed digest, and lists the download', async () => {
    const asked = server({
      '/api/v1/import-sources': sources(true),
      '/api/v1/imports': { status: 200, body: { items: [DOWNLOADING] } },
      'POST /api/v1/imports/url': { status: 202, body: DOWNLOADING },
      '/api/v1/report-settings': SETTINGS,
    });
    render(ImportsPage);
    const address = await screen.findByLabelText('Address of the file');
    await fireEvent.input(address, {
      target: { value: 'https://example.org/files/disc.iso?sig=abc' },
    });
    await fireEvent.input(screen.getByLabelText('SHA-256, if you know it'), {
      target: { value: 'AB'.repeat(32) },
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Fetch and import' }));
    await vi.waitFor(() => {
      expect(asked.some((call) => call.method === 'POST')).toBe(true);
    });
    const sent = asked.find((call) => call.method === 'POST');
    expect(sent?.url).toBe('/api/v1/imports/url');
    expect(JSON.parse(sent?.body ?? '{}')).toEqual({
      url: 'https://example.org/files/disc.iso?sig=abc',
      expected_sha256: 'ab'.repeat(32),
    });
    expect(await screen.findByText('Downloading')).toBeTruthy();
    expect(screen.getByText('https://example.org/files/disc.iso')).toBeTruthy();
    expect(screen.getByText('URL')).toBeTruthy();
  });

  it('shows why the server would not fetch an address', async () => {
    server({
      '/api/v1/import-sources': sources(true),
      '/api/v1/imports': { status: 200, body: { items: [] } },
      'POST /api/v1/imports/url': {
        status: 422,
        body: {
          type: 'about:blank',
          title: 'The server will not fetch from that address',
          status: 422,
          code: 'IMPORT_SSRF_BLOCKED',
          detail: '10.0.0.5 is a private network address',
        },
      },
      '/api/v1/report-settings': SETTINGS,
    });
    render(ImportsPage);
    const address = await screen.findByLabelText('Address of the file');
    await fireEvent.input(address, { target: { value: 'https://10.0.0.5/disc.iso' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Fetch and import' }));
    expect(await screen.findByText(/private network address/)).toBeTruthy();
  });
});
