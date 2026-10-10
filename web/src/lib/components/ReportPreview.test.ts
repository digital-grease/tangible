// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import type { ReportDraft } from '$lib/report';
import ReportPreview from './ReportPreview.svelte';

const ISSUES = 'https://github.com/digital-grease/tangible/issues/new';

// jsdom has no modal dialogs: open and close them the way a browser would,
// including the close event the component listens for.
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
  vi.restoreAllMocks();
  Object.defineProperty(window, 'isSecureContext', { value: false, configurable: true });
  Object.defineProperty(navigator, 'clipboard', { value: undefined, configurable: true });
});

function draft(log: string, removed: Record<string, number> = { path: 2 }): ReportDraft {
  return {
    template: 'burn_report.yml',
    fields: [
      { id: 'version', label: 'Tangible version', value: 'v0.1.0 (76fa515)' },
      { id: 'connection', label: 'Connection', value: '' },
      { id: 'worker_log', label: 'Worker log', value: log },
    ],
    longField: 'worker_log',
    removed,
    truncated: false,
  };
}

function secureClipboard(): ReturnType<typeof vi.fn> {
  const writeText = vi.fn(async () => {});
  Object.defineProperty(window, 'isSecureContext', { value: true, configurable: true });
  Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true });
  return writeText;
}

describe('ReportPreview', () => {
  it('shows everything the issue will contain, and that nothing was sent', () => {
    render(ReportPreview, {
      props: { draft: draft('ENGINE_FAILED\n'), issuesUrl: ISSUES, open: true },
    });
    const dialog = screen.getByRole('dialog', { name: 'Review this report' });
    expect(dialog.hasAttribute('open')).toBe(true);
    expect(screen.getByText(/Nothing has been sent/)).toBeTruthy();
    expect(screen.getByText('Replaced before you saw it: 2 paths.')).toBeTruthy();
    expect(screen.getByText('v0.1.0 (76fa515)')).toBeTruthy();
    expect(screen.getByText('Left for you to fill in.')).toBeTruthy();
    expect((screen.getByLabelText('Worker log') as HTMLTextAreaElement).value).toBe(
      'ENGINE_FAILED\n',
    );
  });

  it('opens the prefilled form in a new tab without the opener', async () => {
    const open = vi.spyOn(window, 'open').mockReturnValue(null);
    render(ReportPreview, {
      props: { draft: draft('ENGINE_FAILED\n'), issuesUrl: ISSUES, open: true },
    });
    await fireEvent.click(
      screen.getByRole('button', { name: 'Open the issue form on github.com' }),
    );
    expect(open).toHaveBeenCalledTimes(1);
    const [url, target, features] = open.mock.calls[0] ?? [];
    const query = new URL(String(url)).searchParams;
    expect(query.get('template')).toBe('burn_report.yml');
    expect(query.get('worker_log')).toBe('ENGINE_FAILED\n');
    expect(target).toBe('_blank');
    expect(features).toBe('noopener,noreferrer');
    await waitFor(() =>
      expect(screen.getByRole('status').textContent).toMatch(/Nothing is sent until you submit/),
    );
  });

  it('copies a log too long for the link before opening the form', async () => {
    const order: string[] = [];
    const writeText = secureClipboard();
    writeText.mockImplementation(async () => {
      order.push('copy');
    });
    vi.spyOn(window, 'open').mockImplementation(() => {
      order.push('open');
      return null;
    });
    const log = Array.from({ length: 3000 }, (_, i) => `event ${i}`).join('\n');
    render(ReportPreview, { props: { draft: draft(log), issuesUrl: ISSUES, open: true } });
    expect(screen.getByText(/too long for a link/)).toBeTruthy();
    await fireEvent.click(
      screen.getByRole('button', { name: 'Open the issue form on github.com' }),
    );
    expect(order).toEqual(['copy', 'open']);
    expect(writeText.mock.calls[0]?.[0]).toContain('event 0\n');
    await waitFor(() =>
      expect(screen.getByRole('status').textContent).toMatch(/full report is on your clipboard/),
    );
  });

  it('offers the text to copy by hand when copying is impossible', async () => {
    render(ReportPreview, {
      props: { draft: draft('ENGINE_FAILED\n'), issuesUrl: ISSUES, open: true },
    });
    expect(screen.queryByLabelText('The report as Markdown')).toBeNull();
    await fireEvent.click(screen.getByRole('button', { name: 'Copy report' }));
    await waitFor(() =>
      expect(screen.getByRole('status').textContent).toMatch(/could not be copied/),
    );
    const text = screen.getByLabelText('The report as Markdown') as HTMLTextAreaElement;
    expect(text.value).toContain('### Worker log');
  });

  it('copies the report as Markdown when the clipboard is available', async () => {
    const writeText = secureClipboard();
    render(ReportPreview, {
      props: { draft: draft('ENGINE_FAILED\n'), issuesUrl: ISSUES, open: true },
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Copy report' }));
    await waitFor(() =>
      expect(screen.getByRole('status').textContent).toMatch(/on your clipboard/),
    );
    expect(writeText.mock.calls[0]?.[0]).toContain('### Tangible version\n\nv0.1.0 (76fa515)');
  });

  it('closes, and returns focus to what opened it', async () => {
    const opener = document.createElement('button');
    opener.textContent = 'Report this burn';
    document.body.appendChild(opener);
    opener.focus();
    render(ReportPreview, { props: { draft: draft('x\n'), issuesUrl: ISSUES, open: true } });
    const dialog = screen.getByRole('dialog', { name: 'Review this report' });
    await fireEvent.click(screen.getByRole('button', { name: 'Close' }));
    await waitFor(() => expect(dialog.hasAttribute('open')).toBe(false));
    expect(document.activeElement).toBe(opener);
    opener.remove();
  });

  it('names a configured tracker on its button', () => {
    render(ReportPreview, {
      props: {
        draft: draft('x\n'),
        issuesUrl: 'https://git.example.org/me/t/issues/new',
        open: true,
      },
    });
    expect(
      screen.getByRole('button', { name: 'Open the issue form on git.example.org' }),
    ).toBeTruthy();
  });
});
