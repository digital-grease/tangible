// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import { describe, expect, it } from 'vitest';
import {
  isChecked,
  isOpen,
  jobStateLabel,
  lossLabel,
  requestDerivative,
  transformationLabel,
  type DerivationJob,
} from './derivations';

const job = (overrides: Partial<DerivationJob>): DerivationJob => ({
  id: 'j1',
  parent_artifact_id: 'a1',
  transformation: 'chd_create_cd',
  tool_name: 'chdman',
  tool_version: '0.251',
  options: {},
  fingerprint: 'f'.repeat(64),
  state: 'queued',
  child_artifact_id: null,
  attempts: 0,
  error_code: null,
  error_detail: null,
  created_by: 'owner',
  created_at: '2026-10-07T00:00:00Z',
  started_at: null,
  completed_at: null,
  ...overrides,
});

describe('derivatives', () => {
  it('says what a derivative preserved, and only the checked ones claim it', () => {
    expect(lossLabel('bit_exact_repack')).toMatch(/^Checked/);
    expect(lossLabel('structurally_equivalent')).toMatch(/^Checked/);
    expect(lossLabel('unknown')).toBe('Not checked against the original');
    expect(lossLabel('lossy')).toMatch(/^Lossy/);
    expect(isChecked('structurally_equivalent')).toBe(true);
    expect(isChecked('unknown')).toBe(false);
    expect(isChecked('semantically_equivalent')).toBe(false);
  });

  it('names transformations and job states in words', () => {
    expect(transformationLabel('chd_create_cd')).toBe('CHD of a CD');
    expect(jobStateLabel(job({ state: 'running' }))).toBe('Being made');
    expect(jobStateLabel(job({ state: 'failed_terminal' }))).toBe('Failed');
    expect(isOpen(job({ state: 'failed_retryable' }))).toBe(true);
    expect(isOpen(job({ state: 'complete' }))).toBe(false);
  });

  it('asks for a derivative with an empty request, so the server picks', async () => {
    const seen: { path: string; method: string; body: unknown }[] = [];
    const fetcher = (async (input: string | URL | Request, init?: RequestInit) => {
      seen.push({
        path: String(input),
        method: init?.method ?? 'GET',
        body: typeof init?.body === 'string' ? JSON.parse(init.body) : undefined,
      });
      return {
        ok: true,
        status: 202,
        headers: new Headers(),
        json: async () => ({ result: 'queued', job: job({}), derivation: null }),
      } as Response;
    }) as typeof fetch;
    const result = await requestDerivative('a/1', fetcher);
    expect(seen[0]).toEqual({
      path: '/api/v1/artifacts/a%2F1/derivatives',
      method: 'POST',
      body: {},
    });
    expect(result.kind).toBe('ok');
  });
});
