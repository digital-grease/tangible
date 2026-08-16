// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

/** One dependency's state, as reported by `GET /readyz`. */
export interface DependencyCheck {
  name: string;
  status: 'up' | 'down';
  detail?: string;
}

/** Body of `GET /readyz`. */
export interface Readiness {
  status: 'ready' | 'degraded';
  checks: DependencyCheck[];
}

/** Body of `GET /livez`. */
export interface Liveness {
  status: string;
  version: string;
}

/**
 * What the health page renders. `unreachable` is distinct from `degraded`:
 * the server answering "my database is down" and the server not answering at
 * all are different problems, and an operator needs to tell them apart.
 */
export type HealthState =
  | { kind: 'loading' }
  | { kind: 'ready'; liveness: Liveness; readiness: Readiness }
  | { kind: 'degraded'; liveness: Liveness | null; readiness: Readiness }
  | { kind: 'unreachable'; reason: string };

async function readJson<T>(fetcher: typeof fetch, path: string): Promise<T> {
  const response = await fetcher(path);
  // /readyz answers 503 with a valid body when degraded, so a non-OK status
  // is not automatically a transport failure.
  if (!response.ok && response.status !== 503) {
    throw new Error(`${path} returned ${response.status}`);
  }
  return (await response.json()) as T;
}

/** Fetch both probes and reduce them to a single renderable state. */
export async function loadHealth(fetcher: typeof fetch = fetch): Promise<HealthState> {
  try {
    const [liveness, readiness] = await Promise.all([
      readJson<Liveness>(fetcher, '/livez'),
      readJson<Readiness>(fetcher, '/readyz'),
    ]);
    return readiness.status === 'ready'
      ? { kind: 'ready', liveness, readiness }
      : { kind: 'degraded', liveness, readiness };
  } catch (error) {
    return {
      kind: 'unreachable',
      reason: error instanceof Error ? error.message : 'unknown transport failure',
    };
  }
}
