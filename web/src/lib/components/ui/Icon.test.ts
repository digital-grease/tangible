// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { render } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import { axeViolations } from '$lib/testing/axe';
import Icon from './Icon.svelte';
import { ICONS } from './icons';

describe('Icon', () => {
  it('is hidden from screen readers when words beside it say what it means', async () => {
    const { container } = render(Icon, { name: 'burn' });
    const svg = container.querySelector('svg');
    expect(svg?.getAttribute('aria-hidden')).toBe('true');
    expect(svg?.getAttribute('role')).toBeNull();
    expect(await axeViolations(container)).toEqual([]);
  });

  it('is an image with a name when it stands alone', async () => {
    const { getByRole, container } = render(Icon, { name: 'lock', label: 'Original, immutable' });
    expect(getByRole('img', { name: 'Original, immutable' })).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);
  });

  it('draws every icon as one path on the 24-pixel grid', () => {
    for (const [name, d] of Object.entries(ICONS)) {
      expect(d, name).toMatch(/^[Mm]/);
      expect(d, name).not.toMatch(/[^MmLlHhVvCcSsAaZz0-9.\s,-]/);
      // Every point it reaches stays on the grid: a relative move joined
      // after another shape lands where that shape ended, not at the origin.
      for (const [x, y] of points(d)) {
        expect(x, `${name} x`).toBeGreaterThanOrEqual(0);
        expect(x, `${name} x`).toBeLessThanOrEqual(24);
        expect(y, `${name} y`).toBeGreaterThanOrEqual(0);
        expect(y, `${name} y`).toBeLessThanOrEqual(24);
      }
    }
  });
});

/** Every point a path's commands end at, in absolute coordinates. */
function points(d: string): [number, number][] {
  const sizes: Record<string, number> = {
    m: 2,
    l: 2,
    h: 1,
    v: 1,
    c: 6,
    s: 4,
    q: 4,
    t: 2,
    a: 7,
    z: 0,
  };
  const tokens = d.match(/[a-zA-Z]|-?(?:\d+\.?\d*|\.\d+)/g) ?? [];
  const reached: [number, number][] = [];
  let x = 0;
  let y = 0;
  let startX = 0;
  let startY = 0;
  let command = '';
  let at = 0;
  while (at < tokens.length) {
    const token = tokens[at] ?? '';
    if (/[a-zA-Z]/.test(token)) {
      command = token;
      at += 1;
      if (command.toLowerCase() === 'z') {
        x = startX;
        y = startY;
        continue;
      }
    }
    const lower = command.toLowerCase();
    const size = sizes[lower] ?? 0;
    const args = tokens.slice(at, at + size).map(Number);
    at += size;
    const relative = command === lower;
    if (lower === 'h') x = (relative ? x : 0) + (args[0] ?? 0);
    else if (lower === 'v') y = (relative ? y : 0) + (args[0] ?? 0);
    else {
      const nx = args[size - 2] ?? 0;
      const ny = args[size - 1] ?? 0;
      x = relative ? x + nx : nx;
      y = relative ? y + ny : ny;
    }
    if (lower === 'm') {
      startX = x;
      startY = y;
      // Further pairs after a move are lines.
      command = relative ? 'l' : 'L';
    }
    reached.push([x, y]);
  }
  return reached;
}
