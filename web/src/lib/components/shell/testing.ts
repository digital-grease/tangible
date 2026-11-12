// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// A stand-in server for the shell's component tests: each path answers as
// given, anything else 404. Imported by tests only.
import { vi } from 'vitest';

export type Routes = Record<string, { status: number; body: unknown }>;

/** Stub fetch with `routes`; returns every path asked for, in order. */
export function server(routes: Routes): string[] {
  const asked: string[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: string | URL | Request) => {
      const path = String(input);
      asked.push(path);
      const reply = routes[path] ?? { status: 404, body: {} };
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

export const MEDIUM = {
  profile: 'CD-R',
  blank: true,
  writable: true,
  rewritable: false,
  erasable: false,
  capacity_blocks: 359_844,
  free_blocks: 359_844,
  block_size: 2048,
  sessions: 0,
  manufacturer: null,
  warnings: [],
};

export function drive(id: string, name: string, activity: unknown = null) {
  return {
    id,
    name,
    device: '/dev/disc-block',
    vendor: null,
    model: null,
    firmware: null,
    connection: 'usb',
    status: 'ready',
    last_seen_at: null,
    worker_id: 'w1',
    worker_name: 'bench-1',
    worker_status: 'online',
    worker_last_seen_at: null,
    can_rip: true,
    can_decrypt_rips: false,
    can_rip_with_redumper: false,
    activity,
  };
}

export const WRITING = {
  kind: 'burn',
  id: 'j1',
  state: 'writing',
  title: 'Debian 13.1 netinst',
  disc_number: 1,
  progress: 0.43,
  medium: MEDIUM,
  disc_shape: { media_family: 'cd', sector_count: 300_000, tracks: [] },
  started_at: '2026-11-11T12:00:00Z',
};

export const DASHBOARD = {
  imports: { active: 2, failed: 0 },
  burns: { queued: 3, waiting_for_media: 1, active: 1, needing_attention: 0 },
  workers: { online: 1, silent: 0, draining: 0, pending: 0, incompatible: 0 },
  integrity: { damaged: 0, unchecked: 0 },
  storage: { objects: 0, stored_bytes: 0, free_bytes: null, total_bytes: null },
  recent_burns: [],
  attention: [],
};
