<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // A short fact as a tag: a format, a count, "Original, immutable". Not a
  // state; StatusMark is for states. `strong` outlines it in the text's
  // colour, for the one fact that matters most on a page.
  import type { Snippet } from 'svelte';
  import Icon from './Icon.svelte';
  import type { IconName } from './icons';

  let {
    icon,
    strong = false,
    children,
  }: {
    icon?: IconName;
    strong?: boolean;
    children: Snippet;
  } = $props();
</script>

<span class="badge" class:strong>
  {#if icon}<Icon name={icon} size={13} />{/if}
  {@render children()}
</span>

<style>
  .badge {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    padding: 0.0625rem var(--space-2);
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    background: var(--surface-2);
    color: var(--text);
    font-size: var(--text-sm);
    white-space: nowrap;
    vertical-align: middle;
  }
  .strong {
    border-color: var(--text);
    font-weight: var(--weight-strong);
  }
</style>
