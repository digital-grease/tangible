// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { render, screen } from '@testing-library/svelte';
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
