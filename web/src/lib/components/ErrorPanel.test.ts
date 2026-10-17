// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { tick } from 'svelte';
import { forgetReportSettings, reportSettingsOnce, type ReportSettings } from '$lib/report';
import type { Problem } from '$lib/session';
import ErrorPanel from './ErrorPanel.svelte';

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

const ON: ReportSettings = {
  enabled: true,
  issues_url: 'https://github.com/digital-grease/tangible/issues/new',
  version: '0.1.0',
  release: 'v0.1.0',
  revision: '76fa5157aea012aae85461e694082d553071a606',
  build: 'v0.1.0 (76fa515)',
};

const OFF: ReportSettings = { ...ON, enabled: false, issues_url: null };

const INTERNAL: Problem = {
  type: 'about:blank',
  title: 'Internal server error',
  status: 500,
  code: 'INTERNAL',
  detail: 'the catalogue could not be read',
  request_id: '0190f5a2-7c1e-7b3d-9a4f-000000000001',
};

const REFUSED: Problem = {
  type: 'about:blank',
  title: 'Validation failed',
  status: 422,
  code: 'VALIDATION_FAILED',
  detail: 'label must be at most 200 characters',
};

const report = {
  template: 'bug_report.yml',
  title: 'Bug: internal server error (INTERNAL)',
  fields: [
    { id: 'version', label: 'Tangible version', value: 'v0.1.0 (76fa515)' },
    { id: 'area', label: 'Area', value: 'library' },
    {
      id: 'logs',
      label: 'Relevant log output',
      value: 'INTERNAL: the catalogue could not be read\n',
    },
  ],
  long_field: 'logs',
  removed: {},
  truncated: false,
};

interface Asked {
  url: string;
  method: string;
  body: unknown;
}

/** The server: report settings, and the error report route; records the calls. */
function server(settings: ReportSettings, answer: { status: number; body: unknown }): Asked[] {
  const asked: Asked[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const url = String(input);
      const method = init?.method ?? 'GET';
      asked.push({
        url,
        method,
        body: typeof init?.body === 'string' ? JSON.parse(init.body) : undefined,
      });
      const reply = url === '/api/v1/report-settings' ? { status: 200, body: settings } : answer;
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

describe('ErrorPanel', () => {
  it('shows the page heading, the title, and the detail with its code and request id', async () => {
    server(OFF, { status: 500, body: {} });
    render(ErrorPanel, {
      props: { problem: INTERNAL, area: 'library', heading: 'Could not load the library' },
    });
    expect(
      screen.getByRole('heading', { level: 2, name: 'Could not load the library' }),
    ).toBeTruthy();
    expect(screen.getByText('Internal server error.')).toBeTruthy();
    expect(screen.getByText('the catalogue could not be read')).toBeTruthy();
    expect(screen.getByText('INTERNAL')).toBeTruthy();
    expect(screen.getByText('0190f5a2-7c1e-7b3d-9a4f-000000000001')).toBeTruthy();
  });

  it('leads a refusal with its title, as a region announced at once', () => {
    server(OFF, { status: 500, body: {} });
    render(ErrorPanel, { props: { problem: REFUSED, area: 'catalog', refusal: true, level: 3 } });
    const region = screen.getByRole('region', { name: 'Validation failed' });
    expect(region.getAttribute('aria-live')).toBe('assertive');
    expect(screen.getByRole('heading', { level: 3, name: 'Validation failed' })).toBeTruthy();
    expect(region.textContent).toContain('label must be at most 200 characters');
    expect(screen.queryByText('Request id:', { exact: false })).toBeNull();
  });

  it('offers a report for a defect when reports are on', async () => {
    server(ON, { status: 200, body: report });
    render(ErrorPanel, { props: { problem: INTERNAL, area: 'library' } });
    expect(await screen.findByRole('button', { name: 'Report this…' })).toBeTruthy();
  });

  it('offers none when reports are off', async () => {
    const asked = server({ ...ON, enabled: false }, { status: 200, body: report });
    render(ErrorPanel, { props: { problem: INTERNAL, area: 'library' } });
    await vi.waitFor(() =>
      expect(asked.map((call) => call.url)).toContain('/api/v1/report-settings'),
    );
    // The panel's own settings request has settled once this has.
    await reportSettingsOnce();
    await tick();
    expect(screen.queryByRole('button', { name: 'Report this…' })).toBeNull();
  });

  it('offers none for a request refused for its own reasons', async () => {
    const asked = server(ON, { status: 200, body: report });
    render(ErrorPanel, { props: { problem: REFUSED, area: 'catalog', refusal: true } });
    await vi.waitFor(() =>
      expect(asked.map((call) => call.url)).toContain('/api/v1/report-settings'),
    );
    // The panel's own settings request has settled once this has.
    await reportSettingsOnce();
    await tick();
    expect(screen.queryByRole('button', { name: 'Report this…' })).toBeNull();
  });

  it('posts what was shown and opens the review dialog', async () => {
    const asked = server(ON, { status: 200, body: report });
    render(ErrorPanel, {
      props: { problem: INTERNAL, area: 'library', endpoint: 'GET /api/v1/artifacts' },
    });
    await fireEvent.click(await screen.findByRole('button', { name: 'Report this…' }));
    const dialog = await screen.findByRole('dialog', { name: 'Review this bug report' });
    expect(dialog.hasAttribute('open')).toBe(true);
    const posted = asked.find((call) => call.url === '/api/v1/error-reports');
    expect(posted?.method).toBe('POST');
    expect(posted?.body).toEqual({
      code: 'INTERNAL',
      title: 'Internal server error',
      detail: 'the catalogue could not be read',
      status: 500,
      request_id: '0190f5a2-7c1e-7b3d-9a4f-000000000001',
      endpoint: 'GET /api/v1/artifacts',
      area: 'library',
    });
  });

  it('says why when the report could not be prepared', async () => {
    server(ON, {
      status: 409,
      body: {
        type: 'about:blank',
        title: 'Conflict',
        status: 409,
        code: 'CONFLICT',
        detail: 'issue reports are turned off on this server',
      },
    });
    render(ErrorPanel, { props: { problem: INTERNAL, area: 'library' } });
    await fireEvent.click(await screen.findByRole('button', { name: 'Report this…' }));
    expect(
      await screen.findByText(
        'The report could not be prepared: issue reports are turned off on this server',
      ),
    ).toBeTruthy();
    expect(screen.queryByRole('dialog')).toBeNull();
  });
});
