<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Nothing to show yet, and the next thing to do about it (UX.md 16): a
  // heading, a sentence or two, and usually one action.
  import type { Snippet } from 'svelte';
  import Icon from './Icon.svelte';
  import type { IconName } from './icons';

  let {
    title,
    icon,
    level = 2,
    action,
    children,
  }: {
    title: string;
    icon?: IconName;
    level?: 2 | 3;
    action?: Snippet;
    children?: Snippet;
  } = $props();

  const tag = $derived(`h${level}`);
</script>

<div class="empty">
  {#if icon}<span class="icon"><Icon name={icon} size={28} /></span>{/if}
  <svelte:element this={tag} class="title">{title}</svelte:element>
  {#if children}<div class="text">{@render children()}</div>{/if}
  {#if action}<div class="action">{@render action()}</div>{/if}
</div>

<style>
  .empty {
    display: grid;
    justify-items: center;
    gap: var(--space-2);
    padding: var(--space-8) var(--space-4);
    text-align: center;
    border: 1px dashed var(--line-strong);
    border-radius: var(--radius-lg);
  }
  .icon {
    color: var(--text-muted);
  }
  .title {
    margin: 0;
    font-size: var(--text-md);
  }
  .text {
    max-width: 60ch;
    color: var(--text-muted);
  }
  .text :global(p:last-child) {
    margin-bottom: 0;
  }
  .action {
    margin-top: var(--space-2);
  }
</style>
