// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

/**
 * The tones a status can take (docs/plans/ui-overhaul.md, "Status marks").
 *
 * - `ok`: done and good (structurally valid, verified, intact, online).
 * - `warn`: needs a look or is waiting (findings, waiting for a disc).
 * - `bad`: failed, damaged, refused.
 * - `busy`: under way (writing, importing, ripping).
 * - `idle`: not known yet, or nothing happening (not checked, queued).
 */
export type Tone = 'ok' | 'warn' | 'bad' | 'busy' | 'idle';

export const TONES: readonly Tone[] = ['ok', 'warn', 'bad', 'busy', 'idle'];
