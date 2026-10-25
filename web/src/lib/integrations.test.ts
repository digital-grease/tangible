// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import { describe, expect, it } from 'vitest';
import {
  createQbittorrentRequest,
  deleteIntegration,
  emptyQbittorrentForm,
  integrationRefusal,
  integrationStatusLabel,
  kindLabel,
  qbittorrentFormFrom,
  secretsLabel,
  testIntegration,
  testOutcome,
  updateQbittorrentRequest,
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

describe('the qBittorrent form', () => {
  it('starts with the defaults, picking the only watched root', () => {
    expect(emptyQbittorrentForm(['downloads']).localRoot).toBe('downloads');
    expect(emptyQbittorrentForm(['a', 'b']).localRoot).toBe('');
    expect(emptyQbittorrentForm([]).category).toBe('tangible');
  });

  it('makes an integration, sending a password only when one was typed', () => {
    const form = {
      ...emptyQbittorrentForm(['downloads']),
      name: ' NAS ',
      baseUrl: ' http://nas:8080 ',
      username: 'tangible',
      clientPath: '/downloads/tangible',
      category: '',
    };
    expect(createQbittorrentRequest(form)).toEqual({
      kind: 'qbittorrent',
      name: 'NAS',
      enabled: true,
      settings: {
        base_url: 'http://nas:8080',
        username: 'tangible',
        category: 'tangible',
        client_path: '/downloads/tangible',
        local_root: 'downloads',
      },
    });
    expect(createQbittorrentRequest({ ...form, password: ' secret ' }).secrets).toEqual({
      password: ' secret ',
    });
  });

  it('edits an integration without ever holding its password', () => {
    const form = qbittorrentFormFrom(qbittorrent());
    expect(form.password).toBe('');
    expect(form.baseUrl).toBe('http://192.168.1.13:8080/');
    expect(updateQbittorrentRequest(form).secrets).toBeUndefined();
    expect(updateQbittorrentRequest({ ...form, password: 'new' }).secrets).toEqual({
      password: 'new',
    });
    expect(
      updateQbittorrentRequest({ ...form, password: 'ignored', clearPassword: true }).secrets,
    ).toEqual({ password: null });
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
    expect(testOutcome('NAS', { ok: true, api_version: '2.11.4' })).toBe(
      'NAS signed in; WebUI API 2.11.4.',
    );
    expect(
      testOutcome('NAS', {
        ok: false,
        error_code: 'INTEGRATION_AUTH_FAILED',
        detail: 'qBittorrent refused to sign in: the user name or password is wrong',
      }),
    ).toBe('NAS did not work: qBittorrent refused to sign in: the user name or password is wrong');
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
