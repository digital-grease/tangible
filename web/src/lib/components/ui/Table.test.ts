// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { render } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import { axeViolations } from '$lib/testing/axe';
import UiHarness from '$lib/testing/UiHarness.svelte';

describe('Table', () => {
  it('scrolls inside a named group a keyboard can reach, and keeps its caption', async () => {
    const { getAllByRole, getByRole, container } = render(UiHarness, { show: 'panel' });
    const region = getAllByRole('group').find((element) =>
      element.classList.contains('table-region'),
    );
    expect(region?.getAttribute('aria-label')).toBe('Latest burns');
    expect(region?.getAttribute('tabindex')).toBe('0');
    const table = getByRole('table', { name: 'Latest burns' });
    expect(table.querySelector('caption')?.classList.contains('visually-hidden')).toBe(true);
    expect(table.querySelectorAll('tbody tr')).toHaveLength(2);
    expect(await axeViolations(container)).toEqual([]);
  });
});
