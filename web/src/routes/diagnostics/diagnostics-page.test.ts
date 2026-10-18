// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen, within } from '@testing-library/svelte';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { forgetReportSettings } from '$lib/report';
import Page from './+page.svelte';

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
  forgetReportSettings();
});

const ISSUES = 'https://github.com/digital-grease/tangible/issues/new';

const DIAGNOSTICS = {
  generated_at: '2026-10-18T12:00:00Z',
  build: 'v0.1.0 (76fa515)',
  status: 'problem',
  markdown: '## Tangible diagnostics\n',
  sections: [
    {
      title: 'Database',
      checks: [
        { name: 'Connection', status: 'ok', detail: 'reachable' },
        { name: 'Schema', status: 'problem', detail: '1 migration(s) not applied (10)' },
      ],
    },
    {
      title: 'Settings',
      checks: [{ name: 'Public URL', status: 'warn', detail: 'plain http' }],
    },
  ],
};

const REPORT = {
  template: 'bug_report.yml',
  title: 'Bug: diagnostics show 1 problem(s)',
  fields: [
    { id: 'area', label: 'Area', value: 'deployment' },
    { id: 'logs', label: 'Logs', value: '## Tangible diagnostics\n' },
  ],
  long_field: 'logs',
  removed: {},
  truncated: false,
};

type Reply = { status: number; body: unknown };

/** The server, by path; records what was asked. */
function server(routes: Record<string, Reply>): string[] {
  const asked: string[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request) => {
      const url = String(input);
      asked.push(url);
      const reply = routes[url] ?? { status: 404, body: {} };
      return {
        ok: reply.status >= 200 && reply.status < 300,
        status: reply.status,
        json: async () => reply.body,
        headers: new Headers(),
      } as Response;
    }),
  );
  return asked;
}

function settings(enabled: boolean): Reply {
  return {
    status: 200,
    body: {
      enabled,
      issues_url: enabled ? ISSUES : null,
      version: '0.1.0',
      build: 'v0.1.0 (76fa515)',
    },
  };
}

describe('Diagnostics page', () => {
  it('shows the checklist with every status in words', async () => {
    server({
      '/api/v1/diagnostics': { status: 200, body: DIAGNOSTICS },
      '/api/v1/report-settings': settings(false),
    });
    render(Page);
    expect(await screen.findByRole('heading', { name: 'Overall: Problem' })).toBeTruthy();
    expect(screen.getByText(/1 problem, 1 warning, 1 OK\./)).toBeTruthy();
    const database = screen.getByRole('table', { name: 'Database' });
    const schema = within(database).getByRole('row', { name: /Schema/ });
    expect(schema.textContent).toContain('Problem');
    expect(schema.textContent).toContain('1 migration(s) not applied (10)');
    expect(
      within(screen.getByRole('table', { name: 'Settings' })).getByText('Warning'),
    ).toBeTruthy();
  });

  it('offers a report only when reports are on', async () => {
    server({
      '/api/v1/diagnostics': { status: 200, body: DIAGNOSTICS },
      '/api/v1/report-settings': settings(false),
    });
    render(Page);
    await screen.findByRole('heading', { name: 'Overall: Problem' });
    expect(screen.getByRole('button', { name: 'Copy as Markdown' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Report a problem…' })).toBeNull();
  });

  it('prepares the report on the server and opens it for review', async () => {
    const asked = server({
      '/api/v1/diagnostics': { status: 200, body: DIAGNOSTICS },
      '/api/v1/report-settings': settings(true),
      '/api/v1/diagnostics/report': { status: 200, body: REPORT },
    });
    render(Page);
    await fireEvent.click(await screen.findByRole('button', { name: 'Report a problem…' }));
    const dialog = await screen.findByRole('dialog', { name: 'Review this diagnostics report' });
    expect(dialog.hasAttribute('open')).toBe(true);
    expect(asked).toContain('/api/v1/diagnostics/report');
  });

  it('tells anyone but an administrator that the page is not theirs', async () => {
    server({
      '/api/v1/diagnostics': {
        status: 403,
        body: {
          type: 'about:blank',
          title: 'Forbidden',
          status: 403,
          code: 'FORBIDDEN',
          detail: 'needs diagnose',
        },
      },
      '/api/v1/report-settings': settings(false),
    });
    render(Page);
    expect(await screen.findByText('Only an administrator can see diagnostics.')).toBeTruthy();
  });

  it('says why when diagnostics could not be collected', async () => {
    server({
      '/api/v1/diagnostics': {
        status: 503,
        body: {
          type: 'about:blank',
          title: 'Storage is unavailable',
          status: 503,
          code: 'STORAGE_UNAVAILABLE',
          detail: 'diagnostics are not available on this server',
        },
      },
      '/api/v1/report-settings': settings(false),
    });
    render(Page);
    expect(
      await screen.findByRole('heading', { name: 'Could not collect diagnostics' }),
    ).toBeTruthy();
    expect(screen.getByText('diagnostics are not available on this server')).toBeTruthy();
  });
});
