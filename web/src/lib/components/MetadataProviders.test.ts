// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen, within } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { forgetReportSettings } from '$lib/report';
import { axeViolations } from '$lib/testing/axe';
import type { IntegrationView } from '$lib/integrations';
import MetadataProviders from './MetadataProviders.svelte';

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

function provider(kind: string, extra: Partial<IntegrationView> = {}): IntegrationView {
  const secrets: Record<string, string[]> = {
    musicbrainz: [],
    tmdb: ['read_access_token'],
    igdb: ['client_id', 'client_secret'],
  };
  return {
    id: `${kind}-id`,
    kind,
    name: { musicbrainz: 'MusicBrainz', tmdb: 'TMDB', igdb: 'IGDB' }[kind] ?? kind,
    enabled: true,
    settings: {},
    secrets: (secrets[kind] ?? []).map((field) => ({ field, set: true })),
    status: 'ok',
    managed: false,
    ...extra,
  } as IntegrationView;
}

const SETTINGS = { status: 200, body: { enabled: false, version: '0.1.0', build: '0.1.0' } };

describe('Metadata providers', () => {
  it('offers to add each provider, and adds one with its key', async () => {
    const asked = server({
      'POST /api/v1/integrations': { status: 201, body: provider('tmdb') },
      '/api/v1/report-settings': SETTINGS,
    });
    const onchanged = vi.fn(async () => {});
    const { container } = render(MetadataProviders, {
      props: { items: [provider('qbittorrent')], onchanged },
    });
    expect(screen.getAllByText('Not added')).toHaveLength(3);
    expect(await axeViolations(container)).toEqual([]);

    await fireEvent.click(screen.getByRole('button', { name: 'Add TMDB' }));
    const form = screen.getByRole('form', { name: 'Add TMDB' });
    expect(within(form).getByText(/Not the short API key/)).toBeTruthy();
    const token = within(form).getByLabelText(/API Read Access Token/) as HTMLInputElement;
    expect(token.type).toBe('password');
    await fireEvent.input(token, { target: { value: ' the-token ' } });
    expect(await axeViolations(container)).toEqual([]);
    await fireEvent.click(within(form).getByRole('button', { name: 'Add TMDB' }));

    await vi.waitFor(() => expect(onchanged).toHaveBeenCalled());
    const sent = asked.find((call) => call.method === 'POST');
    expect(JSON.parse(sent?.body ?? '{}')).toEqual({
      kind: 'tmdb',
      name: 'TMDB',
      enabled: true,
      settings: {},
      secrets: { read_access_token: 'the-token' },
    });
    expect(await screen.findByText(/Added TMDB/)).toBeTruthy();
    expect(screen.queryByRole('form', { name: 'Add TMDB' })).toBeNull();
  });

  it('adds MusicBrainz without a key', async () => {
    const asked = server({
      'POST /api/v1/integrations': { status: 201, body: provider('musicbrainz') },
      '/api/v1/report-settings': SETTINGS,
    });
    render(MetadataProviders, { props: { items: [], onchanged: async () => {} } });
    await fireEvent.click(screen.getByRole('button', { name: 'Add MusicBrainz' }));
    const form = screen.getByRole('form', { name: 'Add MusicBrainz' });
    expect(within(form).getByText(/No key/)).toBeTruthy();
    await fireEvent.click(within(form).getByRole('button', { name: 'Add MusicBrainz' }));
    await vi.waitFor(() =>
      expect(JSON.parse(asked.find((call) => call.method === 'POST')?.body ?? '{}')).toMatchObject({
        kind: 'musicbrainz',
        secrets: {},
      }),
    );
  });

  it('tries, turns off and removes a provider, asking before it removes', async () => {
    const asked = server({
      'POST /api/v1/integrations/igdb-id/test': {
        status: 200,
        body: {
          ok: false,
          error_code: 'INTEGRATION_AUTH_FAILED',
          detail: 'Twitch refused',
          checks: [],
        },
      },
      'PATCH /api/v1/integrations/igdb-id': {
        status: 200,
        body: provider('igdb', { enabled: false }),
      },
      'DELETE /api/v1/integrations/igdb-id': { status: 204, body: null },
      '/api/v1/report-settings': SETTINGS,
    });
    render(MetadataProviders, {
      props: { items: [provider('igdb')], onchanged: async () => {} },
    });
    expect(screen.getByText('Client id set, Client secret set')).toBeTruthy();

    await fireEvent.click(screen.getByRole('button', { name: 'Test IGDB' }));
    expect(await screen.findByText('IGDB did not work: Twitch refused')).toBeTruthy();

    await fireEvent.click(screen.getByRole('button', { name: 'Turn off IGDB' }));
    expect(await screen.findByText(/searches do not ask it/)).toBeTruthy();
    expect(asked).toContainEqual({
      url: '/api/v1/integrations/igdb-id',
      method: 'PATCH',
      body: '{"enabled":false}',
    });

    await fireEvent.click(screen.getByRole('button', { name: 'Remove IGDB' }));
    expect(asked.some((call) => call.method === 'DELETE')).toBe(false);
    await fireEvent.click(await screen.findByRole('button', { name: 'Yes, remove IGDB' }));
    expect(await screen.findByText('Removed IGDB.')).toBeTruthy();
  });

  it('sends only the parts of a key typed when changing it', async () => {
    const asked = server({
      'PATCH /api/v1/integrations/igdb-id': { status: 200, body: provider('igdb') },
      '/api/v1/report-settings': SETTINGS,
    });
    render(MetadataProviders, {
      props: { items: [provider('igdb')], onchanged: async () => {} },
    });
    await fireEvent.click(screen.getByRole('button', { name: "Change IGDB's key" }));
    const form = screen.getByRole('form', { name: 'A new key for IGDB' });
    await fireEvent.input(within(form).getByLabelText(/Client secret/), {
      target: { value: 'new-secret' },
    });
    await fireEvent.click(within(form).getByRole('button', { name: 'Save the key' }));
    await vi.waitFor(() =>
      expect(asked).toContainEqual({
        url: '/api/v1/integrations/igdb-id',
        method: 'PATCH',
        body: '{"secrets":{"client_secret":"new-secret"}}',
      }),
    );
  });
});
