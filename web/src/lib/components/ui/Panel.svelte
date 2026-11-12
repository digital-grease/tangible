<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // A surface holding one part of a page. With a title it is a section a
  // screen reader can jump to, named by its heading; `actions` go at the
  // heading's right. `flush` drops the body's padding, for a table that runs
  // to the panel's edges.
  import type { Snippet } from 'svelte';

  let {
    title,
    level = 2,
    flush = false,
    aside,
    actions,
    children,
  }: {
    title?: string;
    level?: 2 | 3 | 4;
    flush?: boolean;
    /** Muted words after the title, such as a count. */
    aside?: string;
    actions?: Snippet;
    children: Snippet;
  } = $props();

  const uid = $props.id();
  const tag = $derived(`h${level}`);
</script>

{#if title}
  <section class="panel" aria-labelledby="{uid}-title">
    <header>
      <svelte:element this={tag} id="{uid}-title" class="title">
        {title}{#if aside}<span class="aside">{aside}</span>{/if}
      </svelte:element>
      {#if actions}<div class="actions">{@render actions()}</div>{/if}
    </header>
    <div class="body" class:flush>{@render children()}</div>
  </section>
{:else}
  <div class="panel">
    <div class="body" class:flush>{@render children()}</div>
  </div>
{/if}

<style>
  .panel {
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--radius-lg);
    min-width: 0;
  }
  header {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-2) var(--space-4);
    border-bottom: 1px solid var(--line);
    min-height: 2.75rem;
  }
  .title {
    margin: 0;
    font-size: var(--text-base);
    font-weight: var(--weight-strong);
    letter-spacing: 0;
  }
  .aside {
    margin-left: var(--space-2);
    color: var(--text-muted);
    font-weight: var(--weight-medium);
  }
  .actions {
    margin-left: auto;
    display: flex;
    gap: var(--space-2);
    align-items: center;
  }
  .body {
    padding: var(--space-3) var(--space-4);
  }
  .body.flush {
    padding: 0;
  }
</style>
