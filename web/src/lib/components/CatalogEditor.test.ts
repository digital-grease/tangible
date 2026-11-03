// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

const { default: CatalogEditor } = await import('./CatalogEditor.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
});

function server(status: number, body: unknown) {
  const sent: { url: string; ifMatch: string | null; body: unknown }[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      sent.push({
        url: String(input),
        ifMatch: new Headers(init?.headers).get('if-match'),
        body: JSON.parse(String(init?.body)),
      });
      return {
        ok: status < 400,
        status,
        json: async () => body,
        headers: new Headers(),
      } as Response;
    }),
  );
  return sent;
}

const disc = {
  id: 'd1',
  disc_set_id: 's1',
  sequence_number: 1,
  display_name: 'Feature',
  media_family: 'dvd',
  region: null,
  volume_label: null,
  compatibility_claim: 'unknown',
  artifact_count: 0,
  copy_count: 0,
  created_at: '2026-10-01T00:00:00Z',
  version: 2,
};

describe('CatalogEditor', () => {
  it('saves only what changed, as read', async () => {
    const sent = server(200, { ...disc, region: 'B', version: 3 });
    const onsaved = vi.fn();
    render(CatalogEditor, { kind: 'disc', item: disc, onsaved, oncancel: vi.fn() });
    await fireEvent.input(screen.getByLabelText('Region'), { target: { value: 'B' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Save this disc' }));
    await vi.waitFor(() => expect(onsaved).toHaveBeenCalled());
    expect(sent).toEqual([{ url: '/api/v1/discs/d1', ifMatch: '"2"', body: { region: 'B' } }]);
  });

  it('says somebody changed it first, and saves nothing over them', async () => {
    server(409, {
      type: 'about:blank',
      title: 'Changed since it was read',
      status: 409,
      code: 'RESOURCE_VERSION_CONFLICT',
      detail: 'it is at version 3 now',
    });
    const onsaved = vi.fn();
    render(CatalogEditor, { kind: 'disc', item: disc, onsaved, oncancel: vi.fn() });
    await fireEvent.input(screen.getByLabelText('Name'), { target: { value: 'Extras' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Save this disc' }));
    expect(await screen.findByText(/Somebody changed this disc/)).toBeTruthy();
    expect(onsaved).not.toHaveBeenCalled();
  });

  it('closes without asking when nothing changed', async () => {
    const sent = server(200, disc);
    const oncancel = vi.fn();
    render(CatalogEditor, { kind: 'disc', item: disc, onsaved: vi.fn(), oncancel });
    await fireEvent.click(screen.getByRole('button', { name: 'Save this disc' }));
    expect(oncancel).toHaveBeenCalled();
    expect(sent).toEqual([]);
  });
});
