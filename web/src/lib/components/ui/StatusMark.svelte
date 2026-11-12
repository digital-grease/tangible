<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // A state, as a mark and words (docs/plans/ui-overhaul.md, "Status marks").
  //
  // Never colour alone: each tone has its own shape as well as its colour (a
  // circle with a tick, a triangle with "!", a square with a cross, a filled
  // dot, a hollow circle), and the words are always there. The mark is
  // decorative; the words are what a screen reader reads. The words come
  // from the page's own label functions, so their wording does not change.
  import type { Tone } from './tone';

  let {
    tone,
    label,
    pill = false,
  }: {
    tone: Tone;
    /** The state in words. */
    label: string;
    /** Set on a tinted background, for dense tables. */
    pill?: boolean;
  } = $props();
</script>

<span class="mark {tone}" class:pill>
  <svg viewBox="0 0 16 16" width="15" height="15" aria-hidden="true" focusable="false">
    {#if tone === 'ok'}
      <circle cx="8" cy="8" r="7" class="body" />
      <path d="m4.8 8.2 2.1 2.1 4.3-4.6" class="glyph" />
    {:else if tone === 'warn'}
      <path d="M8 1.2 15.2 14.4H.8z" class="body" />
      <path d="M8 6v3.6M8 11.9v.1" class="glyph" />
    {:else if tone === 'bad'}
      <rect x="1" y="1" width="14" height="14" rx="3" class="body" />
      <path d="m5.2 5.2 5.6 5.6m0-5.6-5.6 5.6" class="glyph" />
    {:else if tone === 'busy'}
      <circle cx="8" cy="8" r="6.5" class="ring" />
      <circle cx="8" cy="8" r="3.5" class="body" />
    {:else}
      <circle cx="8" cy="8" r="6" class="ring" />
    {/if}
  </svg>
  <span class="words">{label}</span>
</span>

<style>
  .mark {
    display: inline-flex;
    align-items: center;
    gap: 0.375rem;
    color: var(--tone);
    font-weight: var(--weight-strong);
    vertical-align: middle;
  }
  .mark svg {
    flex: none;
  }
  .ok {
    --tone: var(--ok);
  }
  .warn {
    --tone: var(--warn);
  }
  .bad {
    --tone: var(--bad);
  }
  .busy {
    --tone: var(--accent);
  }
  .idle {
    --tone: var(--text-muted);
  }
  .body {
    fill: var(--tone);
  }
  .glyph {
    fill: none;
    stroke: var(--bg);
    stroke-width: 2;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
  .ring {
    fill: none;
    stroke: var(--tone);
    stroke-width: 1.6;
  }
  .busy .ring {
    stroke-opacity: 0.45;
  }
  .pill {
    padding: 0.0625rem 0.5rem 0.0625rem 0.25rem;
    border-radius: var(--radius-full);
    background: color-mix(in srgb, var(--tone) 12%, transparent);
    font-size: var(--text-sm);
  }
</style>
