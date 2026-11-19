// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
import { describe, expect, it, vi } from 'vitest';
import {
  downloadHref,
  isPruned,
  kindLabel,
  operationLabel,
  readiness,
  reportFindings,
  scheduleLabel,
  stepLabel,
  uncataloguedCount,
  uploadCopy,
  type Backups,
} from './backups';

function backups(extra: Partial<Backups> = {}): Backups {
  return {
    copies: [],
    kept: 10,
    writable: true,
    tools: { ready: true, tools_version: 17, database_version: 17, problem: null },
    operation: null,
    last_outcome: null,
    max_upload_bytes: 1024,
    schedule: { enabled: true, time: '03:00', next_at: null, updated_by: null, updated_at: null },
    last_restore: null,
    can_restore: true,
    ...extra,
  };
}

describe('copies of the database', () => {
  it("names a copy's route by its name, escaped", () => {
    expect(downloadHref('tangible-20261117T210000Z.dump')).toBe(
      '/api/v1/backups/tangible-20261117T210000Z.dump/download',
    );
  });

  it('says who took a copy, and that only those Tangible took go to make room', () => {
    expect(kindLabel('taken')).toBe('Taken by Tangible');
    expect(kindLabel('uploaded')).toBe('Uploaded');
    expect(kindLabel('added')).toBe('Added at the machine');
    expect(isPruned('taken')).toBe(true);
    expect(isPruned('uploaded')).toBe(false);
    expect(isPruned('added')).toBe(false);
  });

  it('says when copies are taken by themselves', () => {
    const schedule = backups().schedule;
    expect(scheduleLabel(schedule, 10)).toBe('Every day at 03:00 UTC');
    expect(scheduleLabel(schedule, 0)).toBe('Every day at 03:00 UTC, but copies are turned off');
    expect(scheduleLabel({ ...schedule, enabled: false }, 10)).toBe(
      'Copies are taken only at a start and when asked for',
    );
  });

  it('says where a restore is, and what its check found', () => {
    expect(stepLabel('rows')).toBe("Loading the copy's rows");
    expect(reportFindings(null)).toEqual([]);
    expect(
      reportFindings({ missing_objects: { count: 0 }, uncatalogued: { count: 0 }, unfinished: [] }),
    ).toEqual([{ tone: 'ok', text: 'The catalogue and the library agree.' }]);
    const findings = reportFindings({
      missing_objects: { count: 2 },
      wrong_size: { count: 0 },
      missing_manifests: { count: 1 },
      uncatalogued: { count: 3 },
      registered: 1,
      unfinished: [['burn', 'writing', 1]],
    });
    expect(findings.map((f) => f.tone)).toEqual(['bad', 'bad', 'warn', 'idle']);
    expect(findings[0]?.text).toMatch(/^2 files the catalogue names are not in the library/);
    expect(findings[2]?.text).toMatch(/^2 artifacts were imported after the copy was taken/);
    expect(findings[3]?.text).toMatch(/^1 burn writing, as the copy recorded it/);
    expect(uncataloguedCount({ uncatalogued: { count: 3 }, registered: 1 })).toBe(2);
    expect(uncataloguedCount(undefined)).toBe(0);
  });

  it('says what is under way', () => {
    expect(operationLabel({ kind: 'taking', by: 'owner', started_at: '' })).toBe(
      'Taking a copy, asked for by owner',
    );
    expect(operationLabel({ kind: 'uploading', by: 'owner', started_at: '' })).toBe(
      'Receiving a copy uploaded by owner',
    );
  });

  it('says first what stops copies, the tools, then the volume, then the setting', () => {
    expect(readiness(backups()).tone).toBe('ok');
    expect(
      readiness(
        backups({
          tools: { ready: false, tools_version: null, database_version: 17, problem: 'no tools' },
          writable: false,
        }),
      ),
    ).toEqual({ tone: 'bad', label: 'Copies cannot be taken', detail: 'no tools' });
    expect(readiness(backups({ writable: false })).label).toBe(
      'The backups volume cannot be written',
    );
    expect(readiness(backups({ kept: 0 })).tone).toBe('warn');
  });

  it('refuses a file larger than the server accepts without sending it', async () => {
    const fetcher = vi.fn();
    const result = await uploadCopy(new Blob([new Uint8Array(2048)]), 1024, fetcher);
    expect(fetcher).not.toHaveBeenCalled();
    expect(result.kind).toBe('error');
    if (result.kind === 'error') {
      expect(result.problem.status).toBe(413);
      expect(result.problem.detail).toMatch(/TANGIBLE_MAX_BACKUP_UPLOAD_BYTES/);
    }
  });

  it("says a proxy's plain-text refusal of a large body is a proxy's", async () => {
    const fetcher = vi.fn(
      async () =>
        ({
          ok: false,
          status: 413,
          json: async () => {
            throw new SyntaxError('not JSON');
          },
        }) as unknown as Response,
    );
    const result = await uploadCopy(new Blob(['PGDMP']), 1024, fetcher);
    expect(result.kind === 'error' && result.problem.detail).toMatch(/a proxy in front of it/);
  });
});
