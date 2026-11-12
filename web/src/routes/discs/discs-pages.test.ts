// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen, within } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { axeViolations } from '$lib/testing/axe';

vi.mock('$app/state', () => ({ page: { params: { id: 'p1' } } }));

const { default: DiscsPage } = await import('./+page.svelte');
const { default: DiscPage } = await import('./[id]/+page.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
});

const at = '2026-11-11T12:00:00Z';
function copy(extra: Record<string, unknown> = {}) {
  return {
    id: 'p1',
    artifact_id: 'a1',
    burn_attempt_id: 'b1',
    disc_id: 'd1',
    label: 'Debian 13.1, disc 1',
    status: 'verified',
    media_profile: 'CD-R',
    manufacturer_id: null,
    media_serial: null,
    storage_location: 'Shelf B',
    notes: null,
    verification_level: 'full_sector_readback',
    verification_result: 'passed',
    check_count: 1,
    last_checked_at: at,
    should_be_destroyed: false,
    is_settled: false,
    created_at: at,
    updated_at: at,
    ...extra,
  };
}

function server(routes: Record<string, unknown>) {
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request) => {
      const body = routes[String(input)];
      return {
        ok: body !== undefined,
        status: body === undefined ? 404 : 200,
        json: async () => body ?? {},
        headers: new Headers(),
      } as Response;
    }),
  );
}

describe('Physical copies', () => {
  it('lists every disc with its condition, and those to destroy first', async () => {
    server({
      '/api/v1/physical-copies': {
        items: [
          copy(),
          copy({
            id: 'p2',
            label: 'Family photos, disc 1',
            status: 'verification_failed',
            should_be_destroyed: true,
          }),
        ],
        next_cursor: null,
      },
    });
    const { container } = render(DiscsPage);
    expect(await screen.findByRole('heading', { level: 1, name: 'Physical copies' })).toBeTruthy();
    expect(await screen.findByText('1 disc should be destroyed')).toBeTruthy();
    const table = await screen.findByRole('table', { name: 'Discs, newest first' });
    expect(within(table).getByText('Verified — read back and matched')).toBeTruthy();
    expect(within(table).getByText('Failed verification — destroy this disc')).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);
  });

  it('shows one disc: how it was made, its checks, and what can be recorded', async () => {
    server({
      '/api/v1/physical-copies/p1': {
        ...copy(),
        checks: [
          {
            id: 'c1',
            checked_at: at,
            method: 'full_sector_readback',
            result: 'passed',
            checked_with: 'Slimtype DS8A8SH',
            notes: null,
          },
        ],
      },
    });
    const { container } = render(DiscPage);
    expect(
      await screen.findByRole('heading', { level: 1, name: 'Debian 13.1, disc 1' }),
    ).toBeTruthy();
    expect(
      within(screen.getByRole('navigation', { name: 'Breadcrumb' }))
        .getByRole('link', { name: 'Physical copies' })
        .getAttribute('href'),
    ).toBe('/discs');
    expect(screen.getByRole('region', { name: 'How it was made' })).toBeTruthy();
    expect(screen.getByRole('table', { name: 'Checks recorded for this disc' })).toBeTruthy();
    expect(screen.getByLabelText('Where it is kept')).toBeTruthy();
    expect(screen.getByLabelText('What you found')).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);

    await fireEvent.click(screen.getByRole('button', { name: 'Mark this disc destroyed' }));
    expect(screen.getByText('This cannot be undone.')).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Yes, this disc has been destroyed' })).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: 'Keep it' }));
    expect(screen.getByRole('button', { name: 'Mark this disc destroyed' })).toBeTruthy();
  });

  it('records nothing further against a destroyed disc', async () => {
    server({
      '/api/v1/physical-copies/p1': {
        ...copy({ status: 'destroyed', is_settled: true }),
        checks: [],
      },
    });
    render(DiscPage);
    expect(
      await screen.findByText(/Its record remains so the burn history stays true/),
    ).toBeTruthy();
    expect(screen.queryByRole('region', { name: 'Record what you know' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'Mark this disc destroyed' })).toBeNull();
  });
});
