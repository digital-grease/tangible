// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { forgetReportSettings } from '$lib/report';

const goto = vi.fn();
vi.mock('$app/navigation', () => ({ goto: (...args: unknown[]) => goto(...args) }));
vi.mock('$app/state', () => ({ page: { params: { id: 'r1' } } }));

const { default: RipsPage } = await import('./+page.svelte');
const { default: RipPage } = await import('./[id]/+page.svelte');

// jsdom has no modal dialogs: open and close them the way a browser would,
// including the close event the dialog listens for.
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
  goto.mockReset();
});

type Reply = { status: number; body: unknown };

type Asked = { url: string; method: string; body?: string };

function server(routes: Record<string, Reply>): Asked[] {
  const asked: Asked[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const url = String(input);
      const method = init?.method ?? 'GET';
      asked.push({ url, method, ...(typeof init?.body === 'string' ? { body: init.body } : {}) });
      const reply = routes[`${method} ${url}`] ?? routes[url] ?? { status: 404, body: {} };
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

const DRIVE = {
  id: 'd1',
  name: 'sr0',
  device: '/dev/sr0',
  vendor: 'Slimtype',
  model: 'DVD A DS8A8SH',
  firmware: 'KS21',
  connection: 'usb',
  status: 'ready_with_media',
  worker_id: 'w1',
  worker_name: 'burner',
  worker_status: 'online',
  can_rip: true,
  can_decrypt_rips: false,
};

const DECRYPTING_DRIVE = { ...DRIVE, can_decrypt_rips: true };

function rip(extra: Record<string, unknown> = {}) {
  return {
    id: 'r1',
    drive_id: 'd1',
    drive_name: 'sr0',
    state: 'reading',
    is_open: true,
    can_cancel: true,
    requested_by: 'operator',
    decrypt: false,
    files: [],
    bytes_received: 0,
    created_at: '2026-10-19T12:00:00Z',
    ...extra,
  };
}

const SETTINGS = { status: 200, body: { enabled: false, version: '0.1.0', build: '0.1.0' } };

describe('Rips page', () => {
  it('rips the disc in a drive and opens the rip', async () => {
    const asked = server({
      '/api/v1/drives': { status: 200, body: { items: [DRIVE] } },
      '/api/v1/rips?limit=20': { status: 200, body: { items: [] } },
      'POST /api/v1/drives/d1/rips': { status: 201, body: rip({ state: 'queued' }) },
      '/api/v1/report-settings': SETTINGS,
    });
    render(RipsPage);
    expect(await screen.findByText('Nothing has been ripped yet.')).toBeTruthy();
    await fireEvent.click(await screen.findByRole('button', { name: 'Rip the disc in sr0' }));
    await vi.waitFor(() => expect(goto).toHaveBeenCalledWith('/rips/r1'));
    expect(asked).toContainEqual({ url: '/api/v1/drives/d1/rips', method: 'POST', body: '{}' });
  });

  it('says why a rip was refused', async () => {
    server({
      '/api/v1/drives': { status: 200, body: { items: [DRIVE] } },
      '/api/v1/rips?limit=20': { status: 200, body: { items: [rip()] } },
      'POST /api/v1/drives/d1/rips': {
        status: 409,
        body: {
          type: 'about:blank',
          title: 'Conflict',
          status: 409,
          code: 'CONFLICT',
          detail: 'this drive already has a rip that has not finished reading',
        },
      },
      '/api/v1/report-settings': SETTINGS,
    });
    render(RipsPage);
    expect(await screen.findByText('Reading the disc')).toBeTruthy();
    await fireEvent.click(await screen.findByRole('button', { name: 'Rip the disc in sr0' }));
    expect(
      await screen.findByText('this drive already has a rip that has not finished reading'),
    ).toBeTruthy();
    expect(goto).not.toHaveBeenCalled();
  });
});

describe('Rips page, on a drive that can decrypt', () => {
  function decryptingServer() {
    return server({
      '/api/v1/drives': { status: 200, body: { items: [DECRYPTING_DRIVE] } },
      '/api/v1/rips?limit=20': { status: 200, body: { items: [] } },
      'POST /api/v1/drives/d1/rips': { status: 201, body: rip({ state: 'queued' }) },
      '/api/v1/report-settings': SETTINGS,
    });
  }

  it('asks first, and rips without decryption unless the box is ticked', async () => {
    const asked = decryptingServer();
    render(RipsPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Rip the disc in sr0' }));
    expect(await screen.findByRole('heading', { name: 'Rip the disc in sr0' })).toBeTruthy();
    expect(screen.getByText(/is yours to determine/)).toBeTruthy();
    // The choice of original appears only once decryption is asked for.
    expect(screen.queryByRole('group', { name: 'What to keep' })).toBeNull();
    await fireEvent.click(screen.getByRole('button', { name: 'Start ripping' }));
    await vi.waitFor(() => expect(goto).toHaveBeenCalledWith('/rips/r1'));
    expect(asked).toContainEqual({ url: '/api/v1/drives/d1/rips', method: 'POST', body: '{}' });
  });

  it('keeps the encrypted image by default when decrypting', async () => {
    const asked = decryptingServer();
    render(RipsPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Rip the disc in sr0' }));
    await fireEvent.click(
      await screen.findByRole('checkbox', { name: 'Decrypt an encrypted disc' }),
    );
    const original = screen.getByRole('radio', {
      name: 'Keep the encrypted image, and a decrypted copy',
    }) as HTMLInputElement;
    expect(original.checked).toBe(true);
    await fireEvent.click(screen.getByRole('button', { name: 'Start ripping' }));
    await vi.waitFor(() => expect(goto).toHaveBeenCalledWith('/rips/r1'));
    expect(asked).toContainEqual({
      url: '/api/v1/drives/d1/rips',
      method: 'POST',
      body: '{"decrypt":true,"keep":"encrypted_original"}',
    });
  });

  it('can keep only a decrypted image', async () => {
    const asked = decryptingServer();
    render(RipsPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Rip the disc in sr0' }));
    await fireEvent.click(
      await screen.findByRole('checkbox', { name: 'Decrypt an encrypted disc' }),
    );
    await fireEvent.click(
      screen.getByRole('radio', {
        name: 'Keep only a decrypted image (not a faithful copy of the disc)',
      }),
    );
    await fireEvent.click(screen.getByRole('button', { name: 'Start ripping' }));
    await vi.waitFor(() => expect(goto).toHaveBeenCalledWith('/rips/r1'));
    expect(asked).toContainEqual({
      url: '/api/v1/drives/d1/rips',
      method: 'POST',
      body: '{"decrypt":true,"keep":"decrypted_original"}',
    });
  });

  it('shows why the server refused to decrypt', async () => {
    server({
      '/api/v1/drives': { status: 200, body: { items: [DECRYPTING_DRIVE] } },
      '/api/v1/rips?limit=20': { status: 200, body: { items: [] } },
      'POST /api/v1/drives/d1/rips': {
        status: 409,
        body: {
          type: 'about:blank',
          title: 'Conflict',
          status: 409,
          code: 'DECRYPTION_OFF',
          detail: 'decryption is turned off on this server',
        },
      },
      '/api/v1/report-settings': SETTINGS,
    });
    render(RipsPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Rip the disc in sr0' }));
    await fireEvent.click(
      await screen.findByRole('checkbox', { name: 'Decrypt an encrypted disc' }),
    );
    await fireEvent.click(screen.getByRole('button', { name: 'Start ripping' }));
    expect(await screen.findByText('decryption is turned off on this server')).toBeTruthy();
    expect(goto).not.toHaveBeenCalled();
  });

  it('withdraws the question without ripping', async () => {
    const asked = decryptingServer();
    render(RipsPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Rip the disc in sr0' }));
    await fireEvent.click(await screen.findByRole('button', { name: 'Cancel' }));
    expect(screen.queryByRole('heading', { name: 'Rip the disc in sr0' })).toBeNull();
    expect(asked.some((call) => call.method === 'POST')).toBe(false);
  });
});

describe('Rip page', () => {
  it('shows progress and withdraws a rip while it reads', async () => {
    const asked = server({
      '/api/v1/rips/r1': { status: 200, body: rip() },
      '/api/v1/rips/r1/events': {
        status: 200,
        body: {
          items: [
            {
              sequence: 0,
              event_type: 'progress',
              code: 'RIP_PROGRESS',
              progress: 0.4,
              message: 'read 4 of 10',
              worker_time: '2026-10-19T12:00:01Z',
            },
          ],
        },
      },
      'POST /api/v1/rips/r1/cancel': {
        status: 200,
        body: rip({ state: 'canceled', is_open: false, can_cancel: false }),
      },
      '/api/v1/report-settings': SETTINGS,
    });
    render(RipPage);
    expect(await screen.findByRole('heading', { name: 'Reading the disc' })).toBeTruthy();
    expect(await screen.findByText('40% read')).toBeTruthy();
    expect(screen.getByText('read 4 of 10')).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: 'Withdraw this rip' }));
    expect(await screen.findByRole('heading', { name: 'Withdrawn' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Withdraw this rip' })).toBeNull();
    expect(asked).toContainEqual({ url: '/api/v1/rips/r1/cancel', method: 'POST' });
  });

  it('links a finished rip to the library', async () => {
    server({
      '/api/v1/rips/r1': {
        status: 200,
        body: rip({
          state: 'complete',
          is_open: false,
          can_cancel: false,
          artifact_id: 'a1',
          engine: 'cdrdao',
          engine_version: '1.2.4',
          read_errors: { unreadable_sectors: 0, retried_sectors: 2 },
          files: [{ name: 'disc.bin', bytes: 1000, sha256: 'ab'.repeat(32) }],
          bytes_received: 1000,
        }),
      },
      '/api/v1/rips/r1/events': { status: 200, body: { items: [] } },
      '/api/v1/report-settings': SETTINGS,
    });
    render(RipPage);
    const link = await screen.findByRole('link', { name: 'Open it in the library' });
    expect(link.getAttribute('href')).toBe('/library/a1');
    expect(screen.getByText('cdrdao 1.2.4')).toBeTruthy();
    expect(screen.getByText(/0 unreadable, 2 retried/)).toBeTruthy();
  });

  it('explains a refusal in words', async () => {
    server({
      '/api/v1/rips/r1': {
        status: 200,
        body: rip({
          state: 'refused',
          is_open: false,
          can_cancel: false,
          error_code: 'BLANK_MEDIUM',
          error_detail: 'the CD-R in the drive is blank',
        }),
      },
      '/api/v1/rips/r1/events': { status: 200, body: { items: [] } },
      '/api/v1/report-settings': SETTINGS,
    });
    render(RipPage);
    expect(await screen.findByRole('heading', { name: 'Not ripped' })).toBeTruthy();
    expect(
      screen.getByText('The disc in the drive is blank; there is nothing on it to rip.'),
    ).toBeTruthy();
  });

  it('shows what was decrypted and links the decrypted copy', async () => {
    server({
      '/api/v1/rips/r1': {
        status: 200,
        body: rip({
          state: 'complete',
          is_open: false,
          can_cancel: false,
          decrypt: true,
          keep: 'encrypted_original',
          encryption: 'css',
          decryption: {
            system: 'css',
            library: 'libdvdcss',
            library_version: '1.6.0',
            tool: 'dvdbackup',
            tool_version: '0.4.2',
          },
          artifact_id: 'a1',
          copy_artifact_id: 'a2',
          engine: 'block_reader',
          engine_version: '0.1.0',
        }),
      },
      '/api/v1/rips/r1/events': { status: 200, body: { items: [] } },
      '/api/v1/report-settings': SETTINGS,
    });
    render(RipPage);
    const copy = await screen.findByRole('link', { name: 'Open the decrypted copy' });
    expect(copy.getAttribute('href')).toBe('/library/a2');
    expect(screen.getByText('Encrypted (CSS, a DVD)')).toBeTruthy();
    expect(screen.getByText('libdvdcss 1.6.0, through dvdbackup 0.4.2')).toBeTruthy();
    expect(
      screen.getByText('Asked for. Keep: The encrypted image, and a decrypted copy'),
    ).toBeTruthy();
    expect(screen.queryByText(/Only a decrypted image was kept/)).toBeNull();
  });

  it('says plainly when only a decrypted image was kept', async () => {
    server({
      '/api/v1/rips/r1': {
        status: 200,
        body: rip({
          state: 'complete',
          is_open: false,
          can_cancel: false,
          decrypt: true,
          keep: 'decrypted_original',
          encryption: 'css',
          decryption: {
            system: 'css',
            library: 'libdvdcss',
            library_version: '1.6.0',
            tool: 'dvdbackup',
            tool_version: '0.4.2',
          },
          artifact_id: 'a1',
        }),
      },
      '/api/v1/rips/r1/events': { status: 200, body: { items: [] } },
      '/api/v1/report-settings': SETTINGS,
    });
    render(RipPage);
    expect(await screen.findByText(/Only a decrypted image was kept/)).toBeTruthy();
    expect(screen.queryByRole('link', { name: 'Open the decrypted copy' })).toBeNull();
  });
});
