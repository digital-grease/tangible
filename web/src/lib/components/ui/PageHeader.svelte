<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts" module>
  /** One step of the breadcrumbs; without an address, the page itself. */
  export interface Crumb {
    label: string;
    href?: string;
  }
</script>

<script lang="ts">
  // The top of a page: where it sits (breadcrumbs), its one h1, a sentence
  // about it, and its actions. The last crumb is the page itself and is not a
  // link.
  import type { Snippet } from 'svelte';

  let {
    title,
    crumbs = [],
    description,
    actions,
    badges,
  }: {
    title: string;
    crumbs?: Crumb[];
    description?: string;
    actions?: Snippet;
    /** Marks and badges under the title, such as a state. */
    badges?: Snippet;
  } = $props();
</script>

<header class="page-header">
  {#if crumbs.length > 0}
    <nav aria-label="Breadcrumb">
      <ol>
        {#each crumbs as crumb, index (index)}
          <li>
            {#if crumb.href}<a href={crumb.href}>{crumb.label}</a>{:else}<span aria-current="page"
                >{crumb.label}</span
              >{/if}
          </li>
        {/each}
      </ol>
    </nav>
  {/if}
  <div class="row">
    <h1>{title}</h1>
    {#if actions}<div class="actions">{@render actions()}</div>{/if}
  </div>
  {#if badges}<div class="badges">{@render badges()}</div>{/if}
  {#if description}<p class="description">{description}</p>{/if}
</header>

<style>
  .page-header {
    margin: 0 0 var(--space-5);
  }
  ol {
    display: flex;
    flex-wrap: wrap;
    list-style: none;
    margin: 0 0 var(--space-1);
    padding: 0;
    font-size: var(--text-sm);
    color: var(--text-muted);
  }
  li + li::before {
    content: '/';
    margin: 0 var(--space-2);
    color: var(--text-muted);
  }
  li a {
    color: var(--text-muted);
  }
  .row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-3);
  }
  h1 {
    margin: 0;
    font-size: var(--text-xl);
    overflow-wrap: anywhere;
  }
  .actions {
    margin-left: auto;
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
  }
  .badges {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    align-items: center;
    margin-top: var(--space-2);
  }
  .description {
    margin: var(--space-2) 0 0;
    max-width: 75ch;
    color: var(--text-muted);
  }
</style>
