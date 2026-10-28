// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
import { describe, expect, it, vi } from 'vitest';
import {
  MAX_DAT_BYTES,
  bestMatchLabel,
  datSummary,
  identificationStateLabel,
  loadOutcome,
  matchLabel,
  matchSource,
  matchedByLabel,
  partialHint,
  uploadDat,
  type Dat,
  type DatMatch,
} from './identification';

function match(extra: Partial<DatMatch> = {}): DatMatch {
  return {
    dat_id: 'd1',
    dat_name: 'Sony - PlayStation',
    dat_version: '2026-10-01',
    game_name: 'Game (USA)',
    description: 'Game (USA)',
    category: 'Games',
    kind: 'exact',
    files_matched: 2,
    files_total: 2,
    files: [
      { name: 'Game (USA) (Track 1).bin', size: 1000, matched_by: 'track 1' },
      { name: 'Game (USA) (Track 2).bin', size: 2000, matched_by: 'track 2' },
    ],
    ...extra,
  };
}

function dat(extra: Partial<Dat> = {}): Dat {
  return {
    id: 'd1',
    name: 'Sony - PlayStation',
    description: null,
    version: '2026-10-01',
    author: 'redump.org',
    homepage: 'redump.org',
    source_sha256: '0'.repeat(64),
    game_count: 10_000,
    rom_count: 30_000,
    loaded_by: 'owner',
    loaded_at: '2026-10-28T12:00:00Z',
    matched_artifacts: 3,
    ...extra,
  };
}

describe('matches in words', () => {
  it('says an exact match matched every file, and a partial one how many', () => {
    expect(matchLabel(match())).toBe('Matches Game (USA), all 2 of its files');
    expect(matchLabel(match({ files_total: 1, files_matched: 1 }))).toBe(
      'Matches Game (USA), its file',
    );
    expect(matchLabel(match({ kind: 'partial', files_matched: 1 }))).toBe(
      'Partly matches Game (USA): 1 of 2 files',
    );
  });

  it('names the DAT with its version, and says what matched each file', () => {
    expect(matchSource(match())).toBe('Sony - PlayStation (2026-10-01)');
    expect(matchSource(match({ dat_version: null }))).toBe('Sony - PlayStation');
    expect(matchedByLabel({ name: 'a', size: 1, matched_by: 'track 2' })).toBe(
      'matched by track 2',
    );
    expect(matchedByLabel({ name: 'a', size: 1, matched_by: null })).toBe('not matched');
  });

  it('explains a partial match only where a file did not match', () => {
    expect(partialHint(match())).toBeNull();
    const partial = match({
      kind: 'partial',
      files_matched: 1,
      files: [
        { name: 'Track 1.bin', size: 1, matched_by: 'track 1' },
        { name: 'Track 2.bin', size: 1, matched_by: null },
      ],
    });
    expect(partialHint(partial)).toMatch(/offset correction/);
  });

  it('marks a partial best match in the library list', () => {
    expect(bestMatchLabel(null)).toBeNull();
    expect(bestMatchLabel({ game_name: 'Game', kind: 'exact', dat_name: 'D' })).toBe('Game');
    expect(bestMatchLabel({ game_name: 'Game', kind: 'partial', dat_name: 'D' })).toBe(
      'Game (partly)',
    );
  });

  it('says what state the hashes are in', () => {
    expect(identificationStateLabel({ state: 'done', error: null })).toBe('Hashed');
    expect(identificationStateLabel({ state: 'pending', error: null })).toMatch(/background/);
    expect(identificationStateLabel({ state: 'failed', error: 'disc.bin is missing' })).toBe(
      'Could not be hashed: disc.bin is missing',
    );
  });
});

describe('DATs in words', () => {
  it('says what a DAT holds and identifies', () => {
    expect(datSummary(dat())).toBe('10000 games; identifies 3 artifacts');
    expect(datSummary(dat({ game_count: 1, matched_artifacts: 0 }))).toBe(
      '1 game; identifies nothing in the library yet',
    );
  });

  it('says whether a load replaced a DAT, changed nothing, or left entries out', () => {
    const loaded = {
      dat: dat(),
      replaced: false,
      unchanged: false,
      warnings: [],
      warning_count: 0,
    };
    expect(loadOutcome(loaded)).toBe(
      'Sony - PlayStation was loaded: 10000 games; identifies 3 artifacts.',
    );
    expect(loadOutcome({ ...loaded, replaced: true, warning_count: 2 })).toBe(
      'Sony - PlayStation replaced the DAT of the same name: 10000 games; identifies 3 artifacts. 2 entries were left out.',
    );
    expect(loadOutcome({ ...loaded, unchanged: true })).toMatch(/nothing changed/);
  });
});

describe('uploadDat', () => {
  it('sends the file as XML, the body itself', async () => {
    const fetcher = vi.fn(async () => ({
      ok: true,
      status: 201,
      json: async () => ({
        dat: dat(),
        replaced: false,
        unchanged: false,
        warnings: [],
        warning_count: 0,
      }),
      headers: new Headers(),
    })) as unknown as typeof fetch;
    const file = new Blob(['<datafile/>'], { type: 'text/xml' });
    const result = await uploadDat(file, fetcher);
    expect(result.kind).toBe('ok');
    const [url, init] = (fetcher as unknown as ReturnType<typeof vi.fn>).mock.calls[0] as [
      string,
      RequestInit,
    ];
    expect(url).toBe('/api/v1/dats');
    expect(init.method).toBe('POST');
    expect(new Headers(init.headers).get('content-type')).toBe('application/xml');
    expect(init.body).toBe(file);
  });

  it('refuses a file over the limit without sending it', async () => {
    const fetcher = vi.fn() as unknown as typeof fetch;
    const big = { size: MAX_DAT_BYTES + 1 } as Blob;
    const result = await uploadDat(big, fetcher);
    expect(fetcher).not.toHaveBeenCalled();
    expect(result.kind).toBe('error');
    if (result.kind === 'error') {
      expect(result.problem.code).toBe('DAT_INVALID');
      expect(result.problem.detail).toMatch(/unzip it/);
    }
  });

  it('says too large when the server refuses the body before reading it', async () => {
    const fetcher = vi.fn(async () => ({
      ok: false,
      status: 413,
      json: async () => {
        throw new SyntaxError('not JSON');
      },
      headers: new Headers(),
    })) as unknown as typeof fetch;
    const result = await uploadDat(new Blob(['x']), fetcher);
    expect(result.kind === 'error' && result.problem.code).toBe('DAT_INVALID');
  });

  it('passes on the server’s refusal of a file that is not a DAT', async () => {
    const fetcher = vi.fn(async () => ({
      ok: false,
      status: 422,
      json: async () => ({
        type: 'about:blank',
        title: 'Not a DAT this server reads',
        status: 422,
        code: 'DAT_INVALID',
        detail: 'the file is not a Logiqx DAT: its root element is not datafile',
      }),
      headers: new Headers(),
    })) as unknown as typeof fetch;
    const result = await uploadDat(new Blob(['<html/>']), fetcher);
    expect(result.kind === 'error' && result.problem.detail).toMatch(/not datafile/);
  });
});
