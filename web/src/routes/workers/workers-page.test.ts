// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { axeViolations } from '$lib/testing/axe';

const { default: WorkersPage } = await import('./+page.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
});

type Reply = { status: number; body: unknown };

/** Answer by method and path; record what was asked. */
function server(routes: Record<string, Reply>): string[] {
  const asked: string[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const key = `${init?.method ?? 'GET'} ${String(input)}`;
      asked.push(key);
      const reply = routes[key] ?? { status: 404, body: {} };
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

function worker(status: string) {
  return {
    id: 'w1',
    name: 'bench',
    status,
    software_version: '0.1.0',
    protocol_version: '1alpha1',
    last_seen_at: '2026-10-28T12:00:00Z',
    created_at: '2026-10-01T12:00:00Z',
    revoked_at: null,
    credential_issued_at: new Date().toISOString(),
    credential_rotation_requested_at: null as string | null,
    credential_rotation_due: false,
    drives: [{ id: 'd1', name: 'sr0' }],
  };
}

function page(status: string, extra: Record<string, Reply> = {}) {
  return server({
    'GET /api/v1/workers?limit=200': {
      status: 200,
      body: { items: [worker(status)], next_cursor: null },
    },
    'GET /api/v1/drives': { status: 200, body: { items: [] } },
    'GET /api/v1/erasures?limit=20': { status: 200, body: { items: [] } },
    ...extra,
  });
}

function drive(activity: unknown = null) {
  return {
    id: 'd1',
    name: 'sr0',
    device: '/dev/sr0',
    worker_id: 'w1',
    worker_name: 'bench',
    worker_status: 'online',
    worker_last_seen_at: '2026-10-28T12:00:00Z',
    vendor: 'PIONEER',
    model: 'BDR-212',
    firmware: '1.10',
    connection: 'sata',
    activity,
  };
}

describe('Workers page', () => {
  it('passes the accessibility checks with workers, drives and erasures shown', async () => {
    server({
      'GET /api/v1/workers?limit=200': {
        status: 200,
        body: { items: [worker('online')], next_cursor: null },
      },
      'GET /api/v1/drives': { status: 200, body: { items: [drive()] } },
      'GET /api/v1/erasures?limit=20': {
        status: 200,
        body: {
          items: [
            {
              id: 'e1',
              drive_id: 'd1',
              drive_name: 'sr0',
              mode: 'quick',
              state: 'refused',
              is_open: false,
              requested_by: 'admin',
              created_at: '2026-10-28T12:00:00Z',
              error_detail: 'The disc is write-once.',
              medium_before: { profile: 'CD-R', blank: false, sessions: 1 },
            },
          ],
        },
      },
    });
    const { container } = render(WorkersPage);
    await screen.findByRole('rowheader', { name: 'bench' });
    await screen.findByText('Not erased');
    expect(await axeViolations(container)).toEqual([]);
  });

  it('shows what a drive is doing, linked to it', async () => {
    server({
      'GET /api/v1/workers?limit=200': {
        status: 200,
        body: { items: [worker('online')], next_cursor: null },
      },
      'GET /api/v1/drives': {
        status: 200,
        body: {
          items: [
            drive({
              kind: 'burn',
              id: 'j1',
              state: 'writing',
              title: 'Homebrew compilation',
              disc_number: 1,
              progress: 0.4,
              medium: null,
              disc_shape: null,
              started_at: '2026-10-28T12:00:00Z',
            }),
          ],
        },
      },
      'GET /api/v1/erasures?limit=20': { status: 200, body: { items: [] } },
    });
    render(WorkersPage);
    const link = await screen.findByRole('link', { name: 'Homebrew compilation, disc 1' });
    expect(link.getAttribute('href')).toBe('/burns/j1');
    expect(screen.getByText('PIONEER BDR-212, firmware 1.10')).toBeTruthy();
  });

  it('erases only once the loss is understood, taking the keyboard to the question', async () => {
    const asked = server({
      'GET /api/v1/workers?limit=200': {
        status: 200,
        body: { items: [worker('online')], next_cursor: null },
      },
      'GET /api/v1/drives': { status: 200, body: { items: [drive()] } },
      'GET /api/v1/erasures?limit=20': { status: 200, body: { items: [] } },
      'POST /api/v1/drives/d1/erasures': {
        status: 202,
        body: { id: 'e2', state: 'queued', is_open: true },
      },
    });
    const { container } = render(WorkersPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Erase the disc in sr0…' }));
    const heading = screen.getByRole('heading', { name: 'Erase the disc in sr0' });
    await vi.waitFor(() => expect(document.activeElement).toBe(heading));
    const submit = screen.getByRole('button', { name: 'Erase the disc in sr0' });
    expect((submit as HTMLButtonElement).disabled).toBe(true);
    expect(await axeViolations(container)).toEqual([]);

    await fireEvent.click(
      screen.getByRole('checkbox', {
        name: 'I understand everything on the disc in sr0 will be destroyed',
      }),
    );
    expect((submit as HTMLButtonElement).disabled).toBe(false);
    await fireEvent.click(submit);
    await vi.waitFor(() => expect(asked).toContain('POST /api/v1/drives/d1/erasures'));
  });

  it('lists workers in words and drains one when asked', async () => {
    const asked = page('online', {
      'POST /api/v1/workers/w1/drain': { status: 200, body: worker('draining') },
    });
    render(WorkersPage);
    expect(await screen.findByRole('rowheader', { name: 'bench' })).toBeTruthy();
    expect(screen.getByText('Taking work')).toBeTruthy();
    expect(screen.getByText('sr0')).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Enable bench' })).toBeNull();
    await fireEvent.click(screen.getByRole('button', { name: 'Drain bench' }));
    await vi.waitFor(() => expect(asked).toContain('POST /api/v1/workers/w1/drain'));
  });

  it('offers to enable a draining worker', async () => {
    page('draining');
    render(WorkersPage);
    expect(await screen.findByRole('button', { name: 'Enable bench' })).toBeTruthy();
    expect(screen.getByText(/taking nothing new/)).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Drain bench' })).toBeNull();
  });

  it('revokes only once it is confirmed, saying what it does', async () => {
    const asked = page('online', {
      'POST /api/v1/workers/w1/revoke': { status: 200, body: worker('revoked') },
    });
    render(WorkersPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Revoke bench…' }));
    expect(screen.getByText(/refused from its next request, for good/)).toBeTruthy();
    await vi.waitFor(() =>
      expect(document.activeElement).toBe(screen.getByRole('heading', { name: 'Revoke bench' })),
    );
    expect(asked).not.toContain('POST /api/v1/workers/w1/revoke');
    await fireEvent.click(screen.getByRole('button', { name: 'Keep bench' }));
    expect(screen.queryByText(/refused from its next request/)).toBeNull();

    await fireEvent.click(screen.getByRole('button', { name: 'Revoke bench…' }));
    await fireEvent.click(screen.getByRole('button', { name: 'Revoke bench' }));
    await vi.waitFor(() => expect(asked).toContain('POST /api/v1/workers/w1/revoke'));
  });

  it('asks a worker to rotate its credential, then says it is asked', async () => {
    const asked = page('online', {
      'POST /api/v1/workers/w1/rotate-credential': {
        status: 200,
        body: { ...worker('online'), credential_rotation_requested_at: '2026-11-02T12:00:00Z' },
      },
    });
    render(WorkersPage);
    expect(await screen.findByText('Issued today')).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: "Rotate bench's credential" }));
    await vi.waitFor(() => expect(asked).toContain('POST /api/v1/workers/w1/rotate-credential'));
  });

  it('shows a rotation already asked for and does not offer another', async () => {
    server({
      'GET /api/v1/workers?limit=200': {
        status: 200,
        body: {
          items: [
            { ...worker('online'), credential_rotation_requested_at: '2026-11-02T12:00:00Z' },
          ],
          next_cursor: null,
        },
      },
      'GET /api/v1/drives': { status: 200, body: { items: [] } },
      'GET /api/v1/erasures?limit=20': { status: 200, body: { items: [] } },
    });
    render(WorkersPage);
    expect(await screen.findByText(/^Rotation asked for/)).toBeTruthy();
    expect(screen.queryByRole('button', { name: "Rotate bench's credential" })).toBeNull();
  });

  it('says only an administrator can change workers', async () => {
    page('online', {
      'POST /api/v1/workers/w1/drain': {
        status: 403,
        body: {
          type: 'about:blank',
          title: 'Forbidden',
          status: 403,
          code: 'FORBIDDEN',
          detail: 'the operator role does not allow manage_workers',
        },
      },
    });
    render(WorkersPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Drain bench' }));
    expect(await screen.findByText('Only an administrator can change workers.')).toBeTruthy();
  });
});
