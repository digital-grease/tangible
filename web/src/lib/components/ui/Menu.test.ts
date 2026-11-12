// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render } from '@testing-library/svelte';
import { tick } from 'svelte';
import { describe, expect, it, vi } from 'vitest';
import { axeViolations } from '$lib/testing/axe';
import UiHarness from '$lib/testing/UiHarness.svelte';

describe('Menu', () => {
  it('starts closed, its button saying so', async () => {
    const { getByRole, container } = render(UiHarness, { show: 'menu' });
    const button = getByRole('button', { name: 'New' });
    expect(button.getAttribute('aria-expanded')).toBe('false');
    const list = document.getElementById(button.getAttribute('aria-controls') ?? '');
    expect(list?.hidden).toBe(true);
    expect(await axeViolations(container)).toEqual([]);
  });

  it('opens on to its first item, moves with the arrow keys, and closes with Escape', async () => {
    const { getByRole, container } = render(UiHarness, { show: 'menu' });
    const button = getByRole('button', { name: 'New' });
    await fireEvent.click(button);
    await tick();
    expect(button.getAttribute('aria-expanded')).toBe('true');
    expect(document.activeElement).toBe(getByRole('link', { name: 'Import' }));
    expect(await axeViolations(container)).toEqual([]);
    await fireEvent.keyDown(window, { key: 'ArrowDown' });
    expect(document.activeElement).toBe(getByRole('link', { name: 'Rip a disc' }));
    await fireEvent.keyDown(window, { key: 'ArrowUp' });
    await fireEvent.keyDown(window, { key: 'ArrowUp' });
    expect(document.activeElement).toBe(getByRole('button', { name: 'Create burn job' }));
    await fireEvent.keyDown(window, { key: 'Escape' });
    await tick();
    expect(button.getAttribute('aria-expanded')).toBe('false');
    expect(document.activeElement).toBe(button);
  });

  it('runs an action item and closes', async () => {
    const onselect = vi.fn();
    const { getByRole } = render(UiHarness, { show: 'menu', onselect });
    await fireEvent.click(getByRole('button', { name: 'New' }));
    await tick();
    await fireEvent.click(getByRole('button', { name: 'Create burn job' }));
    await tick();
    expect(onselect).toHaveBeenCalledWith('burn');
    expect(getByRole('button', { name: 'New' }).getAttribute('aria-expanded')).toBe('false');
  });

  it('closes when the pointer goes down elsewhere', async () => {
    const { getByRole } = render(UiHarness, { show: 'menu' });
    const button = getByRole('button', { name: 'New' });
    await fireEvent.click(button);
    await tick();
    await fireEvent.pointerDown(document.body);
    await tick();
    expect(button.getAttribute('aria-expanded')).toBe('false');
  });
});
