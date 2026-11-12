// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { render } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import { axeViolations } from '$lib/testing/axe';
import Disc from './Disc.svelte';

const mixedMode = {
  media_family: 'cd',
  sector_count: 200_000,
  tracks: [
    { session: 1, sector_count: 100_000, is_audio: false },
    { session: 1, sector_count: 50_000, is_audio: true },
    { session: 1, sector_count: 50_000, is_audio: true },
  ],
};

describe('Disc', () => {
  it('is hidden from screen readers, as the words beside it say what it shows', async () => {
    const { container } = render(Disc, { shape: mixedMode, size: 120 });
    const svg = container.querySelector('svg');
    expect(svg?.getAttribute('aria-hidden')).toBe('true');
    expect(await axeViolations(container)).toEqual([]);
  });

  it('draws a ring for each run of tracks', () => {
    const { container } = render(Disc, { shape: mixedMode, size: 120 });
    const rings = [...container.querySelectorAll('circle.ring')];
    expect(rings.map((ring) => ring.classList.contains('audio'))).toEqual([false, true, true]);
  });

  it('draws the written area of a burn and the rest still to write', () => {
    const { container } = render(Disc, {
      mode: 'burn',
      shape: mixedMode,
      capacity: 359_844,
      progress: 0.43,
    });
    expect(container.querySelector('circle.ring.written')).not.toBeNull();
    expect(container.querySelector('circle.ring.to-write')).not.toBeNull();
  });

  it('draws an empty tray as an outline only', () => {
    const { container } = render(Disc, { mode: 'empty' });
    expect(container.querySelectorAll('circle.empty')).toHaveLength(2);
    expect(container.querySelector('circle.ring')).toBeNull();
  });

  it('gives each drawing its own sheen, so removing one cannot break another', () => {
    const { container } = render(Disc, { shape: mixedMode });
    const second = render(Disc, { shape: mixedMode }).container;
    const id = (root: HTMLElement) => root.querySelector('linearGradient')?.id;
    expect(id(container)).toBeTruthy();
    expect(id(container)).not.toBe(id(second));
  });
});
