// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render } from '@testing-library/svelte';
import { tick } from 'svelte';
import { describe, expect, it } from 'vitest';
import { axeViolations } from '$lib/testing/axe';
import UiHarness from '$lib/testing/UiHarness.svelte';

describe('Tabs', () => {
  it('is a named tab list with one tab stop and its panel', async () => {
    const { getByRole, getAllByRole, container } = render(UiHarness, { show: 'tabs' });
    expect(getByRole('tablist', { name: 'Artifact' })).toBeTruthy();
    const tabs = getAllByRole('tab');
    expect(tabs.map((tab) => tab.textContent?.trim())).toEqual([
      'Summary',
      'Components9',
      'Topology',
    ]);
    expect(tabs.map((tab) => tab.getAttribute('tabindex'))).toEqual(['0', '-1', '-1']);
    const panel = getByRole('tabpanel');
    expect(panel.getAttribute('aria-labelledby')).toBe(tabs[0]?.id);
    expect(panel.textContent?.trim()).toBe('Showing summary.');
    expect(await axeViolations(container)).toEqual([]);
  });

  it('moves with the arrow keys, Home and End, selecting as it goes', async () => {
    const { getAllByRole, getByRole } = render(UiHarness, { show: 'tabs' });
    const tabs = getAllByRole('tab');
    const [first, second, third] = tabs as [HTMLElement, HTMLElement, HTMLElement];
    first.focus();
    await fireEvent.keyDown(first, { key: 'ArrowRight' });
    await tick();
    expect(document.activeElement).toBe(second);
    expect(second.getAttribute('aria-selected')).toBe('true');
    expect(getByRole('tabpanel').textContent?.trim()).toBe('Showing components.');
    await fireEvent.keyDown(second, { key: 'End' });
    await tick();
    expect(document.activeElement).toBe(third);
    await fireEvent.keyDown(third, { key: 'ArrowRight' });
    await tick();
    expect(document.activeElement).toBe(first);
    await fireEvent.keyDown(first, { key: 'ArrowLeft' });
    await tick();
    expect(document.activeElement).toBe(third);
    await fireEvent.keyDown(third, { key: 'Home' });
    await tick();
    expect(first.getAttribute('aria-selected')).toBe('true');
  });

  it('selects a tab that is clicked', async () => {
    const { getByRole } = render(UiHarness, { show: 'tabs' });
    await fireEvent.click(getByRole('tab', { name: 'Topology' }));
    expect(getByRole('tabpanel').textContent?.trim()).toBe('Showing topology.');
  });
});
