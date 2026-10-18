// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
import { describe, expect, it, vi } from 'vitest';
import {
  downloadName,
  loadDiagnostics,
  loadDiagnosticsReport,
  statusCounts,
  statusLabel,
} from './diagnostics';

function answering(body: unknown) {
  return vi.fn(async () => ({
    ok: true,
    status: 200,
    json: async () => body,
  })) as unknown as typeof fetch;
}

describe('diagnostics', () => {
  it('asks the diagnostics routes', async () => {
    const fetcher = answering({ sections: [] });
    await loadDiagnostics(fetcher);
    expect(fetcher).toHaveBeenCalledWith('/api/v1/diagnostics', expect.anything());
    await loadDiagnosticsReport(fetcher);
    expect(fetcher).toHaveBeenCalledWith('/api/v1/diagnostics/report', expect.anything());
  });

  it('says every status in words', () => {
    expect(statusLabel('ok')).toBe('OK');
    expect(statusLabel('warn')).toBe('Warning');
    expect(statusLabel('problem')).toBe('Problem');
  });

  it('counts checks across sections', () => {
    expect(
      statusCounts({
        sections: [
          {
            title: 'A',
            checks: [
              { name: 'a', status: 'ok', detail: '' },
              { name: 'b', status: 'problem', detail: '' },
            ],
          },
          { title: 'B', checks: [{ name: 'c', status: 'warn', detail: '' }] },
        ],
      }),
    ).toEqual({ ok: 1, warn: 1, problem: 1 });
  });

  it('names a download after when it was collected', () => {
    expect(downloadName('2026-10-18T12:00:00Z')).toBe('tangible-diagnostics-2026-10-18T120000Z.md');
    expect(downloadName('2026-10-18T12:00:00.123Z')).toBe(
      'tangible-diagnostics-2026-10-18T120000Z.md',
    );
    expect(downloadName('../../etc')).toBe('tangible-diagnostics-etc.md');
    expect(downloadName('')).toBe('tangible-diagnostics-report.md');
  });
});
