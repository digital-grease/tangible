<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Tangible's mark and name: the shell's sidebar, and the pages outside the
  // shell (signing in, setting up). The drawing is decorative; the name is
  // the words. A container hides the name by setting `--wordmark-name:
  // none`, as the sidebar does when it shows icons only.
  let {
    size = 22,
    hole = 'var(--bg)',
  }: {
    /** The mark's width and height, in pixels. */
    size?: number;
    /** What shows through the centre hole: the colour behind the mark. */
    hole?: string;
  } = $props();
</script>

<span class="wordmark">
  <svg viewBox="0 0 24 24" width={size} height={size} aria-hidden="true" focusable="false">
    <circle cx="12" cy="12" r="10.5" class="disc" />
    <circle cx="12" cy="12" r="6.5" class="band" />
    <circle cx="12" cy="12" r="2" style:fill={hole} />
  </svg>
  <span class="name">TANGIBLE</span>
</span>

<style>
  .wordmark {
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    color: var(--text);
  }
  .name {
    display: var(--wordmark-name, inline);
    font-family: var(--font-mono);
    font-weight: 700;
    font-size: var(--text-sm);
    letter-spacing: 0.18em;
  }
  .disc {
    fill: var(--cd);
  }
  .band {
    fill: none;
    stroke: color-mix(in srgb, var(--cd) 55%, #23252c);
    stroke-width: 3.5;
  }
</style>
