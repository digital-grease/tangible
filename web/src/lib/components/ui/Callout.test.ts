// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { render } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import { axeViolations } from '$lib/testing/axe';
import UiHarness from '$lib/testing/UiHarness.svelte';

describe('Callout', () => {
  it('is a section named by its title, or a note without one', async () => {
    const { getByRole, container } = render(UiHarness, { show: 'callout' });
    const section = getByRole('region', { name: 'A burn is waiting for a disc' });
    expect(section.classList.contains('warn')).toBe(true);
    // Marked: the warning's shape before its words, which name the section.
    expect(section.querySelector('h2 svg')).toBeTruthy();
    expect(getByRole('note').textContent).toContain('Nothing is written');
    expect(await axeViolations(container)).toEqual([]);
  });
});
