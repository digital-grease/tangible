// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import type { components } from './api-types';
import { apiCall, apiFetch, type Result } from './session';

export type RommSettings = components['schemas']['RommSettings'];
export type EditionRomm = components['schemas']['EditionRommView'];
export type RommPlatform = components['schemas']['PlatformView'];
export type RommLink = components['schemas']['RommLinkView'];
export type RommScan = components['schemas']['RommScanView'];
export type RommApi = components['schemas']['RommApiView'];

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

/**
 * What RomM made of an edition's export, in words, once RomM's API has
 * looked it up; `null` before then.
 */
export function inRommLabel(romm: EditionRomm): string | null {
  const link = romm.in_romm;
  if (!link) return null;
  const takenOut = romm.state === 'removed';
  switch (link.state) {
    case 'unseen':
      return 'RomM has not seen it yet';
    case 'linked':
      return takenOut ? 'RomM still lists it' : 'RomM has it';
    case 'missing':
      return takenOut ? 'RomM marks it missing, as expected' : 'RomM marks it missing';
    case 'differs':
      return 'RomM sees different files';
    default:
      return link.state;
  }
}

/** A platform in the picker: its name, and a warning RomM would not know it. */
export function platformOptionLabel(platform: RommPlatform): string {
  return platform.known_to_romm === false
    ? `${platform.name} (not a platform the connected RomM knows)`
    : platform.name;
}

/** What a scan counted, in words, when RomM has counted. */
function scanCounts(scan: RommScan): string {
  if (scan.scanned_roms === null || scan.scanned_roms === undefined) return '';
  const parts = [`${scan.scanned_roms} looked at`];
  if (scan.new_roms) parts.push(`${scan.new_roms} new`);
  if (scan.updated_roms) parts.push(`${scan.updated_roms} changed`);
  return ` (${parts.join(', ')})`;
}

/** Where a scan Tangible asked RomM for stands, in words. */
export function scanLabel(scan: RommScan): string {
  switch (scan.state) {
    case 'refused':
      return `RomM would not scan: ${scan.detail ?? 'no reason given'}`;
    case 'queued':
      return 'Scan waiting in RomM';
    case 'running':
      return `Scan running${scanCounts(scan)}`;
    case 'finished':
      return `Scan finished${scanCounts(scan)}`;
    case 'failed':
      return 'Scan failed in RomM';
    case 'canceled':
      return 'Scan stopped in RomM';
    case 'lost':
      return 'RomM lost track of the scan';
    case 'timed_out':
      return 'Scan had not finished when Tangible stopped waiting';
    default:
      return scan.state;
  }
}

/** Whether RomM's API is connected and in use, so scans and links apply. */
export function rommApiInUse(settings: RommSettings | null): boolean {
  return Boolean(settings?.api?.enabled);
}
