// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { forgetReportSettings } from '$lib/report';

vi.mock('$app/state', () => ({ page: { params: { id: 'a1' } } }));

const { default: ArtifactPage } = await import('./[id]/+page.svelte');

afterEach(() => {
  vi.unstubAllGlobals();
  forgetReportSettings();
});

function serve(artifact: unknown) {
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request) => {
      const url = String(input);
      const reply =
        url === '/api/v1/artifacts/a1'
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

  it('says nothing about ripping for an upload', async () => {
    serve(artifact({ source_kind: 'upload' }));
    render(ArtifactPage);
    expect(await screen.findByRole('heading', { name: 'Summary' })).toBeTruthy();
    expect(screen.queryByRole('heading', { name: 'Ripped from a disc' })).toBeNull();
  });
});
