// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen, within } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { forgetReportSettings } from '$lib/report';
import { rememberSession } from '$lib/session';
import { axeViolations } from '$lib/testing/axe';

const { default: Page } = await import('./+page.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
  forgetReportSettings();
  rememberSession(null);
});

type Reply = { status: number; body: unknown };

/** The server, by `METHOD url`; records what was asked, and with what body. */
function server(routes: Record<string, Reply | (() => Reply)>): {
  asked: string[];
  bodies: unknown[];
} {
  const asked: string[] = [];
  const bodies: unknown[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const key = `${init?.method ?? 'GET'} ${String(input)}`;
      asked.push(key);
      bodies.push(init?.body);
      const route = routes[key];
      const reply = route ? (typeof route === 'function' ? route() : route) : null;
      const { status, body } = reply ?? { status: 404, body: {} };
      return {
        ok: status >= 200 && status < 300,
        status,
        json: async () => body,
        headers: new Headers(),
      } as Response;
    }),
  );
  return { asked, bodies };
}

function copy(extra: Record<string, unknown> = {}) {
  return {
    name: 'tangible-20261117T210000Z.dump',
    kind: 'taken',
    taken_at: '2026-11-17T21:00:00Z',
    size_bytes: 2_400_000,
    ...extra,
  };
}

function backups(extra: Record<string, unknown> = {}) {
  return {
    copies: [copy()],
    kept: 10,
    writable: true,
    tools: { ready: true, tools_version: 17, database_version: 17, problem: null },
    operation: null,
    last_outcome: null,
    max_upload_bytes: 4 * 1024 * 1024 * 1024,
    ...extra,
  };
}

function signIn(role: string) {
  rememberSession({ role } as Parameters<typeof rememberSession>[0]);
}

describe('Backups page', () => {
  it('lists the copies, each to download or remove, and has no accessibility violations', async () => {
    signIn('administrator');
    server({
      'GET /api/v1/backups': {
        status: 200,
        body: backups({
          copies: [
            copy(),
            copy({
              name: 'from-the-old-machine.dump',
              kind: 'added',
              taken_at: '2026-10-01T00:00:00Z',
            }),
          ],
        }),
      },
    });
    const { container } = render(Page);
    const table = await screen.findByRole('table', { name: 'Copies of the database' });
    expect(
      within(table).getByText('tangible-20261117T210000Z.dump', { selector: 'code' }),
    ).toBeTruthy();
    expect(
      within(table).getByText(/Added at the machine; never removed to make room/),
    ).toBeTruthy();
    const download = within(table).getAllByRole('link', {
      name: /Download/,
    })[0] as HTMLAnchorElement;
    expect(download.getAttribute('href')).toBe(
      '/api/v1/backups/tangible-20261117T210000Z.dump/download',
    );
    expect(screen.getByText('The newest 10 copies Tangible takes are kept')).toBeTruthy();
    expect(screen.getByText(/not in it either/)).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);
  });

  it('takes a copy, and follows it until it is kept', async () => {
    signIn('administrator');
    let taking = false;
    const { asked } = server({
      'GET /api/v1/backups': () => ({
        status: 200,
        body: taking
          ? backups({
              operation: { kind: 'taking', by: 'owner', started_at: '2026-11-18T03:00:00Z' },
            })
          : backups({
              last_outcome: {
                finished_at: '2026-11-18T03:00:05Z',
                name: 'tangible-20261118T030000Z.dump',
                error: null,
              },
            }),
      }),
      'POST /api/v1/backups': () => {
        taking = true;
        return {
          status: 202,
          body: backups({
            operation: { kind: 'taking', by: 'owner', started_at: '2026-11-18T03:00:00Z' },
          }),
        };
      },
    });
    render(Page);
    await fireEvent.click(await screen.findByRole('button', { name: 'Take a copy now' }));
    expect(await screen.findByText('Taking a copy, asked for by owner')).toBeTruthy();
    expect(asked).toContain('POST /api/v1/backups');
    expect(
      (screen.getByRole('button', { name: 'Take a copy now' }) as HTMLButtonElement).disabled,
    ).toBe(true);
    taking = false;
    expect(
      await screen.findByText(/tangible-20261118T030000Z.dump was kept/, undefined, {
        timeout: 4000,
      }),
    ).toBeTruthy();
  });

  it('says why copies cannot be taken, and offers no button that would fail', async () => {
    signIn('administrator');
    server({
      'GET /api/v1/backups': {
        status: 200,
        body: backups({
          tools: {
            ready: false,
            tools_version: 17,
            database_version: 18,
            problem:
              "the database is PostgreSQL 18, newer than this image's tools (17), which cannot copy it",
          },
        }),
      },
    });
    render(Page);
    expect(await screen.findByText('Copies cannot be taken')).toBeTruthy();
    expect(screen.getByText(/PostgreSQL 18, newer than this image's tools/)).toBeTruthy();
    expect(
      (screen.getByRole('button', { name: 'Take a copy now' }) as HTMLButtonElement).disabled,
    ).toBe(true);
  });

  it('uploads a copy as the request body, and shows what the server refused', async () => {
    signIn('administrator');
    let accepted = false;
    const { bodies, asked } = server({
      'GET /api/v1/backups': {
        status: 200,
        body: backups(),
      },
      'PUT /api/v1/backups/uploads': () =>
        accepted
          ? {
              status: 201,
              body: copy({ name: 'uploaded-20261118T040000Z.dump', kind: 'uploaded' }),
            }
          : {
              status: 422,
              body: {
                type: 'about:blank',
                title: 'Not a copy of a database',
                status: 422,
                code: 'BACKUP_NOT_A_COPY',
                detail: 'it is not a PostgreSQL custom-format archive',
              },
            },
    });
    render(Page);
    const input = (await screen.findByLabelText(
      'A copy of a Tangible database',
    )) as HTMLInputElement;
    const file = new File(['-- plain SQL'], 'dump.sql');
    await fireEvent.change(input, { target: { files: [file] } });
    await fireEvent.click(screen.getByRole('button', { name: 'Upload' }));
    expect(await screen.findByText(/not a PostgreSQL custom-format archive/)).toBeTruthy();
    expect(bodies[asked.indexOf('PUT /api/v1/backups/uploads')]).toBe(file);
    accepted = true;
    await fireEvent.click(screen.getByRole('button', { name: 'Upload' }));
    expect(await screen.findByText(/Kept uploaded-20261118T040000Z.dump/)).toBeTruthy();
  });

  it('removes a copy only once asked twice', async () => {
    signIn('administrator');
    let removed = false;
    const { asked } = server({
      'GET /api/v1/backups': () => ({
        status: 200,
        body: backups({ copies: removed ? [] : [copy()] }),
      }),
      'DELETE /api/v1/backups/tangible-20261117T210000Z.dump': () => {
        removed = true;
        return { status: 204, body: null };
      },
    });
    render(Page);
    await fireEvent.click(
      await screen.findByRole('button', { name: 'Remove tangible-20261117T210000Z.dump' }),
    );
    expect(asked).not.toContain('DELETE /api/v1/backups/tangible-20261117T210000Z.dump');
    await fireEvent.click(screen.getByRole('button', { name: 'Yes, remove it' }));
    expect(await screen.findByText('No copies yet')).toBeTruthy();
    expect(asked).toContain('DELETE /api/v1/backups/tangible-20261117T210000Z.dump');
  });
});
