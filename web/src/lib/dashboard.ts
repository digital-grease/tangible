// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import type { components } from './api-types';
import { formatBytes } from './library';
import { apiCall, apiFetch, type Result } from './session';

export type Dashboard = components['schemas']['DashboardView'];
export type DashboardBurn = components['schemas']['DashboardBurn'];

/** What needs doing and what is under way. */
export function loadDashboard(fetcher: typeof fetch = apiFetch): Promise<Result<Dashboard>> {
  return apiCall(fetcher, '/api/v1/dashboard');
}

/** `n thing` or `n things`. */
function count(n: number, one: string, many = `${one}s`): string {
  return `${n} ${n === 1 ? one : many}`;
}

/** The parts of a sentence that are not nothing, joined; or `none`. */
function listed(parts: (string | null)[], none: string): string {
  const said = parts.filter((part): part is string => part !== null);
  return said.length === 0 ? none : said.join(', ');
}

/** Imports, in words. */
export function importsSummary(imports: Dashboard['imports']): string {
  return listed(
    [
      imports.active > 0 ? `${count(imports.active, 'import')} under way` : null,
      imports.failed > 0 ? `${imports.failed} failed this week` : null,
    ],
    'Nothing being imported, and nothing failed this week',
  );
}

/** Burns, in words. */
export function burnsSummary(burns: Dashboard['burns']): string {
  return listed(
    [
      burns.needing_attention > 0
        ? `${count(burns.needing_attention, 'burn')} needing attention`
        : null,
      burns.waiting_for_media > 0 ? `${burns.waiting_for_media} waiting for a disc` : null,
      burns.active > 0 ? `${burns.active} under way` : null,
      burns.queued > 0 ? `${burns.queued} queued` : null,
    ],
    'No burns waiting or under way',
  );
}

/** Workers, in words. */
export function workersSummary(workers: Dashboard['workers']): string {
  return listed(
    [
      `${count(workers.online, 'worker')} online`,
      workers.silent > 0 ? `${workers.silent} not heard from lately` : null,
      workers.draining > 0 ? `${workers.draining} draining` : null,
      workers.pending > 0 ? `${workers.pending} enrolled and not heard from yet` : null,
      workers.incompatible > 0 ? `${workers.incompatible} incompatible` : null,
    ],
    '',
  );
}

/** Integrity, in words. */
export function integritySummary(integrity: Dashboard['integrity']): string {
  return listed(
    [
      integrity.damaged > 0 ? `${count(integrity.damaged, 'artifact')} damaged` : null,
      integrity.unchecked > 0 ? `${integrity.unchecked} not checked yet` : null,
    ],
    'Every artifact checked and intact',
  );
}

/** Storage, in words. */
export function storageSummary(storage: Dashboard['storage']): string {
  const stored = `${formatBytes(storage.stored_bytes)} in ${count(storage.objects, 'object')}`;
  if (storage.free_bytes == null || storage.total_bytes == null) return stored;
  return `${stored}; ${formatBytes(storage.free_bytes)} free of ${formatBytes(storage.total_bytes)}`;
}

/** Whether something on the dashboard asks for somebody. */
export function needsSomebody(board: Dashboard): boolean {
  return (
    board.burns.needing_attention > 0 ||
    board.burns.waiting_for_media > 0 ||
    board.workers.silent > 0 ||
    board.workers.incompatible > 0 ||
    board.integrity.damaged > 0 ||
    board.imports.failed > 0
  );
}
