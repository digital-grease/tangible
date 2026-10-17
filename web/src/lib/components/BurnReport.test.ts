// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { forgetReportSettings, type ReportSettings } from '$lib/report';
import BurnReport from './BurnReport.svelte';

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

const failed = { id: 'a1', state: 'write_failed', engine: 'auto', attempt_number: 1 };
const verified = { id: 'a2', state: 'verified', engine: 'xorriso', attempt_number: 2 };

/** The server, answering the report route; records what it was asked. */
function server(status: number, body: unknown): string[] {
  const asked: string[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request) => {
      asked.push(String(input));
      return {
        ok: status >= 200 && status < 300,
        status,
        json: async () => body,
        headers: new Headers(),
      } as Response;
    }),
  );
  return asked;
}

const report = {
  template: 'burn_report.yml',
  title: 'Burn: write failed',
  fields: [
    { id: 'version', label: 'Tangible version', value: 'v0.1.0 (76fa515)' },
    { id: 'outcome', label: 'What did Tangible record?', value: 'Write failed' },
    { id: 'worker_log', label: 'Worker log', value: 'error: ENGINE_FAILED\n' },
  ],
  long_field: 'worker_log',
  removed: { path: 1 },
  truncated: false,
};

describe('BurnReport', () => {
  it('offers nothing on a server with reports turned off', () => {
    const { container } = render(BurnReport, {
      props: { attempt: failed, settings: { ...ON, enabled: false, issues_url: null } },
    });
    expect(container.textContent?.trim()).toBe('');
    render(BurnReport, { props: { attempt: failed, settings: null } });
    expect(screen.queryByRole('button')).toBeNull();
  });

  it('offers a problem report, and "worked" only for a disc that worked', () => {
    render(BurnReport, { props: { attempt: failed, settings: ON } });
    expect(screen.getByRole('button', { name: 'Report a problem with this burn…' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: /as working/ })).toBeNull();
  });

  it('offers both for a verified disc from a real engine', () => {
    render(BurnReport, { props: { attempt: verified, settings: ON } });
    expect(screen.getByRole('button', { name: 'Report a problem with this burn…' })).toBeTruthy();
    expect(
      screen.getByRole('button', { name: 'Report this drive and disc as working…' }),
    ).toBeTruthy();
  });

  it('prepares the report on the server and opens it for review', async () => {
    const asked = server(200, report);
    render(BurnReport, { props: { attempt: failed, settings: ON } });
    await fireEvent.click(screen.getByRole('button', { name: 'Report a problem with this burn…' }));
    expect(asked).toEqual(['/api/v1/burn-attempts/a1/report?kind=problem']);
    const dialog = await screen.findByRole('dialog', { name: 'Review this burn report' });
    expect(dialog.hasAttribute('open')).toBe(true);
    expect(screen.getByText('Write failed')).toBeTruthy();
    expect(screen.getByText('Replaced before you saw it: 1 path.')).toBeTruthy();
  });

  it('asks for a hardware report for "worked"', async () => {
    const asked = server(200, { ...report, template: 'hardware_report.yml', long_field: null });
    render(BurnReport, { props: { attempt: verified, settings: ON } });
    await fireEvent.click(
      screen.getByRole('button', { name: 'Report this drive and disc as working…' }),
    );
    expect(asked).toEqual(['/api/v1/burn-attempts/a2/report?kind=worked']);
    expect(await screen.findByRole('dialog', { name: 'Review this hardware report' })).toBeTruthy();
  });

  it('says why when the server will not prepare one', async () => {
    server(409, {
      type: 'about:blank',
      title: 'Conflict',
      status: 409,
      code: 'CONFLICT',
      detail: 'a burn on the fake engine says nothing about real hardware',
    });
    render(BurnReport, { props: { attempt: verified, settings: ON } });
    await fireEvent.click(
      screen.getByRole('button', { name: 'Report this drive and disc as working…' }),
    );
    const refusal = await screen.findByRole('region', { name: 'The report could not be prepared' });
    expect(refusal.textContent).toContain('says nothing about real hardware');
    expect(refusal.textContent).toContain('CONFLICT');
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
  });
});
