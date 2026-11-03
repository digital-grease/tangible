// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

const { default: Lineage } = await import('./Lineage.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
});

type Asked = { method: string; url: string };

/** A fake server answering the lineage, and a request for a derivative. */
function server(suggested: string | null): Asked[] {
  const asked: Asked[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const url = String(input);
      const method = init?.method ?? 'GET';
      asked.push({ method, url });
      const body =
        method === 'POST'
          ? { result: 'exists', job: null, derivation: null }
          : { derived_from: null, derivatives: [], jobs: [], suggested_transformation: suggested };
      return {
        ok: true,
        status: 200,
        json: async () => body,
        headers: new Headers(),
      } as Response;
    }),
  );
  return asked;
}

describe('the Derivatives section', () => {
  it('offers a CUE/BIN of a CloneCD image, saying it drops the subchannel', async () => {
    const asked = server('cue_bin_from_ccd');
    render(Lineage, { artifactId: 'a1' });
    const button = await screen.findByRole('button', { name: 'Make a CUE/BIN' });
    expect(screen.getByText(/drops the subchannel/)).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Make a CHD' })).toBeNull();
    await fireEvent.click(button);
    expect(await screen.findByText('A CUE/BIN of this already exists.')).toBeTruthy();
    expect(
      asked.some((a) => a.method === 'POST' && a.url === '/api/v1/artifacts/a1/derivatives'),
    ).toBe(true);
  });

  it('offers an ISO of an Alcohol 120% DVD image', async () => {
    server('iso_from_mds');
    render(Lineage, { artifactId: 'a1' });
    const button = await screen.findByRole('button', { name: 'Make an ISO' });
    expect(screen.getByText(/leaves out the MDS descriptor/)).toBeTruthy();
    await fireEvent.click(button);
    expect(await screen.findByText('An ISO of this already exists.')).toBeTruthy();
  });

  it('offers a CUE/BIN extracted from an imported CD CHD', async () => {
    server('cue_bin_from_chd');
    render(Lineage, { artifactId: 'a1' });
    expect(await screen.findByRole('button', { name: 'Make a CUE/BIN' })).toBeTruthy();
    expect(screen.getByText(/Extracts this CHD/)).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Make a CHD' })).toBeNull();
  });

  it('offers a CHD where that is what suits, and nothing where nothing does', async () => {
    server('chd_create_cd');
    const { unmount } = render(Lineage, { artifactId: 'a1' });
    expect(await screen.findByRole('button', { name: 'Make a CHD' })).toBeTruthy();
    unmount();
    server(null);
    render(Lineage, { artifactId: 'a2' });
    expect(await screen.findByText('Nothing has been made from this artifact.')).toBeTruthy();
    expect(screen.queryByRole('button')).toBeNull();
  });
});
