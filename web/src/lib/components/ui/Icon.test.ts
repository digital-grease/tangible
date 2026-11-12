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
    for (const d of Object.values(ICONS)) {
      expect(d).toMatch(/^[Mm]/);
      expect(d).not.toMatch(/[^MmLlHhVvCcSsAaZz0-9.\s,-]/);
    }
  });
});
