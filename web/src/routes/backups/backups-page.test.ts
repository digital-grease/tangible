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
    schedule: {
      enabled: true,
      time: '03:00',
      next_at: '2026-11-19T03:00:00Z',
      updated_by: null,
      updated_at: null,
    },
    last_restore: null,
    can_restore: true,
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
    expect(screen.getByText(/the same key/)).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);
  });

  it('warns first that the disc images are not in a copy, whatever the copies are', async () => {
    signIn('administrator');
    server({ 'GET /api/v1/backups': { status: 503, body: { code: 'BACKUPS_UNAVAILABLE' } } });
    const { container } = render(Page);
    const warning = screen.getByRole('region', {
      name: /Warning: your disc images are not in these copies/,
    });
    expect(within(warning).getByText(/configuration and metadata only/)).toBeTruthy();
    expect(within(warning).getByText(/Back up the library another way/)).toBeTruthy();
    // Above everything else on the page, before the copies have loaded or
    // when they cannot.
    expect(container.querySelector('.stack')?.firstElementChild).toBe(warning);
    await screen.findByText(/The copies could not be loaded/);
    expect(screen.getByRole('region', { name: /Warning: your disc images/ })).toBe(warning);
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

  it('saves the schedule an administrator sets', async () => {
    signIn('administrator');
    const { asked, bodies } = server({
      'GET /api/v1/backups': { status: 200, body: backups() },
      'PUT /api/v1/backups/schedule': {
        status: 200,
        body: {
          enabled: true,
          time: '04:30',
          next_at: '2026-11-19T04:30:00Z',
          updated_by: 'owner',
        },
      },
    });
    render(Page);
    expect(await screen.findByText('Every day at 03:00 UTC')).toBeTruthy();
    const time = screen.getByLabelText('At, in UTC') as HTMLInputElement;
    expect(time.value).toBe('03:00');
    await fireEvent.input(time, { target: { value: '04:30' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Save the schedule' }));
    expect(await screen.findByText('Every day at 04:30 UTC')).toBeTruthy();
    expect(screen.getByText('Saved.')).toBeTruthy();
    const sent = bodies[asked.indexOf('PUT /api/v1/backups/schedule')];
    expect(JSON.parse(String(sent))).toEqual({ enabled: true, time: '04:30' });
  });

  it('restores a copy once its name is typed, and follows it until the server is back', async () => {
    signIn('administrator');
    let polled = 0;
    const { asked, bodies } = server({
      'GET /api/v1/backups': { status: 200, body: backups() },
      'POST /api/v1/backups/tangible-20261117T210000Z.dump/restore': {
        status: 202,
        body: { step: 'stopping', step_number: 1, steps: 7, started_at: '2026-11-18T05:00:00Z' },
      },
      'GET /api/v1/backups/restore': () => {
        polled += 1;
        return polled === 1
          ? {
              status: 200,
              body: {
                restoring: true,
                progress: {
                  step: 'rows',
                  step_number: 4,
                  steps: 7,
                  started_at: '2026-11-18T05:00:00Z',
                },
              },
            }
          : { status: 200, body: { restoring: false, progress: null } };
      },
    });
    render(Page);
    await fireEvent.click(
      await screen.findByRole('button', { name: 'Restore tangible-20261117T210000Z.dump' }),
    );
    const restoreButton = screen.getByRole('button', {
      name: 'Restore from this copy',
    }) as HTMLButtonElement;
    expect(restoreButton.disabled).toBe(true);
    expect(screen.getByText(/Everything since .* goes from the catalogue/)).toBeTruthy();
    await fireEvent.input(screen.getByLabelText("Type the copy's name to confirm"), {
      target: { value: 'tangible-20261117T210000Z.dump' },
    });
    expect(restoreButton.disabled).toBe(false);
    await fireEvent.click(restoreButton);
    expect(await screen.findByText(/Step 1 of 7: Letting the background work finish/)).toBeTruthy();
    const sent =
      bodies[asked.indexOf('POST /api/v1/backups/tangible-20261117T210000Z.dump/restore')];
    expect(JSON.parse(String(sent))).toEqual({ confirm: 'tangible-20261117T210000Z.dump' });
    expect(
      await screen.findByText(/Step 4 of 7: Loading the copy's rows/, undefined, { timeout: 4000 }),
    ).toBeTruthy();
    expect(
      await screen.findByText('The server has started again.', undefined, { timeout: 4000 }),
    ).toBeTruthy();
    expect(screen.getByRole('link', { name: 'Sign in again' }).getAttribute('href')).toBe('/login');
  });

  it('shows the last restore, what its check found, and catalogues what the library had', async () => {
    signIn('administrator');
    let catalogued = false;
    const report = (registered: number) => ({
      missing_objects: { count: 0, listed: [] },
      wrong_size: { count: 0, listed: [] },
      missing_manifests: { count: 0, listed: [] },
      uncatalogued: { count: 2, listed: [] },
      registered,
      unfinished: [],
    });
    const { asked } = server({
      'GET /api/v1/backups': () => ({
        status: 200,
        body: backups({
          last_restore: {
            id: 'r1',
            requested_by: 'owner',
            copy_name: 'tangible-20261117T210000Z.dump',
            safety_copy: 'tangible-20261118T050000Z.dump',
            outcome: 'restored',
            error: null,
            report: report(catalogued ? 2 : 0),
            started_at: '2026-11-18T05:00:00Z',
            finished_at: '2026-11-18T05:02:00Z',
          },
        }),
      }),
      'POST /api/v1/backups/register-missing': () => {
        catalogued = true;
        return { status: 200, body: { uncatalogued: 2, registered: 2, report: report(2) } };
      },
    });
    render(Page);
    expect(await screen.findByText(/Restored from tangible-20261117T210000Z.dump/)).toBeTruthy();
    expect(screen.getByText('tangible-20261118T050000Z.dump', { selector: 'code' })).toBeTruthy();
    expect(screen.getByText(/2 artifacts were imported after the copy was taken/)).toBeTruthy();
    await fireEvent.click(
      screen.getByRole('button', { name: 'Catalogue them from their manifests' }),
    );
    expect(await screen.findByText(/Catalogued 2 of the 2/)).toBeTruthy();
    expect(asked).toContain('POST /api/v1/backups/register-missing');
    expect(await screen.findByText('The catalogue and the library agree.')).toBeTruthy();
  });

  it('says a restore that failed left the database as it was', async () => {
    signIn('administrator');
    server({
      'GET /api/v1/backups': {
        status: 200,
        body: backups({
          last_restore: {
            id: 'r2',
            requested_by: 'owner',
            copy_name: 'tangible-20261117T210000Z.dump',
            safety_copy: 'tangible-20261118T050000Z.dump',
            outcome: 'failed',
            error: "loading the copy's rows failed: disk full",
            report: null,
            started_at: '2026-11-18T05:00:00Z',
            finished_at: '2026-11-18T05:01:00Z',
          },
        }),
      },
    });
    render(Page);
    expect(
      await screen.findByText(/failed .*: loading the copy's rows failed: disk full/),
    ).toBeTruthy();
    expect(
      screen.getByText('The database is as it was before; nothing was replaced.'),
    ).toBeTruthy();
  });

  it('offers no restore where the server cannot restore', async () => {
    signIn('administrator');
    server({ 'GET /api/v1/backups': { status: 200, body: backups({ can_restore: false }) } });
    render(Page);
    expect(
      await screen.findByRole('button', { name: 'Remove tangible-20261117T210000Z.dump' }),
    ).toBeTruthy();
    expect(
      screen.queryByRole('button', { name: 'Restore tangible-20261117T210000Z.dump' }),
    ).toBeNull();
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
