<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Something a page wants read before what follows: a note set apart, with
  // an edge in its tone's colour. The words say what it is; the edge only
  // repeats it, so a warning's words must say it is one.
  //
  // With a title it is a section a screen reader can jump to, named by its
  // heading ("A burn is waiting for a disc"). Without one it is a block of
  // the page's own markup, a note when `role="note"` is given. `marked`
  // puts the tone's status mark before the title, for a callout about
  // something that wants doing.
  import type { Snippet } from 'svelte';
  import StatusMark from './StatusMark.svelte';

  let {
    tone = 'info',
    title,
    level = 2,
    marked = false,
    role,
    children,
  }: {
    tone?: 'info' | 'warn' | 'bad';
    title?: string;
    level?: 2 | 3 | 4;
    /** The tone's status mark before the title. */
    marked?: boolean;
    /** A note, for one beside the content rather than part of it. */
    role?: 'note';
    children: Snippet;
  } = $props();

  const uid = $props.id();
  const tag = $derived(`h${level}`);
</script>

{#if title}
  <section class="callout {tone}" aria-labelledby="{uid}-title">
    <svelte:element this={tag} id="{uid}-title" class="title">
      {#if marked}<StatusMark
          tone={tone === 'info' ? 'busy' : tone}
          label={title}
        />{:else}{title}{/if}
    </svelte:element>
    {@render children()}
  </section>
{:else}
  <div class="callout {tone}" {role}>
    {@render children()}
  </div>
{/if}

<style>
  /* Spaced below as a paragraph is, so it can stand where one stood. */
  .callout {
    max-width: 75ch;
    margin: 0 0 var(--space-4);
    padding: var(--space-3) var(--space-4);
    background: var(--surface);
    border: 1px solid var(--line);
    border-left: 4px solid var(--accent);
    border-radius: var(--radius);
  }
  .warn {
    border-left-color: var(--warn);
  }
  .bad {
    border-left-color: var(--bad);
  }
  .title {
    font-size: var(--text-md);
  }
  /* Whatever it holds is spaced evenly, whatever its own margins. */
  .callout > :global(*) {
    margin-top: 0;
    margin-bottom: 0;
  }
  .callout > :global(* + *) {
    margin-top: var(--space-2);
  }
  .callout > :global(ul) {
    padding-left: var(--space-5);
  }
</style>
