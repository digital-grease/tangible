// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen } from '@testing-library/svelte';
import { tick } from 'svelte';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { forgetReportSettings, reportSettingsOnce } from '$lib/report';
import FormatReport from './FormatReport.svelte';

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

const unknown = { id: 'a1', format: 'unknown', format_confidence: 0, validation_state: 'valid' };
const warned = {
  id: 'a2',
  format: 'cue_bin',
  format_confidence: 0.99,
  validation_state: 'valid_with_warnings',
};
const clean = { id: 'a3', format: 'iso', format_confidence: 0.99, validation_state: 'valid' };

const REPORT = {
  template: 'format_support.yml',
  title: 'Format: an image Tangible did not recognise',
  fields: [
    { id: 'version', label: 'Tangible version', value: 'v0.1.0 (76fa515)' },
    { id: 'structure', label: 'Structure', value: 'Detected: unknown\n' },
  ],
  long_field: 'structure',
  removed: { name: 1 },
  truncated: false,
};

/** The server: report settings and the format report; records the calls. */
function server(enabled: boolean, report: { status: number; body: unknown }): string[] {
  const asked: string[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request) => {
      const url = String(input);
      asked.push(url);
      const reply =
        url === '/api/v1/report-settings'
          ? {
              status: 200,
              body: {
                enabled,
                issues_url: enabled ? ISSUES : null,
                version: '0.1.0',
                build: 'v0.1.0 (76fa515)',
              },
            }
          : report;
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

async function settled() {
  await reportSettingsOnce();
  await tick();
}

describe('FormatReport', () => {
  it('says an unrecognised image was not recognised, and offers support', async () => {
    server(true, { status: 200, body: REPORT });
    render(FormatReport, { props: { artifact: unknown } });
    expect(
      await screen.findByRole('heading', { name: 'Tangible did not recognise this image' }),
    ).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Ask for support…' })).toBeTruthy();
  });

  it('offers a warned image under its own heading', async () => {
    server(true, { status: 200, body: REPORT });
    render(FormatReport, { props: { artifact: warned } });
    expect(await screen.findByRole('heading', { name: 'Ask about this format' })).toBeTruthy();
  });

  it('shows nothing for a clean image, or with reports off', async () => {
    server(true, { status: 200, body: REPORT });
    const { container } = render(FormatReport, { props: { artifact: clean } });
    await settled();
    expect(container.textContent?.trim()).toBe('');

    forgetReportSettings();
    server(false, { status: 200, body: REPORT });
    const off = render(FormatReport, { props: { artifact: unknown } });
    await settled();
    expect(off.container.textContent?.trim()).toBe('');
  });

  it('prepares the report on the server and opens it for review', async () => {
    const asked = server(true, { status: 200, body: REPORT });
    render(FormatReport, { props: { artifact: unknown } });
    await fireEvent.click(await screen.findByRole('button', { name: 'Ask for support…' }));
    const dialog = await screen.findByRole('dialog', { name: 'Review this format report' });
    expect(dialog.hasAttribute('open')).toBe(true);
    expect(asked).toContain('/api/v1/artifacts/a1/format-report');
  });

  it('says why when the server will not prepare one', async () => {
    server(true, {
      status: 409,
      body: {
        type: 'about:blank',
        title: 'Conflict',
        status: 409,
        code: 'CONFLICT',
        detail: 'this image was recognised without doubts or warnings',
      },
    });
    render(FormatReport, { props: { artifact: warned } });
    await fireEvent.click(await screen.findByRole('button', { name: 'Ask for support…' }));
    const refusal = await screen.findByRole('region', {
      name: 'The report could not be prepared',
    });
    expect(refusal.textContent).toContain('recognised without doubts');
  });
});
