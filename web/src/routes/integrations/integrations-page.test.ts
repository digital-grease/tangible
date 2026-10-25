// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen, within } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { IntegrationView } from '$lib/integrations';

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
  it('lists integrations in words, and leaves the RomM export alone', async () => {
    server({
      'GET /api/v1/integrations': list([nas(), romm], false),
      'GET /api/v1/import-sources': sources,
    });
    render(IntegrationsPage);
    const row = (await screen.findByRole('rowheader', { name: 'NAS' })).closest('tr')!;
    expect(within(row).getByText('qBittorrent')).toBeTruthy();
    expect(within(row).getByText('Not tried yet')).toBeTruthy();
    expect(within(row).getByText('Password set')).toBeTruthy();
    expect(within(row).getByRole('button', { name: 'Edit NAS' })).toBeTruthy();
    const managed = screen.getByRole('rowheader', { name: 'romm-filesystem' }).closest('tr')!;
    expect(within(managed).queryByRole('button')).toBeNull();
    expect(within(managed).getByText('TANGIBLE_ROMM_EXPORT_ROOT')).toBeTruthy();
    expect(screen.getByRole('note').textContent).toContain('TANGIBLE_SECRET_KEY_FILE');
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
    const alert = await screen.findByText(/save this without a password/);
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
    render(IntegrationsPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Remove NAS' }));
    expect(asked.some((a) => a.method === 'DELETE')).toBe(false);
    expect(screen.getByText(/Nothing in the client or the library is touched/)).toBeTruthy();
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
        body: { ok: true, api_version: '2.11.4' },
      },
    });
    render(IntegrationsPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Test NAS' }));
    expect(await screen.findByText(/NAS signed in; WebUI API 2.11.4/)).toBeTruthy();
    expect(asked.some((a) => a.method === 'POST' && a.url.endsWith('/test'))).toBe(true);
    expect(await screen.findByText('WebUI API 2.11.4')).toBeTruthy();
    expect(screen.getByText('Working')).toBeTruthy();
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
