// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { axeViolations } from '$lib/testing/axe';
import DriveBay from './DriveBay.svelte';
import { WRITING, drive, server } from './testing';

/** Storage that keeps what it is given, as a browser's localStorage does. */
let stored: Map<string, string>;
beforeEach(() => {
  stored = new Map();
  vi.stubGlobal('localStorage', {
    getItem: (key: string) => stored.get(key) ?? null,
    setItem: (key: string, value: string) => void stored.set(key, value),
    removeItem: (key: string) => void stored.delete(key),
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

const drives = (items: unknown[]) => ({ '/api/v1/drives': { status: 200, body: { items } } });

describe('the drive bay', () => {
  it('opens itself when a drive is at work, saying what and how far', async () => {
    server(drives([drive('d1', 'Slimtype DS8A8SH', WRITING), drive('d2', 'LG GH24NSD5')]));
    const { container } = render(DriveBay);
    expect(await screen.findByText('Slimtype DS8A8SH')).toBeTruthy();
    expect(screen.getByRole('region', { name: 'Drives' })).toBeTruthy();
    expect(screen.getByText('Writing')).toBeTruthy();
    expect(screen.getByText('Idle')).toBeTruthy();
    expect(
      screen.getByRole('link', { name: 'Debian 13.1 netinst, disc 1' }).getAttribute('href'),
    ).toBe('/burns/j1');
    const bar = screen.getByRole('progressbar', { name: 'Slimtype DS8A8SH: Writing' });
    expect(bar.getAttribute('aria-valuenow')).toBe('43');
    expect(screen.getByText('43%')).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Drives' }).getAttribute('aria-expanded')).toBe(
      'true',
    );
    expect(await axeViolations(container)).toEqual([]);
  });

  it("shows a drive's device path to an administrator only", async () => {
    server(drives([drive('d1', 'Slimtype DS8A8SH', WRITING)]));
    const first = render(DriveBay);
    await screen.findByText('Slimtype DS8A8SH');
    expect(screen.queryByText(/\/dev\//)).toBeNull();
    first.unmount();

    render(DriveBay, { administrator: true });
    await screen.findByText('Slimtype DS8A8SH');
    expect(screen.getByText(/\/dev\//)).toBeTruthy();
  });

  it('folds to one line while every drive is idle, and opens and folds by hand', async () => {
    server(drives([drive('d1', 'Slimtype DS8A8SH'), drive('d2', 'LG GH24NSD5')]));
    render(DriveBay);
    expect(await screen.findByText('2 drives, all idle')).toBeTruthy();
    const toggle = screen.getByRole('button', { name: 'Drives' });
    expect(toggle.getAttribute('aria-expanded')).toBe('false');
    await fireEvent.click(toggle);
    expect(toggle.getAttribute('aria-expanded')).toBe('true');
    expect(stored.get('tangible.drive-bay')).toBe('open');
    await fireEvent.click(toggle);
    expect(toggle.getAttribute('aria-expanded')).toBe('false');
    expect(stored.get('tangible.drive-bay')).toBe('folded');
  });

  it('says there are no drives, and shows an administrator where to add one', async () => {
    server(drives([]));
    render(DriveBay, { administrator: true });
    expect(await screen.findByText('No drives yet.')).toBeTruthy();
    expect(screen.getByRole('link', { name: 'Add a burn worker' }).getAttribute('href')).toBe(
      '/workers',
    );
  });

  it('says so when the drives cannot be read', async () => {
    server({
      '/api/v1/drives': {
        status: 503,
        body: {
          type: 'about:blank',
          title: 'Unavailable',
          status: 503,
          code: 'STORAGE_UNAVAILABLE',
          detail: 'drives could not be read',
        },
      },
    });
    render(DriveBay);
    expect(await screen.findByText('The drives could not be read')).toBeTruthy();
  });
});
