// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen } from '@testing-library/svelte';
import { tick } from 'svelte';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { axeViolations } from '$lib/testing/axe';
import { navFor } from '$lib/nav';
import { server } from './testing';

const goto = vi.fn();
vi.mock('$app/navigation', () => ({ goto: (href: string) => goto(href) }));

const { default: CommandBar } = await import('./CommandBar.svelte');

// jsdom has no modal dialogs; see ReportPreview.test.ts.
beforeAll(() => {
  HTMLDialogElement.prototype.showModal = function (this: HTMLDialogElement) {
    this.setAttribute('open', '');
  };
  HTMLDialogElement.prototype.close = function (this: HTMLDialogElement) {
    if (!this.hasAttribute('open')) return;
    this.removeAttribute('open');
    this.dispatchEvent(new Event('close'));
  };
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
  goto.mockReset();
});

async function opened() {
  render(CommandBar, { groups: navFor('viewer') });
  await fireEvent.click(screen.getByRole('button', { name: 'Search or run a command (Ctrl+K)' }));
  await tick();
  return screen.getByRole('combobox', { name: 'Search or run a command' });
}

describe('the command bar', () => {
  it('opens on to its input, offering the actions and the pages', async () => {
    const input = await opened();
    expect(document.activeElement).toBe(input);
    expect(screen.getByRole('option', { name: 'Create a burn job' })).toBeTruthy();
    expect(screen.getByRole('option', { name: /Physical copies/ })).toBeTruthy();
    expect(input.getAttribute('aria-activedescendant')).toBe(screen.getAllByRole('option')[0]?.id);
    const dialog = screen.getByRole('dialog', { name: 'Command bar' });
    expect(await axeViolations(dialog)).toEqual([]);
  });

  it('narrows as you type, moves with the arrow keys, and goes with Enter', async () => {
    const input = await opened();
    await fireEvent.input(input, { target: { value: 'burn' } });
    const options = screen.getAllByRole('option');
    expect(options.map((option) => option.textContent?.replace(/\s+/g, ' ').trim())).toEqual([
      'Create a burn job',
      'Burns Activity',
    ]);
    await fireEvent.keyDown(input, { key: 'ArrowDown' });
    expect(input.getAttribute('aria-activedescendant')).toBe(options[1]?.id);
    expect(options[1]?.getAttribute('aria-selected')).toBe('true');
    await fireEvent.keyDown(input, { key: 'Enter' });
    expect(goto).toHaveBeenCalledWith('/burns');
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('finds artifacts by name in the library', async () => {
    vi.useFakeTimers();
    server({
      '/api/v1/artifacts?name=debian': {
        status: 200,
        body: {
          items: [{ id: 'a1', source_filename: 'debian-13.1.0-amd64-netinst.iso', format: 'iso' }],
          next_cursor: null,
        },
      },
    });
    const input = await opened();
    await fireEvent.input(input, { target: { value: 'debian' } });
    await vi.advanceTimersByTimeAsync(250);
    vi.useRealTimers();
    const found = await screen.findByRole('option', { name: /debian-13\.1\.0-amd64-netinst\.iso/ });
    await fireEvent.click(found);
    expect(goto).toHaveBeenCalledWith('/library/a1');
  });

  it('opens with Ctrl+K from anywhere', async () => {
    render(CommandBar, { groups: navFor('viewer') });
    expect(screen.queryByRole('dialog')).toBeNull();
    await fireEvent.keyDown(window, { key: 'k', ctrlKey: true });
    await tick();
    expect(screen.getByRole('dialog', { name: 'Command bar' })).toBeTruthy();
  });
});
