// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen, within } from '@testing-library/svelte';
import { createRawSnippet } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { axeViolations } from '$lib/testing/axe';
import { DASHBOARD, server } from './testing';

vi.mock('$app/navigation', () => ({ goto: vi.fn() }));
const { default: AppShell } = await import('./AppShell.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
});

const page = createRawSnippet(() => ({ render: () => '<h1>Library</h1>' }));

function shell(role: string, pathname = '/library/a1') {
  server({
    '/api/v1/dashboard': { status: 200, body: DASHBOARD },
    '/api/v1/drives': { status: 200, body: { items: [] } },
  });
  const onTheme = vi.fn();
  const onSignOut = vi.fn();
  const rendered = render(AppShell, {
    session: { username: 'ada', role, csrf_token: 't', expires_at: '2026-11-12T00:00:00Z' },
    pathname,
    theme: 'system',
    onTheme,
    onSignOut,
    children: page,
  });
  return { ...rendered, onTheme, onSignOut };
}

describe('the shell', () => {
  it('groups the pages, marks where you are, and says where in the top bar', async () => {
    const { container } = shell('administrator');
    const nav = screen.getByRole('navigation', { name: 'Primary' });
    const current = within(nav).getByRole('link', { current: 'page' });
    expect(current.textContent).toContain('Library');
    expect(within(nav).getByText('Administration')).toBeTruthy();
    expect(
      within(nav)
        .getByRole('link', { name: /Physical copies/ })
        .getAttribute('href'),
    ).toBe('/discs');
    expect(container.querySelector('.where')?.textContent).toBe('Collection/Library');
    expect(screen.getByRole('main').textContent).toBe('Library');
    expect(await axeViolations(container)).toEqual([]);
  });

  it('keeps the administrators’ pages from everyone else', () => {
    shell('viewer');
    const nav = screen.getByRole('navigation', { name: 'Primary' });
    expect(within(nav).queryByText('Administration')).toBeNull();
    expect(within(nav).queryByRole('link', { name: /Audit log/ })).toBeNull();
  });

  it('counts what is under way beside Imports and Burns, in words for a screen reader', async () => {
    shell('operator');
    const nav = screen.getByRole('navigation', { name: 'Primary' });
    expect(await within(nav).findByRole('link', { name: 'Imports , 2 under way' })).toBeTruthy();
    expect(
      within(nav).getByRole('link', { name: 'Burns , 5 queued, waiting for a disc or under way' }),
    ).toBeTruthy();
  });

  it('carries the account, the theme, signing out and the source on every page', async () => {
    const { onTheme, onSignOut } = shell('viewer');
    expect(screen.getByText('ada')).toBeTruthy();
    await fireEvent.change(screen.getByRole('combobox', { name: 'Theme' }), {
      target: { value: 'dark' },
    });
    expect(onTheme).toHaveBeenCalledWith('dark');
    await fireEvent.click(screen.getByRole('button', { name: /Theme: Follow the system/ }));
    expect(onTheme).toHaveBeenCalledWith('light');
    await fireEvent.click(screen.getByRole('button', { name: 'Sign out' }));
    expect(onSignOut).toHaveBeenCalledOnce();
    expect(screen.getByRole('link', { name: 'Source code' }).getAttribute('href')).toBeTruthy();
  });

  it('opens the menu as a drawer on a narrow screen, and closes it with Escape', async () => {
    const { container } = shell('viewer');
    const menu = screen.getByRole('button', { name: 'Menu' });
    expect(menu.getAttribute('aria-expanded')).toBe('false');
    await fireEvent.click(menu);
    expect(menu.getAttribute('aria-expanded')).toBe('true');
    expect(container.querySelector('.app')?.classList.contains('drawer-open')).toBe(true);
    await fireEvent.keyDown(window, { key: 'Escape' });
    expect(menu.getAttribute('aria-expanded')).toBe('false');
    expect(document.activeElement).toBe(menu);
  });

  it('offers the three ways to start something new', async () => {
    shell('operator');
    await fireEvent.click(screen.getByRole('button', { name: 'New' }));
    expect(screen.getByRole('link', { name: 'Import a file or a URL' }).getAttribute('href')).toBe(
      '/imports',
    );
    expect(screen.getByRole('link', { name: 'Rip a disc' }).getAttribute('href')).toBe('/rips');
    expect(screen.getByRole('link', { name: 'Create a burn job' }).getAttribute('href')).toBe(
      '/burns/new',
    );
  });
});
