// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
import { describe, expect, it } from 'vitest';
import {
  PROGRAM_END,
  burnRings,
  capacityFor,
  familyLabel,
  familyTint,
  imageRings,
  programStart,
  radiusAt,
  ripRings,
  type DiscShape,
} from './disc';

const shape = (overrides: Partial<DiscShape> = {}): DiscShape => ({
  media_family: 'cd',
  sector_count: 300_000,
  tracks: [],
  ...overrides,
});

const close = (actual: number | undefined, expected: number) =>
  expect(actual).toBeCloseTo(expected, 6);

describe('the disc drawing', () => {
  it('chooses the medium’s own capacity, else the smallest nominal one that fits', () => {
    expect(capacityFor('cd', 300_000)).toBe(333_000);
    expect(capacityFor('cd', 350_000)).toBe(360_000);
    expect(capacityFor('dvd', 3_000_000)).toBe(4_173_824);
    expect(capacityFor('cd', 300_000, 359_844)).toBe(359_844);
    // Larger than any nominal size: drawn as a full disc of its own length.
    expect(capacityFor('cd', 500_000)).toBe(500_000);
    // A family with no nominal sizes, or none at all.
    expect(capacityFor('unknown', 1_000)).toBe(1_000);
    expect(capacityFor(null, 1_000)).toBe(1_000);
    // Nothing to size against.
    expect(capacityFor('cd', null)).toBeNull();
    expect(capacityFor('cd', 0)).toBeNull();
  });

  it('places a span by area, as a disc recorded at constant linear velocity is', () => {
    const start = programStart('dvd');
    close(radiusAt(0, start), start);
    close(radiusAt(1, start), PROGRAM_END);
    // Half the sectors take half the area between the two radii.
    const half = radiusAt(0.5, start);
    close(half * half - start * start, (PROGRAM_END ** 2 - start ** 2) / 2);
    // Out of range is held at the ends.
    close(radiusAt(-1, start), start);
    close(radiusAt(2, start), PROGRAM_END);
    expect(programStart('cd')).toBe(25);
  });

  it('draws an image with no tracks as one data ring to its length', () => {
    const rings = imageRings(shape({ tracks: [] }), 333_000);
    expect(rings).toHaveLength(1);
    expect(rings[0]?.kind).toBe('data');
    close(rings[0]?.inner, 25);
    close(rings[0]?.outer, radiusAt(300_000 / 333_000, 25));
  });

  it('sets each audio track off with a gap, and runs data tracks together', () => {
    const disc = shape({
      sector_count: 200_000,
      tracks: [
        { session: 1, sector_count: 50_000, is_audio: false },
        { session: 1, sector_count: 50_000, is_audio: false },
        { session: 1, sector_count: 50_000, is_audio: true },
        { session: 1, sector_count: 50_000, is_audio: true },
      ],
    });
    const rings = imageRings(disc, 333_000);
    expect(rings.map((ring) => ring.kind)).toEqual(['data', 'audio', 'audio']);
    // A gap before each audio track: it starts after the ring before ends.
    expect(rings[1]!.inner).toBeGreaterThan(rings[0]!.outer);
    expect(rings[2]!.inner).toBeGreaterThan(rings[1]!.outer);
  });

  it('merges gaps too thin for a small drawing to show', () => {
    const disc = shape({
      sector_count: 150_000,
      tracks: [
        { session: 1, sector_count: 50_000, is_audio: true },
        { session: 1, sector_count: 50_000, is_audio: true },
        { session: 1, sector_count: 50_000, is_audio: true },
      ],
    });
    // A 24-pixel drawing: five millimetres to a pixel.
    expect(imageRings(disc, 333_000, { mmPerPixel: 5 })).toHaveLength(1);
    expect(imageRings(disc, 333_000, { mmPerPixel: 0.5 })).toHaveLength(3);
  });

  it('leaves a wider gap where a new session starts', () => {
    const disc = shape({
      sector_count: 120_000,
      tracks: [
        { session: 1, sector_count: 50_000, is_audio: false },
        { session: 2, sector_count: 50_000, is_audio: false },
      ],
    });
    const rings = imageRings(disc, 333_000);
    expect(rings).toHaveLength(2);
    expect(rings[1]!.inner - rings[0]!.outer).toBeGreaterThan(0.5);
  });

  it('draws nothing it does not know', () => {
    expect(imageRings(shape({ sector_count: null }), 333_000)).toEqual([]);
    expect(imageRings(shape(), null)).toEqual([]);
    expect(burnRings(null, 333_000, 0.5)).toEqual([]);
    expect(burnRings(shape({ sector_count: null }), 333_000, 0.5)).toEqual([]);
    expect(ripRings('cd', null)).toEqual([]);
    expect(ripRings('cd', 0)).toEqual([]);
  });

  it('grows a burn’s written area outward toward the image’s extent', () => {
    const image = shape({ sector_count: 180_000 });
    const extent = radiusAt(180_000 / 360_000, 25);

    const start = burnRings(image, 360_000, 0);
    expect(start.map((ring) => ring.kind)).toEqual(['to-write']);
    close(start[0]?.outer, extent);

    const partway = burnRings(image, 360_000, 0.43);
    expect(partway.map((ring) => ring.kind)).toEqual(['written', 'to-write']);
    close(partway[0]?.outer, radiusAt(0.43 * 0.5, 25));
    close(partway[1]?.inner, partway[0]!.outer);
    close(partway[1]?.outer, extent);

    const done = burnRings(image, 360_000, 1);
    expect(done.map((ring) => ring.kind)).toEqual(['written']);
    close(done[0]?.outer, extent);

    // Before the worker reports any progress, nothing is drawn as written.
    expect(burnRings(image, 360_000, null).map((ring) => ring.kind)).toEqual(['to-write']);
  });

  it('grows a rip’s read area over the whole disc', () => {
    const rings = ripRings('dvd', 0.5);
    expect(rings).toHaveLength(1);
    expect(rings[0]?.kind).toBe('read');
    close(rings[0]?.outer, radiusAt(0.5, 24));
  });

  it('names and tints each family, and anything else as a disc', () => {
    expect(familyLabel('cd')).toBe('CD');
    expect(familyLabel('bluray')).toBe('Blu-ray');
    expect(familyLabel(null)).toBe('Disc');
    expect(familyTint('dvd')).toBe('var(--dvd)');
    expect(familyTint('uhd_bluray')).toBe('var(--bluray)');
    expect(familyTint('proprietary_optical')).toBe('var(--unknown)');
  });
});
