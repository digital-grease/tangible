// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen, within } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { IntegrationView } from '$lib/integrations';
import { axeViolations } from '$lib/testing/axe';

const { default: IntegrationsPage } = await import('./+page.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
});

type Reply = { status: number; body?: unknown };
type Asked = { method: string; url: string; body: unknown };

/** A fake server answering `METHOD url`; a list of replies is used in turn. */
function server(routes: Record<string, Reply | Reply[]>): Asked[] {
  const asked: Asked[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const method = init?.method ?? 'GET';
      const url = String(input);
      asked.push({
        method,
        url,
        body: typeof init?.body === 'string' ? JSON.parse(init.body) : undefined,
      });
      const route = routes[`${method} ${url}`];
      const reply = (Array.isArray(route) ? route.shift() : route) ?? { status: 404, body: {} };
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

const nas = (overrides: Partial<IntegrationView> = {}): IntegrationView => ({
  id: 'i1',
  kind: 'qbittorrent',
  name: 'NAS',
  enabled: true,
  settings: {
    base_url: 'http://192.168.1.13:8080/',
    username: 'tangible',
    category: 'tangible',
    client_path: '/downloads/tangible',
    local_root: 'downloads',
  },
  secrets: [{ field: 'password', set: true }],
  status: 'unconfigured',
  last_success_at: null,
  last_error_code: null,
  managed: false,
  ...overrides,
});

const romm: IntegrationView = {
  ...nas(),
  id: 'r1',
  kind: 'romm',
  name: 'romm-filesystem',
  settings: {},
  secrets: [],
  managed: true,
};

const sources = {
  status: 200,
  body: {
    max_upload_bytes: 1,
    url_imports: false,
    url_imports_allow_http: false,
    watch_roots: ['downloads'],
  },
};

const list = (items: IntegrationView[], keyLoaded = true): Reply => ({
  status: 200,
  body: { items, secret_key_loaded: keyLoaded },
});

function problem(status: number, code: string, detail: string): Reply {
  return { status, body: { type: 'about:blank', title: 'Refused', status, code, detail } };
}

async function fill(label: string | RegExp, value: string) {
  await fireEvent.input(screen.getByLabelText(label), { target: { value } });
}

describe('Integrations page', () => {
  it('removes a torrent after import only when asked, saying what goes and when', async () => {
    const asked = server({
      'GET /api/v1/integrations': [list([]), list([nas()])],
      'GET /api/v1/import-sources': sources,
      'POST /api/v1/integrations': { status: 201, body: nas() },
    });
    render(IntegrationsPage);
    await screen.findByText(/No integrations yet/);
    const group = screen.getByRole('group', { name: 'After an import is in the library' });
    const keep = within(group).getByLabelText(
      /Keep the torrent in qBittorrent/,
    ) as HTMLInputElement;
    const remove = within(group).getByLabelText(
      'Remove the torrent and its files from qBittorrent',
    ) as HTMLInputElement;
    expect(keep.checked).toBe(true);
    expect(remove.disabled).toBe(false);
    expect(group.textContent).toContain('integrity check reads the library');
    await fill('Name', 'NAS');
    await fill('WebUI address', 'http://192.168.1.13:8080');
    await fill('WebUI user', 'tangible');
    await fill('Download folder, as qBittorrent sees it', '/downloads/tangible');
    await fireEvent.click(remove);
    await fireEvent.click(screen.getByRole('button', { name: 'Add qBittorrent' }));
    expect(await screen.findByText('Added NAS.')).toBeTruthy();
    const post = asked.find((a) => a.method === 'POST');
    expect((post?.body as { settings: Record<string, unknown> }).settings.after_import).toBe(
      'remove',
    );
  });

  it('offers deleting Usenet downloads only in a root Tangible may delete in', async () => {
    server({
      'GET /api/v1/integrations': list([]),
      'GET /api/v1/import-sources': {
        status: 200,
        body: {
          ...sources.body,
          watch_roots: ['downloads', 'usenet'],
          removable_roots: ['usenet'],
        },
      },
    });
    render(IntegrationsPage);
    await screen.findByText(/No integrations yet/);
    await fireEvent.change(screen.getByLabelText('Kind'), { target: { value: 'sabnzbd' } });
    const deleting = () =>
      screen.getByLabelText(
        "Delete the downloaded files, and remove the job from SABnzbd's history",
      ) as HTMLInputElement;
    expect(deleting().disabled).toBe(true);
    expect(screen.getByText('Choose the watched root first.')).toBeTruthy();
    await fireEvent.change(screen.getByLabelText('The same folder, as mounted here'), {
      target: { value: 'downloads' },
    });
    expect(deleting().disabled).toBe(true);
    expect(screen.getByText(/downloads is not one Tangible may delete in/)).toBeTruthy();
    await fireEvent.change(screen.getByLabelText('The same folder, as mounted here'), {
      target: { value: 'usenet' },
    });
    expect(deleting().disabled).toBe(false);
    await fireEvent.click(deleting());
    expect(deleting().checked).toBe(true);
    // Back to a root it may not delete in: the choice goes back to keeping.
    await fireEvent.change(screen.getByLabelText('The same folder, as mounted here'), {
      target: { value: 'downloads' },
    });
    expect(deleting().checked).toBe(false);
  });

  it('keeps a stored removal when an integration is edited', async () => {
    const asked = server({
      'GET /api/v1/integrations': [
        list([nas({ settings: { ...nas().settings, after_import: 'remove' } })]),
        list([nas()]),
      ],
      'GET /api/v1/import-sources': sources,
      'PATCH /api/v1/integrations/i1': { status: 200, body: nas() },
    });
    render(IntegrationsPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Edit NAS' }));
    expect(
      (
        screen.getByLabelText(
          'Remove the torrent and its files from qBittorrent',
        ) as HTMLInputElement
      ).checked,
    ).toBe(true);
    await fireEvent.click(screen.getByRole('button', { name: 'Save changes' }));
    expect(await screen.findByText('Saved NAS.')).toBeTruthy();
    const patch = asked.find((a) => a.method === 'PATCH');
    expect((patch?.body as { settings: Record<string, unknown> }).settings.after_import).toBe(
      'remove',
    );
  });

  it('lists integrations in words, and leaves the RomM export alone', async () => {
    server({
      'GET /api/v1/integrations': list([nas(), romm], false),
      'GET /api/v1/import-sources': sources,
    });
    const { container } = render(IntegrationsPage);
    const row = (await screen.findByRole('rowheader', { name: 'NAS' })).closest('tr')!;
    expect(within(row).getByText('qBittorrent')).toBeTruthy();
    expect(within(row).getByText('Not tried yet')).toBeTruthy();
    expect(within(row).getByText('Password set')).toBeTruthy();
    expect(within(row).getByRole('button', { name: 'Edit NAS' })).toBeTruthy();
    const managed = screen.getByRole('rowheader', { name: 'romm-filesystem' }).closest('tr')!;
    expect(within(managed).queryByRole('button')).toBeNull();
    expect(within(managed).getByText('TANGIBLE_ROMM_EXPORT_ROOT')).toBeTruthy();
    expect(screen.getByRole('note').textContent).toContain('TANGIBLE_SECRET_KEY_FILE');
    expect(await axeViolations(container)).toEqual([]);
  });

  it('adds a qBittorrent and keeps no password in the page', async () => {
    const asked = server({
      'GET /api/v1/integrations': [list([]), list([nas()])],
      'GET /api/v1/import-sources': sources,
      'POST /api/v1/integrations': { status: 201, body: nas() },
    });
    render(IntegrationsPage);
    await screen.findByText(/No integrations yet/);
    await fill('Name', 'NAS');
    await fill('WebUI address', 'http://192.168.1.13:8080');
    await fill('WebUI user', 'tangible');
    await fill('WebUI password', 'hunter2');
    await fill('Download folder, as qBittorrent sees it', '/downloads/tangible');
    await fireEvent.submit(
      screen.getByRole('button', { name: 'Add qBittorrent' }).closest('form')!,
    );

    expect(await screen.findByText('Added NAS.')).toBeTruthy();
    const post = asked.find((a) => a.method === 'POST');
    expect(post?.body).toEqual({
      kind: 'qbittorrent',
      name: 'NAS',
      enabled: true,
      settings: {
        base_url: 'http://192.168.1.13:8080',
        username: 'tangible',
        category: 'tangible',
        client_path: '/downloads/tangible',
        local_root: 'downloads',
        after_import: 'keep',
      },
      secrets: { password: 'hunter2' },
    });
    expect((screen.getByLabelText('WebUI password') as HTMLInputElement).value).toBe('');
    expect(await screen.findByRole('rowheader', { name: 'NAS' })).toBeTruthy();
  });

  it('edits without sending the password unless one is typed or removed', async () => {
    const asked = server({
      'GET /api/v1/integrations': [list([nas()]), list([nas()]), list([nas()])],
      'GET /api/v1/import-sources': sources,
      'PATCH /api/v1/integrations/i1': [
        { status: 200, body: nas() },
        { status: 200, body: nas({ secrets: [{ field: 'password', set: false }] }) },
      ],
    });
    render(IntegrationsPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Edit NAS' }));
    expect((screen.getByLabelText('WebUI password') as HTMLInputElement).value).toBe('');
    expect((screen.getByLabelText('WebUI address') as HTMLInputElement).value).toBe(
      'http://192.168.1.13:8080/',
    );
    await fill('WebUI user', 'other');
    await fireEvent.click(screen.getByRole('button', { name: 'Save changes' }));
    expect(await screen.findByText('Saved NAS.')).toBeTruthy();
    const first = asked.filter((a) => a.method === 'PATCH')[0];
    expect(first?.body).toMatchObject({ name: 'NAS', settings: { username: 'other' } });
    expect(first?.body).not.toHaveProperty('secrets');

    await fireEvent.click(screen.getByRole('button', { name: 'Edit NAS' }));
    await fireEvent.click(screen.getByLabelText('Remove the stored password'));
    await fireEvent.click(screen.getByRole('button', { name: 'Save changes' }));
    await screen.findByText('Saved NAS.');
    const second = asked.filter((a) => a.method === 'PATCH')[1];
    expect(second?.body).toMatchObject({ secrets: { password: null } });
  });

  it('says what to do when the server holds no key', async () => {
    server({
      'GET /api/v1/integrations': list([], false),
      'GET /api/v1/import-sources': sources,
      'POST /api/v1/integrations': problem(409, 'SECRET_KEY_MISSING', 'no key'),
    });
    render(IntegrationsPage);
    await screen.findByText(/No integrations yet/);
    await fill('Name', 'NAS');
    await fill('WebUI address', 'http://nas:8080');
    await fill('WebUI user', 'tangible');
    await fill('WebUI password', 'hunter2');
    await fill('Download folder, as qBittorrent sees it', '/downloads');
    await fireEvent.click(screen.getByRole('button', { name: 'Add qBittorrent' }));
    const alert = await screen.findByText(/save this without one/);
    expect(alert.textContent).toContain('TANGIBLE_SECRET_KEY_FILE');
    expect((screen.getByLabelText('WebUI password') as HTMLInputElement).value).toBe('');
    expect((screen.getByLabelText('Name') as HTMLInputElement).value).toBe('NAS');
  });

  it('removes only after a second, deliberate click', async () => {
    const asked = server({
      'GET /api/v1/integrations': [list([nas()]), list([])],
      'GET /api/v1/import-sources': sources,
      'DELETE /api/v1/integrations/i1': { status: 204 },
    });
    const { container } = render(IntegrationsPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Remove NAS' }));
    expect(asked.some((a) => a.method === 'DELETE')).toBe(false);
    expect(screen.getByText(/Nothing in the client or the library is touched/)).toBeTruthy();
    // The keyboard is taken to the question.
    await vi.waitFor(() =>
      expect(document.activeElement).toBe(screen.getByRole('heading', { name: 'Remove NAS?' })),
    );
    expect(await axeViolations(container)).toEqual([]);
    await fireEvent.click(screen.getByRole('button', { name: 'Yes, remove NAS' }));
    expect(await screen.findByText('Removed NAS.')).toBeTruthy();
    expect(asked.some((a) => a.method === 'DELETE')).toBe(true);
    expect(await screen.findByText(/No integrations yet/)).toBeTruthy();
  });

  it('tries a client and says how it went', async () => {
    const asked = server({
      'GET /api/v1/integrations': [
        list([nas()]),
        list([nas({ status: 'ok', client_version: '2.11.4' })]),
      ],
      'GET /api/v1/import-sources': sources,
      'POST /api/v1/integrations/i1/test': {
        status: 200,
        body: { ok: true, api_version: '2.11.4', checks: [] },
      },
    });
    render(IntegrationsPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Test NAS' }));
    expect(await screen.findByText(/NAS signed in; WebUI API 2.11.4/)).toBeTruthy();
    expect(asked.some((a) => a.method === 'POST' && a.url.endsWith('/test'))).toBe(true);
    expect(await screen.findByText('WebUI API 2.11.4')).toBeTruthy();
    expect(screen.getByText('Working')).toBeTruthy();
  });

  it('adds a SABnzbd, which takes an API key and no user', async () => {
    const asked = server({
      'GET /api/v1/integrations': [list([]), list([])],
      'GET /api/v1/import-sources': sources,
      'POST /api/v1/integrations': { status: 201, body: nas({ kind: 'sabnzbd', name: 'SAB' }) },
    });
    render(IntegrationsPage);
    await screen.findByText(/No integrations yet/);
    await fireEvent.change(screen.getByLabelText('Kind'), { target: { value: 'sabnzbd' } });
    expect(screen.queryByLabelText('WebUI user')).toBeNull();
    expect(screen.getByText(/It must exist in SABnzbd first/)).toBeTruthy();
    await fill('Name', 'SAB');
    await fill('SABnzbd address', 'http://192.168.1.13:8080/sabnzbd');
    await fill('API key', 'the-key');
    await fill('Download folder, as SABnzbd sees it', '/downloads/complete/tangible');
    await fireEvent.click(screen.getByRole('button', { name: 'Add SABnzbd' }));
    expect(await screen.findByText('Added SAB.')).toBeTruthy();
    const post = asked.find((a) => a.method === 'POST');
    expect(post?.body).toEqual({
      kind: 'sabnzbd',
      name: 'SAB',
      enabled: true,
      settings: {
        base_url: 'http://192.168.1.13:8080/sabnzbd',
        category: 'tangible',
        client_path: '/downloads/complete/tangible',
        local_root: 'downloads',
        after_import: 'keep',
      },
      secrets: { api_key: 'the-key' },
    });
    expect((screen.getByLabelText('API key') as HTMLInputElement).value).toBe('');
  });

  it('adds S3-compatible storage with its buckets and both keys', async () => {
    const asked = server({
      'GET /api/v1/integrations': [list([]), list([])],
      'GET /api/v1/import-sources': sources,
      'POST /api/v1/integrations': { status: 201, body: nas({ kind: 's3', name: 'Archive' }) },
    });
    render(IntegrationsPage);
    await screen.findByText(/No integrations yet/);
    await fireEvent.change(screen.getByLabelText('Kind'), { target: { value: 's3' } });
    expect(screen.queryByLabelText('Category')).toBeNull();
    expect(screen.queryByLabelText(/Download folder/)).toBeNull();
    await fill('Name', 'Archive');
    await fill('Endpoint', 'http://192.168.1.13:8333');
    await fill('Access key id', 'AKID');
    await fill('Secret access key', 'shh');
    await fill('Buckets operators may import from', 'discs\nold-discs');
    await fireEvent.click(screen.getByRole('button', { name: 'Add S3-compatible storage' }));
    expect(await screen.findByText('Added Archive.')).toBeTruthy();
    const post = asked.find((a) => a.method === 'POST');
    expect(post?.body).toEqual({
      kind: 's3',
      name: 'Archive',
      enabled: true,
      settings: {
        endpoint: 'http://192.168.1.13:8333',
        region: 'us-east-1',
        path_style: true,
        buckets: ['discs', 'old-discs'],
        after_import: 'keep',
      },
      secrets: { access_key_id: 'AKID', secret_access_key: 'shh' },
    });
    expect((screen.getByLabelText('Secret access key') as HTMLInputElement).value).toBe('');
    expect((screen.getByLabelText('Access key id') as HTMLInputElement).value).toBe('');
  });

  it("adds RomM's API with a token, its browser address and the scan switch", async () => {
    const asked = server({
      'GET /api/v1/integrations': [list([]), list([])],
      'GET /api/v1/import-sources': sources,
      'POST /api/v1/integrations': { status: 201, body: nas({ kind: 'romm_api', name: 'RomM' }) },
    });
    render(IntegrationsPage);
    await screen.findByText(/No integrations yet/);
    await fireEvent.change(screen.getByLabelText('Kind'), { target: { value: 'romm_api' } });
    expect(screen.getByText(/It never changes anything in RomM/)).toBeTruthy();
    expect(screen.queryByLabelText('Category')).toBeNull();
    expect(screen.queryByLabelText(/Download folder/)).toBeNull();
    expect(screen.getByText('tasks.run')).toBeTruthy();
    await fill('Name', 'RomM');
    await fill('RomM address, as this server reaches it', 'http://romm:8080/');
    await fill('RomM address, as your browser reaches it', 'https://romm.example.net/');
    await fill('Client token', 'rmm_secret');
    await fireEvent.click(screen.getByLabelText('Ask RomM to scan what the export changes'));
    await fireEvent.click(screen.getByRole('button', { name: "Add RomM's API" }));
    expect(await screen.findByText('Added RomM.')).toBeTruthy();
    const post = asked.find((a) => a.method === 'POST');
    expect(post?.body).toEqual({
      kind: 'romm_api',
      name: 'RomM',
      enabled: true,
      settings: {
        base_url: 'http://romm:8080/',
        web_url: 'https://romm.example.net/',
        scan_after_export: false,
      },
      secrets: { token: 'rmm_secret' },
    });
    expect((screen.getByLabelText('Client token') as HTMLInputElement).value).toBe('');
  });

  it('adds a Prowlarr with its address and API key, and nothing else', async () => {
    const prowlarr = nas({
      id: 'p1',
      kind: 'prowlarr',
      name: 'Prowlarr',
      settings: { base_url: 'http://192.168.1.13:9696/' },
      secrets: [{ field: 'api_key', set: true }],
    });
    const asked = server({
      'GET /api/v1/integrations': [list([]), list([prowlarr]), list([prowlarr])],
      'GET /api/v1/import-sources': sources,
      'POST /api/v1/integrations': { status: 201, body: prowlarr },
      'POST /api/v1/integrations/p1/test': {
        status: 200,
        body: { ok: true, api_version: '2.7.0.5723', checks: [] },
      },
    });
    const { container } = render(IntegrationsPage);
    await screen.findByText(/No integrations yet/);
    await fireEvent.change(screen.getByLabelText('Kind'), { target: { value: 'prowlarr' } });
    expect(screen.getByText(/It never changes anything in Prowlarr/)).toBeTruthy();
    expect(screen.getByText(/adds or changes none/)).toBeTruthy();
    expect(screen.queryByLabelText('Category')).toBeNull();
    expect(screen.queryByRole('group', { name: /After an import/ })).toBeNull();
    expect(await axeViolations(container)).toEqual([]);
    await fill('Name', 'Prowlarr');
    await fill('Prowlarr address', 'http://192.168.1.13:9696/');
    await fill('API key', 'the-key');
    await fireEvent.click(screen.getByRole('button', { name: 'Add Prowlarr' }));
    expect(await screen.findByText('Added Prowlarr.')).toBeTruthy();
    expect(asked.find((a) => a.method === 'POST')?.body).toEqual({
      kind: 'prowlarr',
      name: 'Prowlarr',
      enabled: true,
      settings: { base_url: 'http://192.168.1.13:9696/' },
      secrets: { api_key: 'the-key' },
    });
    expect((screen.getByLabelText('API key') as HTMLInputElement).value).toBe('');

    await fireEvent.click(await screen.findByRole('button', { name: 'Test Prowlarr' }));
    expect(
      await screen.findByText(/answered as Prowlarr 2.7.0.5723; its indexers can be searched/),
    ).toBeTruthy();
  });

  it("tries RomM's API and lists what it checked, in words", async () => {
    const rommApi = nas({
      id: 'a1',
      kind: 'romm_api',
      name: 'RomM',
      settings: { base_url: 'http://romm:8080/', scan_after_export: true },
      secrets: [{ field: 'token', set: true }],
    });
    server({
      'GET /api/v1/integrations': [list([rommApi]), list([rommApi])],
      'GET /api/v1/import-sources': sources,
      'POST /api/v1/integrations/a1/test': {
        status: 200,
        body: {
          ok: true,
          api_version: '5.4.1',
          checks: [
            { name: 'Finding exported games', ok: true, detail: 'platforms and ROMs can be read' },
            {
              name: 'Export marker',
              ok: false,
              detail: "RomM counts .tangible-export.json as one of each game's files",
            },
          ],
        },
      },
    });
    render(IntegrationsPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Test RomM' }));
    expect(
      await screen.findByText(/RomM answered as RomM 5.4.1; exported games can be found/),
    ).toBeTruthy();
    const checks = screen.getByRole('list', { name: 'What was checked' });
    expect(within(checks).getByText('Finding exported games: yes.')).toBeTruthy();
    expect(within(checks).getByText('Export marker: no.')).toBeTruthy();
    expect(within(checks).getByText(/counts .tangible-export.json/)).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Edit RomM' })).toBeTruthy();
  });

  it('edits and tests an NZBGet like any client', async () => {
    const nzbget = nas({
      id: 'n1',
      kind: 'nzbget',
      name: 'Usenet',
      settings: {
        base_url: 'http://192.168.1.13:6789/',
        username: 'nzbget',
        category: 'tangible',
        client_path: '/downloads/completed/tangible',
        local_root: 'downloads',
      },
      client_version: '25.3',
    });
    server({
      'GET /api/v1/integrations': [list([nzbget]), list([nzbget])],
      'GET /api/v1/import-sources': sources,
      'POST /api/v1/integrations/n1/test': {
        status: 200,
        body: { ok: true, api_version: '25.3', checks: [] },
      },
    });
    render(IntegrationsPage);
    expect(await screen.findByText('version 25.3')).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: 'Test Usenet' }));
    expect(await screen.findByText(/Usenet signed in; version 25.3/)).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: 'Edit Usenet' }));
    expect(screen.queryByLabelText('Kind')).toBeNull();
    expect((screen.getByLabelText('Control user') as HTMLInputElement).value).toBe('nzbget');
    expect((screen.getByLabelText('Control password') as HTMLInputElement).value).toBe('');
    expect(screen.getByLabelText('Remove the stored password')).toBeTruthy();
  });

  it('tells anyone else it is for administrators', async () => {
    server({
      'GET /api/v1/integrations': problem(403, 'FORBIDDEN', 'administrators only'),
      'GET /api/v1/import-sources': sources,
    });
    render(IntegrationsPage);
    expect(await screen.findByText(/Only an administrator/)).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Add qBittorrent' })).toBeNull();
  });
});
