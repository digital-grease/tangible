// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen, within } from '@testing-library/svelte';
import { createRawSnippet } from 'svelte';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { forgetReportSettings } from '$lib/report';
import { axeViolations } from '$lib/testing/axe';
import { DASHBOARD, server } from './testing';

vi.mock('$app/navigation', () => ({ goto: vi.fn() }));
const { default: AppShell } = await import('./AppShell.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
  forgetReportSettings();
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
    expect(screen.getByRole('link', { name: 'Buy me a coffee' }).getAttribute('href')).toBe(
      'https://www.buymeacoffee.com/digitalgrease',
    );
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

  it('expands the icons-only sidebar when asked, and remembers it', async () => {
    const stored = new Map<string, string>();
    vi.stubGlobal('localStorage', {
      getItem: (key: string) => stored.get(key) ?? null,
      setItem: (key: string, value: string) => void stored.set(key, value),
    });
    // jsdom's window is 1024 pixels wide: icons only, left alone.
    expect(window.innerWidth).toBe(1024);
    const { container } = shell('viewer');
    const app = container.querySelector('.app');
    expect(app?.classList.contains('compact')).toBe(true);

    await fireEvent.click(screen.getByRole('button', { name: 'Expand the sidebar' }));
    expect(app?.classList.contains('compact')).toBe(false);
    expect(stored.get('tangible.sidebar')).toBe('full');

    await fireEvent.click(screen.getByRole('button', { name: 'Collapse the sidebar' }));
    expect(app?.classList.contains('compact')).toBe(true);
    expect(stored.get('tangible.sidebar')).toBe('compact');
    expect(await axeViolations(container)).toEqual([]);
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

describe('Reporting an issue', () => {
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

  const ISSUES = 'https://github.com/digital-grease/tangible/issues/new';
  const REPORT = {
    template: 'bug_report.yml',
    title: null,
    fields: [
      { id: 'version', label: 'Tangible version', value: 'v0.2.1 (fc4dc51)' },
      { id: 'area', label: 'Area', value: 'library' },
      { id: 'steps', label: 'Steps to reproduce', value: '1. On the page /library/{id}: \n' },
    ],
    long_field: null,
    removed: {},
    truncated: false,
  };

  /** A server with reports on or off; answers what "Report an issue" sent. */
  function reporting(enabled: boolean) {
    const sent: { url: string; method: string; body: unknown }[] = [];
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
        const url = String(input);
        const method = init?.method ?? 'GET';
        if (method === 'POST') {
          sent.push({ url, method, body: JSON.parse(String(init?.body ?? 'null')) });
        }
        const body =
          url === '/api/v1/report-settings'
            ? { enabled, issues_url: enabled ? ISSUES : null, version: '0.2.1', build: 'fc4dc51' }
            : url === '/api/v1/issue-reports'
              ? REPORT
              : url === '/api/v1/dashboard'
                ? DASHBOARD
                : { items: [] };
        return {
          ok: true,
          status: 200,
          json: async () => body,
          headers: new Headers(),
        } as Response;
      }),
    );
    render(AppShell, {
      session: {
        username: 'ada',
        role: 'viewer',
        csrf_token: 't',
        expires_at: '2026-11-12T00:00:00Z',
      },
      pathname: '/library/01a1e1bc-5e71-74f2-b979-0130a78da3b6',
      theme: 'system',
      onTheme: vi.fn(),
      onSignOut: vi.fn(),
      children: page,
    });
    return sent;
  }

  it('prepares the bug form for the page, and shows it for review first', async () => {
    const sent = reporting(true);
    const [button] = await screen.findAllByRole('button', { name: 'Report an issue' });
    await fireEvent.click(button as HTMLElement);
    // The page as a route, its identifier taken out, and its area.
    await vi.waitFor(() =>
      expect(sent).toEqual([
        {
          url: '/api/v1/issue-reports',
          method: 'POST',
          body: { page: '/library/{id}', area: 'library' },
        },
      ]),
    );
    expect(await screen.findByRole('heading', { name: 'Review this bug report' })).toBeTruthy();
    expect(screen.getByText('v0.2.1 (fc4dc51)')).toBeTruthy();
  });

  it('offers nothing where reports are off', async () => {
    reporting(false);
    expect(await screen.findByRole('link', { name: 'Buy me a coffee' })).toBeTruthy();
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(screen.queryByRole('button', { name: 'Report an issue' })).toBeNull();
  });
});
