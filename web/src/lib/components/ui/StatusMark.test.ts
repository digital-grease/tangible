// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { render } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import { axeViolations } from '$lib/testing/axe';
import StatusMark from './StatusMark.svelte';
import { TONES } from './tone';

/** The shape a mark draws, so tones never differ by colour alone. */
function shape(container: HTMLElement): string {
  const svg = container.querySelector('svg');
  return [...(svg?.children ?? [])].map((child) => child.tagName.toLowerCase()).join('+');
}

describe('StatusMark', () => {
  it.each(TONES)('says a %s state in words, the mark hidden from screen readers', async (tone) => {
    const { getByText, container } = render(StatusMark, { tone, label: 'Structurally valid' });
    expect(getByText('Structurally valid')).toBeTruthy();
    expect(container.querySelector('svg')?.getAttribute('aria-hidden')).toBe('true');
    expect(await axeViolations(container)).toEqual([]);
  });

  it('gives every tone its own shape', () => {
    const shapes = TONES.map((tone) => {
      const { container, unmount } = render(StatusMark, { tone, label: tone });
      const drawn =
        shape(container) + ':' + (container.querySelector('svg path')?.getAttribute('d') ?? '');
      unmount();
      return drawn;
    });
    expect(new Set(shapes).size).toBe(TONES.length);
  });

  it('can sit on a tint, for dense tables', () => {
    const { container } = render(StatusMark, { tone: 'bad', label: 'Damaged', pill: true });
    expect(container.querySelector('.mark')?.classList.contains('pill')).toBe(true);
  });
});
