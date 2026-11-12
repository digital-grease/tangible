<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // How far something has got, as a bar a screen reader can read.
  //
  // `value` is a fraction from 0 to 1, or null when the work has not said
  // yet (the bar is then hatched, not animated, and has no value). `laser`
  // colours it for a drive at work. The page's own words go beside it; the
  // bar is never the only place the progress is said.
  let {
    value,
    label,
    tone = 'accent',
  }: {
    value: number | null;
    /** What is progressing, as its accessible name. */
    label: string;
    tone?: 'accent' | 'laser';
  } = $props();

  const percent = $derived(
    value === null ? null : Math.round(Math.min(1, Math.max(0, value)) * 100),
  );
</script>

<div
  class="progress {tone}"
  class:unknown={percent === null}
  role="progressbar"
  aria-label={label}
  aria-valuemin={0}
  aria-valuemax={100}
  aria-valuenow={percent ?? undefined}
  aria-valuetext={percent === null ? 'Not reported yet' : `${percent}%`}
>
  {#if percent !== null}
    <div class="fill" style:width="{percent}%"></div>
  {/if}
</div>

<style>
  .progress {
    height: 0.375rem;
    border-radius: var(--radius-full);
    background: var(--line-2);
    overflow: hidden;
  }
  .fill {
    height: 100%;
    border-radius: inherit;
    background: var(--accent);
    transition: width 0.4s ease;
  }
  .laser .fill {
    background: var(--laser);
  }
  .unknown {
    background: repeating-linear-gradient(-45deg, var(--line-2) 0 6px, var(--line) 6px 12px);
  }
</style>
