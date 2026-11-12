// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { createRawSnippet } from 'svelte';
import { render } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import { axeViolations } from '$lib/testing/axe';
import Badge from './Badge.svelte';

describe('Badge', () => {
  it('shows its words with a decorative icon', async () => {
    const { getByText, container } = render(Badge, {
      icon: 'lock',
      strong: true,
      children: createRawSnippet(() => ({ render: () => '<span>Original, immutable</span>' })),
    });
    expect(getByText('Original, immutable')).toBeTruthy();
    expect(container.querySelector('svg')?.getAttribute('aria-hidden')).toBe('true');
    expect(container.querySelector('.badge')?.classList.contains('strong')).toBe(true);
    expect(await axeViolations(container)).toEqual([]);
  });
});
