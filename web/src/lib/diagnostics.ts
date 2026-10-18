// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// Diagnostics, for administrators: the checklist `tangible doctor --report`
// prints, from the server, and the same prepared as a bug report.

import type { components } from './api-types';
import type { IssueReport } from './report';
import { apiCall, apiFetch, type Result } from './session';

export type DiagnosticsView = components['schemas']['DiagnosticsView'];
export type DiagnosticSection = components['schemas']['DiagnosticSection'];
export type DiagnosticCheck = components['schemas']['DiagnosticCheck'];
export type CheckStatus = components['schemas']['CheckStatus'];

/** The deployment's diagnostics, collected when asked. */
export function loadDiagnostics(
  fetcher: typeof fetch = apiFetch,
): Promise<Result<DiagnosticsView>> {
  return apiCall(fetcher, '/api/v1/diagnostics');
}

/** The diagnostics as a bug report, scrubbed, for review. */
export function loadDiagnosticsReport(
  fetcher: typeof fetch = apiFetch,
): Promise<Result<IssueReport>> {
  return apiCall(fetcher, '/api/v1/diagnostics/report');
}

/** A status as words: never colour alone. */
export function statusLabel(status: CheckStatus): string {
  switch (status) {
    case 'ok':
      return 'OK';
    case 'warn':
      return 'Warning';
    case 'problem':
      return 'Problem';
  }
}

/** How many checks came out each way, across every section. */
export function statusCounts(view: Pick<DiagnosticsView, 'sections'>): Record<CheckStatus, number> {
  const counts: Record<CheckStatus, number> = { ok: 0, warn: 0, problem: 0 };
  for (const section of view.sections) {
    for (const check of section.checks) counts[check.status] += 1;
  }
  return counts;
}

/** A file name for the Markdown download: `tangible-diagnostics-2026-10-18T120000Z.md`. */
export function downloadName(generatedAt: string): string {
  const stamp = generatedAt.replace(/[:]/g, '').replace(/\.\d+/, '');
  return `tangible-diagnostics-${stamp.replace(/[^0-9A-Za-z-]/g, '') || 'report'}.md`;
}
