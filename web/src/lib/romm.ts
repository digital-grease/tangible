// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import type { components } from './api-types';
import { apiCall, apiFetch, type Result } from './session';

export type RommSettings = components['schemas']['RommSettings'];
export type EditionRomm = components['schemas']['EditionRommView'];

/** Whether this server exports to RomM, and the platforms it knows. */
export function loadRommSettings(fetcher: typeof fetch = apiFetch): Promise<Result<RommSettings>> {
  return apiCall(fetcher, '/api/v1/romm');
}

/** An edition's RomM settings and how its export stands. */
export function loadEditionRomm(
  editionId: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<EditionRomm>> {
  return apiCall(fetcher, `/api/v1/editions/${encodeURIComponent(editionId)}/romm`);
}

/** Save an edition's platform and whether it appears in RomM. */
export function saveEditionRomm(
  editionId: string,
  platform: string | null,
  exportToRomm: boolean,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<EditionRomm>> {
  return apiCall(fetcher, `/api/v1/editions/${encodeURIComponent(editionId)}/romm`, 'PUT', {
    platform,
    export: exportToRomm,
  });
}

/** How an edition's export stands, in words. Never colour alone. */
export function rommStateLabel(romm: EditionRomm): string {
  if (!romm.export && (!romm.state || romm.state === 'removed')) return 'Not in RomM';
  if (!romm.state) return 'Waiting for the exporter';
  switch (romm.state) {
    case 'current':
      return romm.export ? 'In RomM' : 'Being taken out of RomM';
    case 'blocked':
      return 'Not exported';
    case 'failed':
      return 'Export failed';
    case 'removed':
      return romm.export ? 'Waiting for the exporter' : 'Not in RomM';
    default:
      return romm.state;
  }
}
