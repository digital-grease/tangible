<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // One of Tangible's icons, in the surrounding text's colour.
  //
  // Decorative unless given a label: an icon beside words adds nothing for a
  // screen reader, so it is hidden from one. An icon standing alone, with no
  // words, must be given a label, and is then announced as an image with it.
  import { ICONS, type IconName } from './icons';

  let {
    name,
    size = 16,
    label,
  }: {
    name: IconName;
    /** Width and height in CSS pixels. */
    size?: number;
    /** What the icon means, when no words beside it say so. */
    label?: string;
  } = $props();
</script>

<svg
  class="icon"
  viewBox="0 0 24 24"
  width={size}
  height={size}
  role={label ? 'img' : undefined}
  aria-label={label}
  aria-hidden={label ? undefined : 'true'}
  focusable="false"
>
  <path d={ICONS[name]} />
</svg>

<style>
  .icon {
    flex: none;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.8;
    stroke-linecap: round;
    stroke-linejoin: round;
    vertical-align: -0.15em;
  }
</style>
