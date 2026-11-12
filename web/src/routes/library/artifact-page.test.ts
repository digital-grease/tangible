// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render, screen, within } from '@testing-library/svelte';
import { axeViolations } from '$lib/testing/axe';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { forgetReportSettings } from '$lib/report';
import { rememberSession } from '$lib/session';

vi.mock('$app/state', () => ({ page: { params: { id: 'a1' } } }));

const { default: ArtifactPage } = await import('./[id]/+page.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
  forgetReportSettings();
  rememberSession(null);
});

type Reply = { status: number; body: unknown };

/** The server: the artifact, its lineage, and any other routes by `METHOD url`. */
function serve(artifact: unknown, routes: Record<string, Reply | (() => Reply)> = {}): string[] {
  const asked: string[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const url = String(input);
      const key = `${init?.method ?? 'GET'} ${url}`;
      asked.push(key);
      const route = routes[key];
      const reply: Reply = route
        ? typeof route === 'function'
          ? route()
          : route
        : url === '/api/v1/artifacts/a1'
          ? { status: 200, body: artifact }
          : url === '/api/v1/artifacts/a1/lineage'
            ? { status: 200, body: { derivatives: [], jobs: [] } }
            : { status: 404, body: {} };
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

function artifact(extra: Record<string, unknown> = {}) {
  return {
    id: 'a1',
    format: 'video_ts_directory',
    artifact_kind: 'directory_tree',
    validation_state: 'valid',
    component_count: 1,
    total_bytes: 2048,
    source_filename: 'VIDEO_TS',
    has_warnings: false,
    format_confidence: 1,
    source_kind: 'physical_drive',
    integrity_state: 'unchecked',
    warnings: [],
    components: [],
    ...extra,
  };
}

const RIPPED = {
  rip_id: 'r1',
  drive_vendor: 'Slimtype',
  drive_model: 'DVD A DS8A8SH',
  drive_firmware: 'KS21',
  drive_connection: 'usb',
  engine: 'dvdbackup',
  engine_version: '0.4.2',
  read_errors: null,
  encryption: 'css',
  decrypted_at_acquisition: true,
  decryption: {
    system: 'css',
    library: 'libdvdcss',
    library_version: '1.6.0',
    tool: 'dvdbackup',
    tool_version: '0.4.2',
  },
};

describe('Artifact page', () => {
  it('says a decrypted original is not a faithful copy of the disc', async () => {
    serve(artifact({ ripped: RIPPED }));
    render(ArtifactPage);
    expect(await screen.findByRole('heading', { name: 'Ripped from a disc' })).toBeTruthy();
    expect(screen.getByText('Decrypted when ripped.')).toBeTruthy();
    expect(screen.getByText('Slimtype DVD A DS8A8SH, firmware KS21, USB')).toBeTruthy();
    expect(screen.getByText('Encrypted (CSS, a DVD)')).toBeTruthy();
    expect(screen.getByText('libdvdcss 1.6.0, through dvdbackup 0.4.2')).toBeTruthy();
    const link = screen.getByRole('link', { name: 'The rip that read it' });
    expect(link.getAttribute('href')).toBe('/rips/r1');
  });

  it('does not call an encrypted original decrypted', async () => {
    serve(
      artifact({
        format: 'iso',
        ripped: {
          ...RIPPED,
          engine: 'block_reader',
          decrypted_at_acquisition: false,
          decryption: null,
          read_errors: { unreadable_sectors: 0, retried_sectors: 3 },
        },
      }),
    );
    render(ArtifactPage);
    expect(await screen.findByRole('heading', { name: 'Ripped from a disc' })).toBeTruthy();
    expect(screen.queryByText('Decrypted when ripped.')).toBeNull();
    expect(screen.queryByText(/through dvdbackup/)).toBeNull();
    expect(screen.getByText(/0 unreadable, 3 retried/)).toBeTruthy();
  });

  it('shows the offsets of a CD read by redumper', async () => {
    serve(
      artifact({
        format: 'cue_bin',
        ripped: {
          ...RIPPED,
          engine: 'redumper',
          engine_version: 'b768',
          encryption: 'none',
          decrypted_at_acquisition: false,
          decryption: null,
          cd_read: { read_offset: 6, write_offset: -6, generic_drive: true, retried_samples: 0 },
        },
      }),
    );
    render(ArtifactPage);
    expect(await screen.findByText('Corrected (+6 samples)')).toBeTruthy();
    expect(screen.getByText('-6 samples')).toBeTruthy();
    expect(screen.getByText(/If its rips go wrong, ask for cdrdao/)).toBeTruthy();
    expect(screen.getByText('None: every sample read cleanly the first time')).toBeTruthy();
  });

  it('says nothing about offsets for a DVD', async () => {
    serve(artifact({ format: 'iso', ripped: { ...RIPPED, engine: 'block_reader' } }));
    render(ArtifactPage);
    expect(await screen.findByRole('heading', { name: 'Ripped from a disc' })).toBeTruthy();
    expect(screen.queryByText('Read offset')).toBeNull();
  });

  it('says nothing about ripping for an upload', async () => {
    serve(artifact({ source_kind: 'upload' }));
    render(ArtifactPage);
    expect(await screen.findByRole('heading', { name: 'Summary' })).toBeTruthy();
    expect(screen.queryByRole('heading', { name: 'Ripped from a disc' })).toBeNull();
  });
});

function integrityCheck(extra: Record<string, unknown> = {}) {
  return {
    id: 'c1',
    scope: 'artifact',
    artifact_id: 'a1',
    state: 'queued',
    objects_total: null,
    bytes_total: null,
    objects_checked: 0,
    bytes_checked: 0,
    problems: [],
    problem_count: 0,
    created_by: 'alice',
    created_at: '2026-10-25T12:00:00Z',
    ...extra,
  };
}

describe('Artifact integrity', () => {
  it('says damage plainly, and that nothing was repaired', async () => {
    serve(artifact({ integrity_state: 'damaged', integrity_checked_at: '2026-10-25T12:00:00Z' }));
    render(ArtifactPage);
    expect(await screen.findByRole('heading', { name: 'Integrity' })).toBeTruthy();
    expect(screen.getByText(/Damaged: a stored file is missing or reads back wrong/)).toBeTruthy();
    expect(screen.getByText('Nothing was repaired.')).toBeTruthy();
  });

  it('verifies on request and shows what the check found', async () => {
    const asked = serve(artifact(), {
      'POST /api/v1/artifacts/a1/validate': {
        status: 200,
        body: integrityCheck({
          state: 'complete',
          objects_total: 1,
          bytes_total: 2048,
          objects_checked: 1,
          bytes_checked: 2048,
          problem_count: 1,
          problems: [{ kind: 'corrupt', sha256: 'ab'.repeat(32), detail: 'reads back as another' }],
        }),
      },
    });
    render(ArtifactPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Verify now' }));
    expect(await screen.findByText('Done: 1 problem.')).toBeTruthy();
    expect(screen.getByText(/Reads back as other bytes/)).toBeTruthy();
    expect(screen.getByText('reads back as another')).toBeTruthy();
    // The artifact is loaded again, to show what the check marked.
    expect(asked.filter((key) => key === 'GET /api/v1/artifacts/a1')).toHaveLength(2);
  });

  it('follows a queued check to its end', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    let polls = 0;
    serve(artifact(), {
      'POST /api/v1/artifacts/a1/validate': { status: 202, body: integrityCheck() },
      'GET /api/v1/integrity-checks/c1': () => {
        polls += 1;
        return {
          status: 200,
          body:
            polls === 1
              ? integrityCheck({
                  state: 'running',
                  objects_total: 2,
                  bytes_total: 4096,
                  objects_checked: 1,
                  bytes_checked: 2048,
                })
              : integrityCheck({ state: 'complete', objects_total: 2, bytes_total: 4096 }),
        };
      },
    });
    render(ArtifactPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Verify now' }));
    expect(await screen.findByText('Waiting for its turn.')).toBeTruthy();
    await vi.advanceTimersByTimeAsync(3000);
    expect(await screen.findByText(/1 of 2 files/)).toBeTruthy();
    expect(screen.getByRole('progressbar').getAttribute('aria-valuenow')).toBe('50');
    await vi.advanceTimersByTimeAsync(3000);
    expect(await screen.findByText('Done: nothing wrong.')).toBeTruthy();
  });

  it('offers a viewer no button', async () => {
    rememberSession({ role: 'viewer' } as Parameters<typeof rememberSession>[0]);
    serve(artifact());
    render(ArtifactPage);
    expect(await screen.findByRole('heading', { name: 'Integrity' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Verify now' })).toBeNull();
  });
});

describe('Deleting an artifact', () => {
  const PENDING = {
    state: 'pending',
    requested_at: '2026-11-02T12:00:00Z',
    requested_by: 'admin',
    delete_after: '2026-11-09T12:00:00Z',
    deleted_at: null,
  };

  it('asks for an original only once it is confirmed, saying what happens', async () => {
    const asked = serve(artifact({ origin: 'imported_original', deletion: null }), {
      'POST /api/v1/artifacts/a1/deletion-requests': { status: 202, body: PENDING },
    });
    const { container } = render(ArtifactPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Delete this artifact…' }));
    expect(screen.getByText(/cannot be brought back/)).toBeTruthy();
    // The keyboard is taken to the question.
    await vi.waitFor(() =>
      expect(document.activeElement).toBe(screen.getByRole('heading', { name: 'Delete VIDEO_TS' })),
    );
    expect(await axeViolations(container)).toEqual([]);
    const submit = screen.getByRole('button', { name: 'Delete VIDEO_TS' });
    expect((submit as HTMLButtonElement).disabled).toBe(true);
    await fireEvent.click(screen.getByLabelText(/this is an original/));
    expect((submit as HTMLButtonElement).disabled).toBe(false);
    await fireEvent.click(submit);
    await vi.waitFor(() => expect(asked).toContain('POST /api/v1/artifacts/a1/deletion-requests'));
  });

  it('asks for a derivative without the original confirmation', async () => {
    serve(artifact({ origin: 'derived', deletion: null }));
    render(ArtifactPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Delete this artifact…' }));
    expect(screen.queryByLabelText(/this is an original/)).toBeNull();
    expect(
      (screen.getByRole('button', { name: 'Delete VIDEO_TS' }) as HTMLButtonElement).disabled,
    ).toBe(false);
  });

  it('says when a pending artifact goes, and keeps it on request', async () => {
    const asked = serve(artifact({ origin: 'imported_original', deletion: PENDING }), {
      'DELETE /api/v1/artifacts/a1/deletion-requests': { status: 204, body: null },
    });
    render(ArtifactPage);
    expect(await screen.findByRole('heading', { name: 'Pending deletion' })).toBeTruthy();
    expect(screen.getByText(/asked for by admin/i)).toBeTruthy();
    // Nothing offers to burn it, and the Burning tab says why.
    expect(screen.queryByRole('link', { name: 'Create burn job' })).toBeNull();
    await fireEvent.click(screen.getByRole('tab', { name: 'Burning' }));
    expect(screen.getByText(/Not burned while it is pending deletion/)).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: 'Keep VIDEO_TS' }));
    await vi.waitFor(() =>
      expect(asked).toContain('DELETE /api/v1/artifacts/a1/deletion-requests'),
    );
  });

  it('shows a deleted artifact as a record, with nothing to download or check', async () => {
    serve(
      artifact({
        origin: 'imported_original',
        components: [
          {
            id: 'c1',
            logical_path: 'VIDEO_TS/VIDEO_TS.IFO',
            role: 'primary_image',
            length_bytes: 2048,
            sha256: 'a'.repeat(64),
            ordinal: 0,
          },
        ],
        deletion: {
          ...PENDING,
          state: 'deleted',
          delete_after: null,
          deleted_at: '2026-11-09T12:05:00Z',
        },
      }),
    );
    render(ArtifactPage);
    expect(await screen.findByRole('heading', { name: 'Deleted' })).toBeTruthy();
    expect(screen.getByText(/Its bytes are gone/)).toBeTruthy();
    await fireEvent.click(screen.getByRole('tab', { name: /Components/ }));
    expect(screen.getByText('Removed')).toBeTruthy();
    expect(screen.queryByRole('link', { name: /Download VIDEO_TS\/VIDEO_TS.IFO/ })).toBeNull();
    expect(screen.queryByRole('button', { name: /Verify/ })).toBeNull();
    expect(screen.queryByRole('link', { name: 'Create burn job' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'Delete this artifact…' })).toBeNull();
  });

  it('says only an administrator can delete', async () => {
    serve(artifact({ origin: 'derived', deletion: null }), {
      'POST /api/v1/artifacts/a1/deletion-requests': {
        status: 403,
        body: {
          type: 'about:blank',
          title: 'Forbidden',
          status: 403,
          code: 'FORBIDDEN',
          detail: 'the operator role does not allow delete_artifacts',
        },
      },
    });
    render(ArtifactPage);
    await fireEvent.click(await screen.findByRole('button', { name: 'Delete this artifact…' }));
    await fireEvent.click(screen.getByRole('button', { name: 'Delete VIDEO_TS' }));
    expect(await screen.findByText('Only an administrator can delete artifacts.')).toBeTruthy();
  });
});

describe('An artifact unpacked from an archive', () => {
  it('says what it came in, and that the archive is not kept', async () => {
    serve(
      artifact({
        origin: 'imported_original',
        deletion: null,
        unpacked_from: {
          format: '7z',
          sha256: 'b'.repeat(64),
          length_bytes: 520,
          files: 2,
          notes: ["macOS's own files (__MACOSX, .DS_Store) were left out"],
        },
      }),
    );
    render(ArtifactPage);
    expect(await screen.findByText('Unpacked from')).toBeTruthy();
    expect(screen.getByText(/A 7z archive of/)).toBeTruthy();
    expect(screen.getByText(/The archive itself is not kept/)).toBeTruthy();
    expect(screen.getByText(/__MACOSX/)).toBeTruthy();
  });

  it('says nothing of an archive for an artifact that was not in one', async () => {
    serve(artifact({ origin: 'imported_original', deletion: null, unpacked_from: null }));
    render(ArtifactPage);
    expect(await screen.findByRole('heading', { name: 'Summary' })).toBeTruthy();
    expect(screen.queryByText('Unpacked from')).toBeNull();
  });
});

describe('Artifact page, in tabs', () => {
  it('heads the page with what it is, and lays its disc out to scale', async () => {
    serve(
      artifact({
        origin: 'imported_original',
        format: 'cue_bin',
        disc_shape: {
          media_family: 'cd',
          sector_count: 200_000,
          tracks: [
            { session: 1, sector_count: 100_000, is_audio: false },
            { session: 1, sector_count: 100_000, is_audio: true },
          ],
        },
      }),
    );
    const { container } = render(ArtifactPage);
    expect(await screen.findByText('Original, immutable')).toBeTruthy();
    expect(screen.getByRole('navigation', { name: 'Breadcrumb' })).toBeTruthy();
    const tabs = screen.getByRole('tablist', { name: 'About this artifact' });
    expect(
      within(tabs)
        .getAllByRole('tab')
        .map((tab) => tab.textContent?.replace(/\d+$/, '')),
    ).toEqual(['Summary', 'Components', 'Topology', 'Validation', 'Lineage', 'Burning']);
    expect(await axeViolations(container)).toEqual([]);

    await fireEvent.click(screen.getByRole('tab', { name: 'Topology' }));
    const panel = screen.getByRole('tabpanel');
    expect(panel.querySelectorAll('circle.ring')).toHaveLength(2);
    expect(within(panel).getByText(/CD, 200,000\s+sectors/)).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);
  });

  it('offers to burn from the header and the Burning tab, saying nothing is promised', async () => {
    serve(artifact({ origin: 'imported_original' }));
    render(ArtifactPage);
    const header = await screen.findByRole('link', { name: 'Create burn job' });
    expect(header.getAttribute('href')).toBe('/burns/new?artifact=a1');
    await fireEvent.click(screen.getByRole('tab', { name: 'Burning' }));
    expect(screen.getByText('Target compatibility unknown')).toBeTruthy();
    expect(screen.getAllByRole('link', { name: 'Create burn job' })).toHaveLength(2);
  });
});
