<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // A search result's thumbnail (ADR 0027), at Tangible's address for it:
  // the browser never asks a provider. Tangible fetches the picture the
  // first time it is asked for, so it may turn out the provider has none;
  // the drawing of a disc stands in then, as it does for a result with no
  // picture at all. Decorative, like a cover: the result's title beside it
  // says what it is.
  import Disc from './Disc.svelte';

  let {
    url,
  }: {
    /** The result's `thumbnail`, if it has one. */
    url: string | null | undefined;
  } = $props();

  /** The address that could not be shown; another one is tried afresh. */
  let failed = $state<string | null>(null);
  const shown = $derived(url && url !== failed ? url : null);
</script>

{#if shown}
  <img
    class="thumbnail"
    src={shown}
    alt=""
    loading="lazy"
    decoding="async"
    width="48"
    height="72"
    onerror={() => (failed = shown)}
  />
{:else}
  <span class="thumbnail none"><Disc size={34} hole="var(--surface-2)" /></span>
{/if}

<style>
  .thumbnail {
    flex: none;
    width: 3rem;
    height: 4.5rem;
    object-fit: contain;
    background: var(--surface-2);
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
  }
  .none {
    display: grid;
    place-items: center;
  }
</style>
