// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
import { describe, expect, it } from 'vitest';
import { loadHealth } from './health';

function stubFetch(routes: Record<string, { status: number; body: unknown }>): typeof fetch {
  return (async (input: string | URL | Request) => {
    const path = typeof input === 'string' ? input : input.toString();
    const route = routes[path];
    if (!route) throw new Error(`unexpected request to ${path}`);
    return {
      ok: route.status >= 200 && route.status < 300,
      status: route.status,
      json: async () => route.body,
    } as Response;
  }) as typeof fetch;
}

describe('loadHealth', () => {
  it('reports ready when both probes succeed', async () => {
    const state = await loadHealth(
      stubFetch({
        '/livez': { status: 200, body: { status: 'alive', version: '0.1.0' } },
        '/readyz': { status: 200, body: { status: 'ready', checks: [] } },
      }),
    );
    expect(state.kind).toBe('ready');
  });

  it('treats a 503 from readyz as degraded, not as a failure', async () => {
    const state = await loadHealth(
      stubFetch({
        '/livez': { status: 200, body: { status: 'alive', version: '0.1.0' } },
        '/readyz': {
          status: 503,
          body: { status: 'degraded', checks: [{ name: 'database', status: 'down' }] },
        },
      }),
    );
    expect(state.kind).toBe('degraded');
    if (state.kind === 'degraded') {
      expect(state.readiness.checks[0]?.name).toBe('database');
    }
  });

  it('distinguishes an unreachable server from a degraded one', async () => {
    const state = await loadHealth((() => {
      throw new Error('connection refused');
    }) as unknown as typeof fetch);
    expect(state.kind).toBe('unreachable');
  });
});
