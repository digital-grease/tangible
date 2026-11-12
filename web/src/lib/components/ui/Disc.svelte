<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // A drawing of a disc, from its own data (docs/plans/ui-overhaul.md, "The
  // disc drawing"; the layout is lib/disc.ts).
  //
  // Modes: `image`, an artifact's disc with its tracks; `burn`, a blank
  // being written, the written area growing outward to the image's extent;
  // `rip`, a disc being read; `empty`, a drive waiting for a disc.
  //
  // Decorative: hidden from screen readers, because whatever it shows is
  // said in words beside it (the track table, the progress, the state).
  import {
    CENTRE,
    HOLE_RADIUS,
    HUB_RADIUS,
    OUTER_EDGE,
    burnRings,
    capacityFor,
    familyTint,
    imageRings,
    ripRings,
    type DiscShape,
    type Ring,
  } from '$lib/disc';

  let {
    mode = 'image',
    shape = null,
    family = null,
    capacity = null,
    progress = null,
    size = 96,
    hole = 'var(--surface)',
  }: {
    mode?: 'image' | 'burn' | 'rip' | 'empty';
    /** The disc an artifact describes. */
    shape?: DiscShape | null;
    /** The family, where there is no shape (a rip). */
    family?: string | null;
    /** The medium's capacity in sectors, when the drive reported it. */
    capacity?: number | null;
    /** From 0 to 1, for a burn or a rip. */
    progress?: number | null;
    /** Width and height in CSS pixels. */
    size?: number;
    /** What shows through the hole: the colour behind the drawing. */
    hole?: string;
  } = $props();

  const uid = $props.id();
  const discFamily = $derived(shape?.media_family ?? family);
  const tint = $derived(mode === 'burn' ? 'var(--dye)' : familyTint(discFamily));
  const rings = $derived.by((): Ring[] => {
    const mmPerPixel = (2 * OUTER_EDGE) / size;
    if (mode === 'image' && shape) {
      return imageRings(shape, capacityFor(discFamily, shape.sector_count), { mmPerPixel });
    }
    if (mode === 'burn') {
      return burnRings(shape, capacityFor(discFamily, shape?.sector_count, capacity), progress);
    }
    if (mode === 'rip') return ripRings(discFamily, progress);
    return [];
  });

  function ringColour(ring: Ring): string {
    switch (ring.kind) {
      case 'data':
        return `color-mix(in srgb, ${tint} 62%, #23252c)`;
      case 'audio':
        return `color-mix(in srgb, ${tint} 72%, #23252c)`;
      case 'written':
      case 'read':
        return 'var(--laser)';
      case 'to-write':
        return `color-mix(in srgb, ${tint} 80%, #23252c)`;
    }
  }
</script>

<svg
  class="disc"
  viewBox="0 0 120 120"
  width={size}
  height={size}
  aria-hidden="true"
  focusable="false"
>
  {#if mode === 'empty'}
    <circle cx={CENTRE} cy={CENTRE} r={OUTER_EDGE - 1.5} class="empty" />
    <circle cx={CENTRE} cy={CENTRE} r={HOLE_RADIUS} class="empty" />
  {:else}
    <defs>
      <linearGradient id="{uid}-sheen" x1="0" y1="0" x2="1" y2="1">
        <stop offset="0" stop-color="#fff" stop-opacity="0.4" />
        <stop offset="0.38" stop-color="#fff" stop-opacity="0" />
        <stop offset="0.62" stop-color="#fff" stop-opacity="0.15" />
        <stop offset="1" stop-color="#fff" stop-opacity="0" />
      </linearGradient>
    </defs>
    <circle cx={CENTRE} cy={CENTRE} r={OUTER_EDGE} style:fill={tint} />
    {#each rings as ring, index (index)}
      <circle
        cx={CENTRE}
        cy={CENTRE}
        r={(ring.inner + ring.outer) / 2}
        class="ring {ring.kind}"
        style:stroke={ringColour(ring)}
        stroke-width={Math.max(ring.outer - ring.inner, 0.3)}
      />
    {/each}
    <circle cx={CENTRE} cy={CENTRE} r={OUTER_EDGE} fill="url(#{uid}-sheen)" />
    <circle cx={CENTRE} cy={CENTRE} r={OUTER_EDGE - 0.6} class="rim" />
    <circle cx={CENTRE} cy={CENTRE} r={HUB_RADIUS} class="hub" style:fill={tint} />
    <circle cx={CENTRE} cy={CENTRE} r={HOLE_RADIUS} style:fill={hole} />
  {/if}
</svg>

<style>
  .disc {
    flex: none;
    display: block;
  }
  .ring {
    fill: none;
  }
  .rim {
    fill: none;
    stroke: #000;
    stroke-opacity: 0.16;
    stroke-width: 1.2;
  }
  .hub {
    fill-opacity: 0.55;
  }
  .empty {
    fill: none;
    stroke: var(--text-muted);
    stroke-opacity: 0.6;
    stroke-width: 1.6;
    stroke-dasharray: 5 5;
  }
</style>
