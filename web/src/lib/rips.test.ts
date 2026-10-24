// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
import { describe, expect, it, vi } from 'vitest';
import {
  cancelRip,
  cdEngineLabel,
  decryptionSummary,
  encryptionLabel,
  genericDriveLabel,
  keepLabel,
  loadRip,
  loadRipEvents,
  loadRips,
  requestRip,
  ripProgress,
  ripRefusal,
  ripOptions,
  rippedDrive,
  ripStateLabel,
  readOffsetLabel,
  retriedLabel,
  writeOffsetLabel,
  type CdReadView,
  type RipEventView,
  type RippedView,
} from './rips';

function answering() {
  return vi.fn(async () => ({
    ok: true,
    status: 200,
    json: async () => ({}),
  })) as unknown as typeof fetch;
}

function event(progress: number | null): RipEventView {
  return { sequence: 0, event_type: 'progress', code: 'RIP_PROGRESS', progress, worker_time: '' };
}

describe('rips', () => {
  it('asks the rip routes', async () => {
    const fetcher = answering();
    await requestRip('d/1', {}, fetcher);
    expect(fetcher).toHaveBeenCalledWith(
      '/api/v1/drives/d%2F1/rips',
      expect.objectContaining({ method: 'POST', body: '{}' }),
    );
    await requestRip('d1', { decrypt: true, keep: 'decrypted_original' }, fetcher);
    expect(fetcher).toHaveBeenCalledWith(
      '/api/v1/drives/d1/rips',
      expect.objectContaining({
        method: 'POST',
        body: '{"decrypt":true,"keep":"decrypted_original"}',
      }),
    );
    await loadRips(fetcher);
    expect(fetcher).toHaveBeenCalledWith('/api/v1/rips?limit=20', expect.anything());
    await loadRip('r1', fetcher);
    expect(fetcher).toHaveBeenCalledWith('/api/v1/rips/r1', expect.anything());
    await loadRipEvents('r1', fetcher);
    expect(fetcher).toHaveBeenCalledWith('/api/v1/rips/r1/events', expect.anything());
    await cancelRip('r1', fetcher);
    expect(fetcher).toHaveBeenCalledWith(
      '/api/v1/rips/r1/cancel',
      expect.objectContaining({ method: 'POST' }),
    );
  });

  it('says every state in words', () => {
    for (const state of [
      'queued',
      'reading',
      'uploading',
      'registering',
      'complete',
      'canceled',
      'refused',
      'failed',
    ]) {
      expect(ripStateLabel(state)).not.toBe(state);
    }
    expect(ripStateLabel('something_new')).toBe('something_new');
  });

  it('explains the refusals an operator can act on', () => {
    expect(ripRefusal('NO_MEDIUM')).toContain('no disc');
    expect(ripRefusal('BLANK_MEDIUM')).toContain('blank');
    expect(ripRefusal('MEDIUM_UNSUPPORTED')).toContain('Only CDs');
    expect(ripRefusal('RELOAD_DISC')).toContain('put it back');
    expect(ripRefusal('ENCRYPTED')).toContain('Decrypt an encrypted disc');
    expect(ripRefusal('DECRYPTION_FAILED')).toContain('decrypting it failed');
    expect(ripRefusal('CD_ENGINE_UNAVAILABLE')).toContain('redumper');
    expect(ripRefusal('INSUFFICIENT_SPACE')).toContain('too little free space');
    expect(ripRefusal('READ_FAILED')).toBeNull();
    expect(ripRefusal(null)).toBeNull();
  });

  it('takes progress from the newest event that has some', () => {
    expect(ripProgress([])).toBeNull();
    expect(ripProgress([event(0.25), event(0.5), event(null)])).toBe(50);
    expect(ripProgress([event(1.7)])).toBe(100);
  });

  it('asks to decrypt only when the box is ticked', () => {
    expect(ripOptions(false, 'decrypted_original')).toEqual({});
    expect(ripOptions(true, 'encrypted_original')).toEqual({
      decrypt: true,
      keep: 'encrypted_original',
    });
  });

  it('asks for a CD tool only when cdrdao is chosen', () => {
    expect(ripOptions(false, 'encrypted_original', 'redumper')).toEqual({});
    expect(ripOptions(false, 'encrypted_original', 'cdrdao')).toEqual({ cd_engine: 'cdrdao' });
    expect(ripOptions(true, 'decrypted_original', 'cdrdao')).toEqual({
      decrypt: true,
      keep: 'decrypted_original',
      cd_engine: 'cdrdao',
    });
    expect(cdEngineLabel('redumper')).toContain('corrected');
    expect(cdEngineLabel('cdrdao')).toContain('no offset correction');
    expect(cdEngineLabel(null)).toBe("The worker's best");
  });

  it('says what was found and what was kept in words', () => {
    expect(encryptionLabel(null)).toBe('Not read yet');
    expect(encryptionLabel('none')).toBe('Not encrypted');
    expect(encryptionLabel('css')).toContain('CSS');
    expect(encryptionLabel('aacs')).toContain('AACS');
    expect(encryptionLabel('something_new')).toBe('something_new');
    expect(keepLabel('encrypted_original')).toContain('decrypted copy');
    expect(keepLabel('decrypted_original')).toContain('not a faithful copy');
    expect(keepLabel(null)).toBe('—');
    expect(
      decryptionSummary({
        system: 'css',
        library: 'libdvdcss',
        library_version: '1.6.0',
        tool: 'dvdbackup',
        tool_version: '0.4.2',
      }),
    ).toBe('libdvdcss 1.6.0, through dvdbackup 0.4.2');
  });

  it('says whether a CD was read offset-corrected, and only for a CD', () => {
    const read: CdReadView = {
      read_offset: 6,
      write_offset: -6,
      generic_drive: true,
      retried_samples: 0,
    };
    expect(readOffsetLabel('redumper', read)).toBe('Corrected (+6 samples)');
    expect(readOffsetLabel('redumper', { ...read, read_offset: -1164 })).toBe(
      'Corrected (-1164 samples)',
    );
    expect(readOffsetLabel('redumper', { ...read, read_offset: null })).toContain(
      'redumper has no read offset for this drive',
    );
    expect(readOffsetLabel('redumper', { ...read, read_offset: undefined })).toContain(
      'Not corrected',
    );
    expect(readOffsetLabel('cdrdao', null)).toContain('read with cdrdao');
    expect(readOffsetLabel('block_reader', null)).toBeNull();
    expect(readOffsetLabel(null, undefined)).toBeNull();
    expect(writeOffsetLabel(read)).toBe('-6 samples');
    expect(writeOffsetLabel({ ...read, write_offset: 0 })).toBe('0 samples');
    expect(writeOffsetLabel({ ...read, write_offset: null })).toBe('Not found');
    expect(genericDriveLabel(true)).toContain('ask for cdrdao');
    expect(genericDriveLabel(false)).toBe('No: a drive redumper knows');
    expect(retriedLabel(read)).toContain('None');
    expect(retriedLabel({ ...read, retried_samples: 588 })).toBe(
      '588 samples read again; every one read in the end',
    );
  });

  it('names the drive a disc was ripped in', () => {
    const ripped: RippedView = {
      rip_id: 'r1',
      drive_vendor: 'Slimtype',
      drive_model: 'DVD A DS8A8SH',
      drive_firmware: 'KS21',
      drive_connection: 'usb',
      engine: 'block_reader',
      engine_version: '0.1.0',
      encryption: 'none',
      decrypted_at_acquisition: false,
    };
    expect(rippedDrive(ripped)).toBe('Slimtype DVD A DS8A8SH, firmware KS21, USB');
    expect(
      rippedDrive({ ...ripped, drive_vendor: null, drive_model: null, drive_firmware: null }),
    ).toBe('A drive that did not say what it is, USB');
    expect(rippedDrive({ ...ripped, drive_connection: 'unknown' })).toBe(
      'Slimtype DVD A DS8A8SH, firmware KS21',
    );
  });
});
