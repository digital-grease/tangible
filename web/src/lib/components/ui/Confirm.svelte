<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // The question a destructive action asks before it is done (AGENTS.md 9):
  // a heading that says what will happen, the consequences in words, and
  // the buttons, the danger one first. Shown in place, under what it is
  // about, and bordered as dangerous.
  //
  // It takes the keyboard to its heading as it appears, so what it asks is
  // read before anything else is reached. With `onsubmit` it is a form,
  // for a question with fields (how thoroughly to erase, a box to tick);
  // otherwise a named group of buttons.
  import type { Snippet } from 'svelte';

  let {
    title,
    level = 3,
    onsubmit,
    children,
    actions,
  }: {
    /** What will happen, as a question or a heading: "Remove NAS?" */
    title: string;
    level?: 2 | 3 | 4;
    onsubmit?: (event: SubmitEvent) => void;
    /** The consequences, and any fields. */
    children: Snippet;
    /** The buttons. */
    actions: Snippet;
  } = $props();

  const uid = $props.id();
  const tag = $derived(`h${level}`);
  let heading = $state<HTMLElement | null>(null);

  $effect(() => {
    heading?.focus();
  });
</script>

{#snippet body()}
  <svelte:element this={tag} id="{uid}-title" class="title" tabindex="-1" bind:this={heading}
    >{title}</svelte:element
  >
  {@render children()}
  <div class="buttons">{@render actions()}</div>
{/snippet}

{#if onsubmit}
  <form class="confirm" aria-labelledby="{uid}-title" {onsubmit}>{@render body()}</form>
{:else}
  <div class="confirm" role="group" aria-labelledby="{uid}-title">{@render body()}</div>
{/if}

<style>
  .confirm {
    display: grid;
    gap: var(--space-3);
    margin: 0;
    padding: var(--space-4);
    border-top: 1px solid var(--line);
    border-left: 4px solid var(--bad);
    background: color-mix(in srgb, var(--bad) 6%, var(--surface));
  }
  .title {
    margin: 0;
    font-size: var(--text-md);
  }
  .confirm :global(p) {
    margin: 0;
    max-width: 75ch;
  }
  .confirm :global(fieldset) {
    display: grid;
    gap: var(--space-2);
    margin: 0;
    padding: var(--space-2) var(--space-3) var(--space-3);
    border: 1px solid var(--line);
    border-radius: var(--radius);
    max-width: 75ch;
  }
  .confirm :global(legend) {
    padding: 0 var(--space-1);
    font-weight: var(--weight-strong);
  }
  .buttons {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
  }
</style>
