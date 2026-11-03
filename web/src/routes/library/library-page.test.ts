// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

const { default: LibraryPage } = await import('./+page.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
});

function summary(id: string, extra: Record<string, unknown> = {}) {
  return {
    id,
    format: 'cue_bin',
    artifact_kind: 'multi_file_image',
    validation_state: 'valid',
    component_count: 2,
    total_bytes: 2048,
    source_filename: `${id}.cue`,
    has_warnings: false,
    integrity_state: 'unchecked',
    integrity_checked_at: null,
    best_match: null,
    ...extra,
  };
}

describe('Library page', () => {
  it('names what each artifact was identified as, a partial match marked', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          ({
            ok: true,
            status: 200,
            json: async () => ({
              items: [
                summary('exact', {
                  best_match: {
                    game_name: 'Game (USA)',
                    kind: 'exact',
                    dat_name: 'Sony - PlayStation',
                  },
                }),
                summary('partial', {
                  best_match: {
                    game_name: 'Other (Europe)',
                    kind: 'partial',
                    dat_name: 'Sony - PlayStation',
                  },
                }),
                summary('none'),
              ],
              next_cursor: null,
            }),
            headers: new Headers(),
          }) as Response,
      ),
    );
    render(LibraryPage);
    expect(await screen.findByRole('columnheader', { name: 'Identified as' })).toBeTruthy();
    const exact = screen.getByText('Game (USA)');
    expect(exact.getAttribute('title')).toBe('In Sony - PlayStation');
    expect(screen.getByText('Other (Europe) (partly)')).toBeTruthy();
    expect(screen.getByText('Not identified')).toBeTruthy();
  });
});

describe('Library filters', () => {
  function server(pages: Record<string, unknown[]>): string[] {
    const asked: string[] = [];
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: string | URL | Request) => {
        const url = String(input);
        asked.push(url);
        return {
          ok: true,
          status: 200,
          json: async () => ({ items: pages[url] ?? [], next_cursor: null }),
          headers: new Headers(),
        } as Response;
      }),
    );
    return asked;
  }

  it('asks for what the form says, and says when nothing matches', async () => {
    const asked = server({
      '/api/v1/artifacts': [summary('everything')],
      '/api/v1/artifacts?format=cue_bin&linked=false': [summary('unlinked')],
    });
    render(LibraryPage);
    expect(await screen.findByText('everything.cue')).toBeTruthy();

    await fireEvent.change(screen.getByLabelText('Format'), { target: { value: 'cue_bin' } });
    await fireEvent.change(screen.getByLabelText('Disc'), { target: { value: 'false' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Show' }));
    expect(await screen.findByText('unlinked.cue')).toBeTruthy();
    expect(asked).toContain('/api/v1/artifacts?format=cue_bin&linked=false');

    await fireEvent.change(screen.getByLabelText('Format'), { target: { value: 'chd' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Show' }));
    expect(await screen.findByText('Nothing in the library matches these filters.')).toBeTruthy();

    await fireEvent.click(screen.getByRole('button', { name: 'Show everything' }));
    expect(await screen.findByText('everything.cue')).toBeTruthy();
  });
});
