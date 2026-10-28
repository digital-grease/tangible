// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import { describe, expect, it } from 'vitest';
import {
  afterImportWording,
  clientFormFrom,
  clientWording,
  createClientRequest,
  deleteIntegration,
  emptyClientForm,
  isClientKind,
  parseBuckets,
  removalBlocked,
  removesAfterImport,
  integrationRefusal,
  integrationStatusLabel,
  kindLabel,
  secretsLabel,
  testIntegration,
  testOutcome,
  updateClientRequest,
  versionLabel,
  type IntegrationView,
} from './integrations';
import type { Problem } from './session';

const qbittorrent = (overrides: Partial<IntegrationView> = {}): IntegrationView => ({
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

describe('integration labels', () => {
  it('names kinds, statuses and secrets in words', () => {
    expect(kindLabel('qbittorrent')).toBe('qBittorrent');
    expect(kindLabel('romm')).toBe('RomM export');
    expect(kindLabel('webhook')).toBe('webhook');
    expect(integrationStatusLabel({ status: 'ok', enabled: true })).toBe('Working');
    expect(integrationStatusLabel({ status: 'ok', enabled: false })).toBe('Turned off');
    expect(integrationStatusLabel({ status: 'unconfigured', enabled: true })).toBe('Not tried yet');
    expect(integrationStatusLabel({ status: 'failing', enabled: true })).toBe('Failing');
    expect(secretsLabel(qbittorrent())).toBe('Password set');
    expect(secretsLabel(qbittorrent({ secrets: [{ field: 'password', set: false }] }))).toBe(
      'Password not set',
    );
    expect(secretsLabel(qbittorrent({ secrets: [{ field: 'api_key', set: true }] }))).toBe(
      'Api key set',
    );
    expect(secretsLabel(qbittorrent({ secrets: [] }))).toBe('None needed');
  });
});

describe('the download client form', () => {
  it('starts with the defaults, picking the only watched root', () => {
    expect(emptyClientForm('qbittorrent', ['downloads']).localRoot).toBe('downloads');
    expect(emptyClientForm('sabnzbd', ['a', 'b']).localRoot).toBe('');
    expect(emptyClientForm('nzbget', []).category).toBe('tangible');
  });

  it('makes a qBittorrent, sending a password only when one was typed', () => {
    const form = {
      ...emptyClientForm('qbittorrent', ['downloads']),
      name: ' NAS ',
      baseUrl: ' http://nas:8080 ',
      username: 'tangible',
      clientPath: '/downloads/tangible',
      category: '',
    };
    expect(createClientRequest(form)).toEqual({
      kind: 'qbittorrent',
      name: 'NAS',
      enabled: true,
      settings: {
        base_url: 'http://nas:8080',
        username: 'tangible',
        category: 'tangible',
        client_path: '/downloads/tangible',
        local_root: 'downloads',
        after_import: 'keep',
      },
    });
    expect(createClientRequest({ ...form, secret: ' secret ' }).secrets).toEqual({
      password: ' secret ',
    });
  });

  it('makes a SABnzbd with no user and an API key', () => {
    const form = {
      ...emptyClientForm('sabnzbd', ['downloads']),
      name: 'SAB',
      baseUrl: 'http://nas:8080/sabnzbd',
      username: 'left over from another kind',
      clientPath: '/downloads/complete/tangible',
      secret: 'k',
    };
    const request = createClientRequest(form);
    expect(request.kind).toBe('sabnzbd');
    expect(request.settings).not.toHaveProperty('username');
    expect(request.secrets).toEqual({ api_key: 'k' });
  });

  it('makes an NZBGet with a control user and password', () => {
    const request = createClientRequest({
      ...emptyClientForm('nzbget', ['downloads']),
      name: 'NZBGet',
      baseUrl: 'http://nas:6789/',
      username: 'nzbget',
      clientPath: '/downloads/completed/tangible',
      secret: 'p',
    });
    expect(request.kind).toBe('nzbget');
    expect(request.settings).toMatchObject({ username: 'nzbget' });
    expect(request.secrets).toEqual({ password: 'p' });
  });

  it('makes an S3 integration with its endpoint, buckets and both keys', () => {
    const request = createClientRequest({
      ...emptyClientForm('s3', []),
      name: 'Archive',
      baseUrl: ' http://192.168.1.13:8333 ',
      region: '',
      buckets: 'discs\n old-discs , \n',
      accessKey: 'AKID',
      secret: 'shh',
    });
    expect(request).toEqual({
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
    expect(parseBuckets(' a ,b\n\nc ')).toEqual(['a', 'b', 'c']);
  });

  it('edits S3 without holding its keys, and clears both together', () => {
    const view = qbittorrent({
      kind: 's3',
      settings: {
        endpoint: 'https://s3.example.internal/',
        region: 'eu-west-1',
        path_style: false,
        buckets: ['a', 'b'],
      },
      secrets: [
        { field: 'access_key_id', set: true },
        { field: 'secret_access_key', set: true },
      ],
    });
    const form = clientFormFrom({ ...view, kind: 's3' });
    expect(form.baseUrl).toBe('https://s3.example.internal/');
    expect(form.pathStyle).toBe(false);
    expect(form.buckets).toBe('a\nb');
    expect(form.accessKey).toBe('');
    expect(updateClientRequest(form).secrets).toBeUndefined();
    expect(updateClientRequest({ ...form, secret: 'new' }).secrets).toEqual({
      secret_access_key: 'new',
    });
    expect(updateClientRequest({ ...form, clearSecret: true }).secrets).toEqual({
      access_key_id: null,
      secret_access_key: null,
    });
  });

  it('edits an integration without ever holding its secret', () => {
    const form = clientFormFrom({ ...qbittorrent(), kind: 'qbittorrent' });
    expect(form.secret).toBe('');
    expect(form.baseUrl).toBe('http://192.168.1.13:8080/');
    expect(updateClientRequest(form).secrets).toBeUndefined();
    expect(updateClientRequest({ ...form, secret: 'new' }).secrets).toEqual({
      password: 'new',
    });
    expect(updateClientRequest({ ...form, secret: 'ignored', clearSecret: true }).secrets).toEqual({
      password: null,
    });
    const sab = clientFormFrom({
      ...qbittorrent({ kind: 'sabnzbd', secrets: [{ field: 'api_key', set: true }] }),
      kind: 'sabnzbd',
    });
    expect(updateClientRequest({ ...sab, clearSecret: true }).secrets).toEqual({ api_key: null });
  });

  it('words each kind, and knows which kinds have a form', () => {
    expect(clientWording('qbittorrent').secretLabel).toBe('WebUI password');
    expect(clientWording('sabnzbd').usernameLabel).toBeNull();
    expect(clientWording('nzbget').usernameLabel).toBe('Control user');
    expect(isClientKind('nzbget')).toBe(true);
    expect(isClientKind('romm')).toBe(false);
    expect(versionLabel('qbittorrent')).toBe('WebUI API');
    expect(versionLabel('sabnzbd')).toBe('version');
    expect(isClientKind('romm_api')).toBe(true);
    expect(kindLabel('romm_api')).toBe("RomM's API");
    expect(versionLabel('romm_api')).toBe('RomM');
    expect(clientWording('romm_api').secretField).toBe('token');
  });

  it("makes RomM's API with its addresses, the scan switch and a token", () => {
    const form = {
      ...emptyClientForm('romm_api', ['downloads']),
      name: 'RomM',
      baseUrl: ' http://romm:8080/ ',
      secret: 'rmm_token',
    };
    expect(createClientRequest(form)).toEqual({
      kind: 'romm_api',
      name: 'RomM',
      enabled: true,
      settings: { base_url: 'http://romm:8080/', scan_after_export: true },
      secrets: { token: 'rmm_token' },
    });
    const linked = createClientRequest({
      ...form,
      webUrl: ' https://romm.example.net/ ',
      scanAfterExport: false,
    });
    expect(linked.settings).toEqual({
      base_url: 'http://romm:8080/',
      web_url: 'https://romm.example.net/',
      scan_after_export: false,
    });

    const view = qbittorrent({
      kind: 'romm_api',
      name: 'RomM',
      settings: {
        base_url: 'http://romm:8080/',
        web_url: 'https://romm.example.net/',
        scan_after_export: false,
      },
      secrets: [{ field: 'token', set: true }],
    });
    const edited = clientFormFrom({ ...view, kind: 'romm_api' });
    expect(edited).toMatchObject({
      baseUrl: 'http://romm:8080/',
      webUrl: 'https://romm.example.net/',
      scanAfterExport: false,
      secret: '',
    });
    expect(updateClientRequest(edited).secrets).toBeUndefined();
    expect(updateClientRequest({ ...edited, clearSecret: true }).secrets).toEqual({ token: null });
  });

  it('says what to do when the server has no key', () => {
    const problem = (code: string, detail: string): Problem => ({
      type: 'about:blank',
      title: 'Refused',
      status: 409,
      code,
      detail,
    });
    expect(integrationRefusal(problem('SECRET_KEY_MISSING', 'x'))).toContain(
      'TANGIBLE_SECRET_KEY_FILE',
    );
    expect(integrationRefusal(problem('CONFLICT', 'another integration has that name'))).toBe(
      'another integration has that name',
    );
  });

  it('removes by id, escaped', async () => {
    const seen: string[] = [];
    const fetcher = (async (input: string | URL | Request, init?: RequestInit) => {
      seen.push(`${init?.method} ${String(input)}`);
      return { ok: true, status: 204, headers: new Headers() } as Response;
    }) as typeof fetch;
    const result = await deleteIntegration('a/b', fetcher);
    expect(result.kind).toBe('ok');
    expect(seen).toEqual(['DELETE /api/v1/integrations/a%2Fb']);
  });
});

describe('trying an integration', () => {
  it('says how it went', () => {
    expect(testOutcome('NAS', { ok: true, api_version: '2.11.4', checks: [] })).toBe(
      'NAS signed in; WebUI API 2.11.4.',
    );
    expect(testOutcome('SAB', { ok: true, api_version: '4.5.3', checks: [] }, 'sabnzbd')).toBe(
      'SAB signed in; version 4.5.3.',
    );
    expect(
      testOutcome('NAS', {
        ok: false,
        error_code: 'INTEGRATION_AUTH_FAILED',
        detail: 'qBittorrent refused to sign in: the user name or password is wrong',
        checks: [],
      }),
    ).toBe('NAS did not work: qBittorrent refused to sign in: the user name or password is wrong');
    expect(testOutcome('RomM', { ok: true, api_version: '5.4.1', checks: [] }, 'romm_api')).toBe(
      'RomM answered as RomM 5.4.1; exported games can be found.',
    );
    expect(
      testOutcome(
        'RomM',
        {
          ok: false,
          error_code: 'INTEGRATION_MISCONFIGURED',
          detail: 'the client token lacks the roms.read scope',
          checks: [],
        },
        'romm_api',
      ),
    ).toBe('RomM will not do: the client token lacks the roms.read scope');
  });

  it('posts to the integration, escaped', async () => {
    const seen: string[] = [];
    const fetcher = (async (input: string | URL | Request, init?: RequestInit) => {
      seen.push(`${init?.method} ${String(input)}`);
      return {
        ok: true,
        status: 200,
        headers: new Headers(),
        json: async () => ({ ok: true }),
      } as Response;
    }) as typeof fetch;
    await testIntegration('a/b', fetcher);
    expect(seen).toEqual(['POST /api/v1/integrations/a%2Fb/test']);
  });
});

describe('removing after import', () => {
  it('is offered for download clients and S3, and not for RomM', () => {
    for (const kind of ['qbittorrent', 'sabnzbd', 'nzbget', 's3'] as const) {
      expect(removesAfterImport(kind)).toBe(true);
      expect(afterImportWording(kind).remove).not.toBe('');
    }
    expect(removesAfterImport('romm_api')).toBe(false);
    expect(afterImportWording('sabnzbd').needs).toContain('TANGIBLE_REMOVABLE_ROOTS');
    expect(afterImportWording('s3').needs).toContain('s3:DeleteObject');
  });

  it('reads a stored choice back and sends it on every save', () => {
    const removing = clientFormFrom({
      ...qbittorrent({ settings: { ...qbittorrent().settings, after_import: 'remove' } }),
      kind: 'qbittorrent',
    });
    expect(removing.afterImport).toBe('remove');
    expect(updateClientRequest(removing).settings?.after_import).toBe('remove');
    const keeping = clientFormFrom({ ...qbittorrent(), kind: 'qbittorrent' });
    expect(keeping.afterImport).toBe('keep');
    expect(createClientRequest(keeping).settings.after_import).toBe('keep');
    expect(
      createClientRequest(emptyClientForm('romm_api', [])).settings.after_import,
    ).toBeUndefined();
  });

  it('needs a removable root for a Usenet client, and nothing more for the others', () => {
    const sab = { ...emptyClientForm('sabnzbd', []), localRoot: 'downloads' };
    expect(removalBlocked(sab, ['usenet'])).toContain('TANGIBLE_REMOVABLE_ROOTS');
    expect(removalBlocked({ ...sab, localRoot: 'usenet' }, ['usenet'])).toBeNull();
    expect(removalBlocked({ ...sab, localRoot: '' }, ['usenet'])).toBe(
      'Choose the watched root first.',
    );
    expect(removalBlocked(emptyClientForm('qbittorrent', ['downloads']), [])).toBeNull();
    expect(removalBlocked(emptyClientForm('s3', []), [])).toBeNull();
  });
});
