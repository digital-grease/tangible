// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { forgetReportSettings } from '$lib/report';
import { TOPICS } from '$lib/events';
import { axeViolations } from '$lib/testing/axe';

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

function sources(
  urlImports: boolean,
  downloadClients: { id: string; name: string; kind: string }[] = [],
  s3Sources: { id: string; name: string; buckets: string[] }[] = [],
) {
  return {
    status: 200,
    body: {
      watch_roots: [],
      max_upload_bytes: 1024,
      url_imports: urlImports,
      url_imports_allow_http: false,
      download_clients: downloadClients,
      s3_sources: s3Sources,
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

const NAS = { id: 'q1', name: 'NAS', kind: 'qbittorrent' };
const HASH = '23760839656b18c262361c3019426c1e717b7310';

function torrentImport(overrides: Record<string, unknown> = {}) {
  return {
    ...DOWNLOADING,
    id: 't1',
    source_kind: 'qbittorrent_job',
    source_filename: null,
    source_reference: `qbittorrent:${HASH}`,
    bytes_expected: 300000,
    bytes_received: 150000,
    ...overrides,
  };
}

describe('Imports page, sending a torrent to a download client', () => {
  it('points to the Integrations page where there is no client', async () => {
    server({
      '/api/v1/import-sources': sources(false),
      '/api/v1/imports': { status: 200, body: { items: [] } },
      '/api/v1/report-settings': SETTINGS,
    });
    render(ImportsPage);
    expect(await screen.findByText(/No download client is set up/)).toBeTruthy();
    for (const link of screen.getAllByRole('link', { name: 'Integrations' })) {
      expect(link.getAttribute('href')).toBe('/integrations');
    }
    expect(screen.queryByLabelText('or a magnet link')).toBeNull();
  });

  it('hands a magnet link over and says what happens next', async () => {
    const asked = server({
      '/api/v1/import-sources': sources(false, [NAS]),
      '/api/v1/imports': { status: 200, body: { items: [torrentImport()] } },
      'POST /api/v1/integrations/q1/jobs': {
        status: 202,
        body: {
          integration_id: 'q1',
          identity: HASH,
          client_job_id: HASH,
          import_id: 't1',
          outcome: 'created',
          artifact_id: null,
        },
      },
      '/api/v1/report-settings': SETTINGS,
    });
    render(ImportsPage);
    const magnet = await screen.findByLabelText('or a magnet link');
    await fireEvent.input(magnet, { target: { value: `  magnet:?xt=urn:btih:${HASH}  ` } });
    await fireEvent.click(screen.getByRole('button', { name: 'Send to NAS' }));
    expect(await screen.findByText(/Handed to NAS/)).toBeTruthy();
    const sent = asked.find((call) => call.method === 'POST');
    expect(sent?.url).toBe('/api/v1/integrations/q1/jobs');
    expect(JSON.parse(sent?.body ?? '{}')).toEqual({ magnet: `magnet:?xt=urn:btih:${HASH}` });
    expect(await screen.findByText('Downloading in qBittorrent')).toBeTruthy();
    expect(screen.getByText('qBittorrent')).toBeTruthy();
    expect(screen.getByText(`qbittorrent:${HASH}`)).toBeTruthy();
  });

  it('sends a chosen .torrent file as base64', async () => {
    const asked = server({
      '/api/v1/import-sources': sources(false, [NAS]),
      '/api/v1/imports': { status: 200, body: { items: [] } },
      'POST /api/v1/integrations/q1/jobs': {
        status: 200,
        body: {
          integration_id: 'q1',
          identity: HASH,
          client_job_id: HASH,
          import_id: 't1',
          outcome: 'already_imported',
          artifact_id: 'a1',
        },
      },
      '/api/v1/report-settings': SETTINGS,
    });
    render(ImportsPage);
    const input = await screen.findByLabelText('.torrent file');
    const file = new File([new Uint8Array([100, 101, 0, 255])], 'x.torrent');
    await fireEvent.change(input, { target: { files: [file] } });
    expect((screen.getByLabelText('or a magnet link') as HTMLInputElement).disabled).toBe(true);
    await fireEvent.click(screen.getByRole('button', { name: 'Send to NAS' }));
    expect(await screen.findByText(/already in the library/)).toBeTruthy();
    const sent = asked.find((call) => call.method === 'POST');
    expect(JSON.parse(sent?.body ?? '{}')).toEqual({ torrent: 'ZGUA/w==' });
  });

  it('sends an .nzb to a Usenet client and lists its import in words', async () => {
    const sab = { id: 's1', name: 'SAB', kind: 'sabnzbd' };
    const asked = server({
      '/api/v1/import-sources': sources(false, [NAS, sab]),
      '/api/v1/imports': {
        status: 200,
        body: {
          items: [
            torrentImport({
              id: 'n1',
              source_kind: 'sabnzbd_job',
              source_reference: 'sabnzbd:SABnzbd_nzo_x',
            }),
          ],
        },
      },
      'POST /api/v1/integrations/s1/jobs': {
        status: 202,
        body: {
          integration_id: 's1',
          identity: 'a'.repeat(64),
          client_job_id: 'SABnzbd_nzo_x',
          import_id: 'n1',
          outcome: 'created',
          artifact_id: null,
        },
      },
      '/api/v1/report-settings': SETTINGS,
    });
    render(ImportsPage);
    await fireEvent.change(await screen.findByLabelText('Client'), { target: { value: 's1' } });
    expect(screen.queryByLabelText('or a magnet link')).toBeNull();
    const nzb = new File([new Uint8Array([60, 110, 122, 98])], 'x.nzb');
    await fireEvent.change(screen.getByLabelText('.nzb file'), { target: { files: [nzb] } });
    await fireEvent.click(screen.getByRole('button', { name: 'Send to SAB' }));
    expect(await screen.findByText(/Handed to SAB/)).toBeTruthy();
    const sent = asked.find((call) => call.method === 'POST');
    expect(sent?.url).toBe('/api/v1/integrations/s1/jobs');
    expect(JSON.parse(sent?.body ?? '{}')).toEqual({ nzb: 'PG56Yg==' });
    expect(await screen.findByText('Downloading in SABnzbd')).toBeTruthy();
    expect(screen.getByText('sabnzbd:SABnzbd_nzo_x')).toBeTruthy();
  });

  it('says an import is waiting when the client cannot be asked', async () => {
    server({
      '/api/v1/import-sources': sources(false, [NAS]),
      '/api/v1/imports': {
        status: 200,
        body: {
          items: [
            torrentImport({
              error_code: 'INTEGRATION_UNREACHABLE',
              error_detail: 'qBittorrent could not be reached: connection refused',
            }),
          ],
        },
      },
      '/api/v1/report-settings': SETTINGS,
    });
    render(ImportsPage);
    expect(await screen.findByText('Waiting for qBittorrent')).toBeTruthy();
    expect(screen.getByText(/could not be reached/)).toBeTruthy();
  });
});

describe('Imports page, importing from S3', () => {
  it('says how to set it up where there is none', async () => {
    server({
      '/api/v1/import-sources': sources(false),
      '/api/v1/imports': { status: 200, body: { items: [] } },
      '/api/v1/report-settings': SETTINGS,
    });
    render(ImportsPage);
    expect(await screen.findByText(/No S3-compatible storage is set up/)).toBeTruthy();
    expect(screen.queryByLabelText('Object key')).toBeNull();
  });

  it('asks for an object in an allowed bucket, and lists the import', async () => {
    const asked = server({
      '/api/v1/import-sources': sources(
        false,
        [],
        [
          { id: 's1', name: 'Archive', buckets: ['discs', 'old-discs'] },
          { id: 's2', name: 'Cloud', buckets: ['cloud-discs'] },
        ],
      ),
      '/api/v1/imports': {
        status: 200,
        body: {
          items: [
            {
              ...DOWNLOADING,
              id: 'o1',
              source_kind: 's3_object',
              source_filename: 'Disc.iso',
              source_reference: 's3://cloud-discs/a/Disc.iso',
            },
          ],
        },
      },
      'POST /api/v1/imports/s3': { status: 202, body: DOWNLOADING },
      '/api/v1/report-settings': SETTINGS,
    });
    render(ImportsPage);
    // Choosing another storage offers its buckets, starting from its first.
    await fireEvent.change(await screen.findByLabelText('Storage'), { target: { value: 's2' } });
    expect((screen.getByLabelText('Bucket') as HTMLSelectElement).value).toBe('cloud-discs');
    await fireEvent.input(screen.getByLabelText('Object key'), {
      target: { value: ' a/Disc.iso ' },
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Import from S3' }));
    await vi.waitFor(() => {
      expect(asked.some((call) => call.method === 'POST')).toBe(true);
    });
    const sent = asked.find((call) => call.method === 'POST');
    expect(sent?.url).toBe('/api/v1/imports/s3');
    expect(JSON.parse(sent?.body ?? '{}')).toEqual({
      integration_id: 's2',
      bucket: 'cloud-discs',
      key: 'a/Disc.iso',
    });
    expect(await screen.findByText('s3://cloud-discs/a/Disc.iso')).toBeTruthy();
    expect(screen.getByText('S3')).toBeTruthy();
  });
});

describe('Imports page, removing sources', () => {
  it('says where removing an import from its client stands, in words', async () => {
    const imported = {
      ...DOWNLOADING,
      id: 'i2',
      source_kind: 'sabnzbd_job',
      source_reference: 'sabnzbd:SABnzbd_nzo_x',
      state: 'complete',
      is_settled: true,
      artifact_id: 'a1',
      source_removal: {
        state: 'kept',
        detail: 'SABnzbd has moved the download since it was imported, so it was left',
        files_removed: null,
        bytes_removed: null,
        files_kept: null,
        next_attempt_at: null,
        requested_at: '2026-10-27T20:00:00Z',
        ended_at: '2026-10-27T20:01:00Z',
      },
    };
    server({
      '/api/v1/import-sources': sources(false),
      '/api/v1/imports': { status: 200, body: { items: [imported] } },
      '/api/v1/report-settings': SETTINGS,
    });
    render(ImportsPage);
    expect(await screen.findByText('Left in SABnzbd.')).toBeTruthy();
    expect(screen.getByText(/has moved the download since it was imported/)).toBeTruthy();
  });
});

describe('Imports page, live', () => {
  it('refreshes when the event stream says an import changed', async () => {
    const opened: string[] = [];
    const listeners = new Map<string, ((event: MessageEvent) => void)[]>();
    class StandInSource {
      constructor(url: string) {
        opened.push(url);
      }
      addEventListener(type: string, listener: (event: MessageEvent) => void) {
        listeners.set(type, [...(listeners.get(type) ?? []), listener]);
      }
      close() {}
    }
    vi.stubGlobal('EventSource', StandInSource);
    const asked = server({
      '/api/v1/import-sources': sources(false),
      '/api/v1/imports': { status: 200, body: { items: [DOWNLOADING] } },
      '/api/v1/report-settings': SETTINGS,
    });
    render(ImportsPage);
    expect(await screen.findByText('disc.iso')).toBeTruthy();
    // The tab's one stream, for every topic (docs/plans/ui-overhaul.md, decision 6).
    await vi.waitFor(() => expect(opened).toEqual([`/api/v1/events?topics=${TOPICS.join(',')}`]));
    const before = asked.filter((request) => request.url === '/api/v1/imports').length;

    for (const listener of listeners.get('import.changed') ?? []) {
      listener({
        data: JSON.stringify({ topic: 'imports', id: 'i1', state: 'complete' }),
      } as MessageEvent);
    }
    await vi.waitFor(() =>
      expect(asked.filter((request) => request.url === '/api/v1/imports').length).toBe(before + 1),
    );
  });
});

describe('Imports page, for assistive technology', () => {
  it('has nothing axe objects to, with a download under way', async () => {
    server({
      '/api/v1/import-sources': sources(true),
      '/api/v1/imports': { status: 200, body: { items: [DOWNLOADING] } },
      '/api/v1/report-settings': SETTINGS,
    });
    const { container } = render(ImportsPage);
    expect(await screen.findByText('disc.iso')).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);
  });
});
