// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

/**
 * The disc drawing's layout (docs/plans/ui-overhaul.md, "The disc drawing").
 *
 * Lays a disc out as rings, to scale. CDs, DVDs and Blu-ray discs are
 * recorded at a constant linear velocity, so a span of sectors takes an area
 * of the disc in proportion to its length: its radii follow a square root
 * from where the program area starts to its edge. A track is a ring; an
 * audio track is set off from the one before by a thin gap, as a pressed
 * audio CD shows them; a new session starts after a wider one. What the data
 * does not say is not drawn: an unknown family or length is a plain disc.
 *
 * Pure: Disc.svelte draws what this returns, and the tests check it.
 */
import type { components } from './api-types';

export type DiscShape = components['schemas']['DiscShapeView'];

/** The drawing's coordinates: a 120 mm disc, centre at 60, in millimetres. */
export const CENTRE = 60;
export const OUTER_EDGE = 60;
/** The hole, and the clear clamping area around it. */
export const HOLE_RADIUS = 7.5;
export const HUB_RADIUS = 17;
/** Where data stops, near the rim. */
export const PROGRAM_END = 58;

/** Where data starts: a CD's program area after its lead-in; a DVD's or BD's data zone. */
export function programStart(family: string | null | undefined): number {
  return family === 'cd' || family === 'gd_rom' ? 25 : 24;
}

/**
 * Nominal capacities in sectors, smallest first, used only when the disc in
 * a drive did not say its own: CD 74, 80, 90 and 99 minutes; DVD single and
 * dual layer; Blu-ray 25, 50, 100 and 128 GB; a GD-ROM's high-density area.
 * They are nominal, which is why they size a drawing and nothing else.
 */
export const NOMINAL_SECTORS: Readonly<Record<string, readonly number[]>> = {
  cd: [333_000, 360_000, 405_000, 445_500],
  dvd: [2_298_496, 4_173_824],
  bluray: [12_219_392, 24_438_784, 48_878_592, 62_500_864],
  uhd_bluray: [24_438_784, 32_440_320, 48_878_592],
  gd_rom: [549_150],
};

/**
 * The capacity to draw a disc of `sectors` against: the medium's own when the
 * drive reported it, otherwise the smallest nominal size it fits, otherwise
 * the disc's own length (a full disc).
 */
export function capacityFor(
  family: string | null | undefined,
  sectors: number | null | undefined,
  mediumCapacity?: number | null,
): number | null {
  if (mediumCapacity && mediumCapacity > 0) return mediumCapacity;
  if (!sectors || sectors <= 0) return null;
  const nominal = family ? NOMINAL_SECTORS[family] : undefined;
  return nominal?.find((capacity) => capacity >= sectors) ?? sectors;
}

/** The radius `fraction` of the way through the program area, by area. */
export function radiusAt(fraction: number, start: number, end: number = PROGRAM_END): number {
  const clamped = Math.min(1, Math.max(0, fraction));
  return Math.sqrt(start * start + clamped * (end * end - start * start));
}

/** What a ring is, which decides its colour. */
export type RingKind = 'data' | 'audio' | 'written' | 'read' | 'to-write';

export interface Ring {
  inner: number;
  outer: number;
  kind: RingKind;
}

/** Sectors between sessions: a lead-out, then the next session's lead-in. */
const SESSION_GAP_SECTORS = 11_400;

/** The gap drawn before an audio track, in millimetres. */
const TRACK_GAP_MM = 0.5;

export interface LayoutOptions {
  /** Millimetres per drawn pixel; gaps narrower than half a pixel are dropped. */
  mmPerPixel?: number;
}

/**
 * The rings of a disc image: each track's span, or the whole length as one
 * data ring for an image with no tracks. Empty when the length or capacity
 * is not known.
 */
export function imageRings(
  shape: DiscShape,
  capacity: number | null,
  options: LayoutOptions = {},
): Ring[] {
  const start = programStart(shape.media_family);
  const total = shape.sector_count;
  if (!capacity || !total) return [];
  const radius = (sectors: number) => radiusAt(sectors / capacity, start);
  const minimumGap = (options.mmPerPixel ?? 0) / 2;

  if (shape.tracks.length === 0) {
    return [{ inner: start, outer: radius(total), kind: 'data' }];
  }

  const rings: Ring[] = [];
  let at = 0;
  let session = shape.tracks[0]?.session ?? 1;
  for (const [index, track] of shape.tracks.entries()) {
    const newSession = track.session !== session;
    if (newSession) {
      at += SESSION_GAP_SECTORS;
      session = track.session;
    }
    let inner = radius(at);
    const outer = radius(at + track.sector_count);
    at += track.sector_count;
    // An audio track after another track is set off by a thin gap, unless
    // the gap would be thinner than the drawing can show.
    const gapped = track.is_audio && index > 0 && TRACK_GAP_MM >= minimumGap;
    if (gapped) inner = Math.min(outer, inner + TRACK_GAP_MM / 2);
    const previous = rings.at(-1);
    if (!gapped && !newSession && previous && previous.kind === kindOf(track.is_audio)) {
      // The same kind running on with no gap shown: one ring, not many.
      previous.outer = outer;
    } else {
      rings.push({ inner, outer, kind: kindOf(track.is_audio) });
    }
  }
  return rings;
}

function kindOf(isAudio: boolean): RingKind {
  return isAudio ? 'audio' : 'data';
}

/**
 * A burn in progress: the area written so far, from the centre outward, and
 * the rest of the image still to write. `progress` is from 0 to 1, or null
 * when the worker has not said (nothing is drawn as written then).
 */
export function burnRings(
  shape: DiscShape | null | undefined,
  capacity: number | null,
  progress: number | null,
): Ring[] {
  const family = shape?.media_family;
  const start = programStart(family);
  const total = shape?.sector_count;
  if (!capacity || !total) return [];
  const extent = Math.min(1, total / capacity);
  const written = extent * Math.min(1, Math.max(0, progress ?? 0));
  const rings: Ring[] = [];
  if (written > 0) rings.push({ inner: start, outer: radiusAt(written, start), kind: 'written' });
  if (written < extent) {
    rings.push({
      inner: radiusAt(written, start),
      outer: radiusAt(extent, start),
      kind: 'to-write',
    });
  }
  return rings;
}

/**
 * A rip in progress: the area read so far, over the whole recorded disc
 * (`capacity` is then the disc's own length).
 */
export function ripRings(family: string | null | undefined, progress: number | null): Ring[] {
  if (progress === null || progress <= 0) return [];
  const start = programStart(family);
  return [{ inner: start, outer: radiusAt(progress, start), kind: 'read' }];
}

/** The token a family's disc is tinted with. */
export function familyTint(family: string | null | undefined): string {
  switch (family) {
    case 'cd':
    case 'gd_rom':
      return 'var(--cd)';
    case 'dvd':
      return 'var(--dvd)';
    case 'bluray':
    case 'uhd_bluray':
      return 'var(--bluray)';
    default:
      return 'var(--unknown)';
  }
}

/** A family in words, for the label beside a drawing. */
export function familyLabel(family: string | null | undefined): string {
  switch (family) {
    case 'cd':
      return 'CD';
    case 'dvd':
      return 'DVD';
    case 'bluray':
      return 'Blu-ray';
    case 'uhd_bluray':
      return 'UHD Blu-ray';
    case 'gd_rom':
      return 'GD-ROM';
    case 'proprietary_optical':
      return 'Optical';
    default:
      return 'Disc';
  }
}
