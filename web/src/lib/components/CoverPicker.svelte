<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // An artifact's cover, and picking one by hand (ADR 0023): search the
  // metadata providers as the request form does, and pick what the
  // artifact is. Tangible fetches the cover in the background, and a picked
  // cover comes before the cover of the request the artifact fulfilled.
  // Operators and administrators pick; everyone sees where a cover came
  // from.
  import { onDestroy } from 'svelte';
  import Button from './ui/Button.svelte';
  import Field from './ui/Field.svelte';
  import Select from './ui/Select.svelte';
  import StatusMark from './ui/StatusMark.svelte';
  import {
    coverSourceLabel,
    pickArtwork,
    unpickArtwork,
    type CoverView,
    type Problem,
  } from '$lib/library';
  import {
    KINDS,
    candidateLine,
    searchMetadata,
    type MetadataCandidate,
    type MetadataSearchView,
  } from '$lib/requests';
  import { sessionRole } from '$lib/session';

  let {
    artifactId,
    name,
    cover,
    picked,
    onchanged,
  }: {
    artifactId: string;
    /** What to search for at first: the artifact's name. */
    name: string;
    cover: CoverView | null | undefined;
    /** Whether the cover was picked by hand. */
    picked: boolean;
    onchanged: () => void | Promise<void>;
  } = $props();

  const mayPick = sessionRole() !== 'viewer';
  const MAX_QUERY = 200;

  let open = $state(false);
  let query = $state('');
  let kind = $state('');
  let searching = $state(false);
  let found = $state<MetadataSearchView | null>(null);
  let problem = $state<Problem | null>(null);
  let working = $state(false);
  let said = $state('');
  let waiting: ReturnType<typeof setTimeout> | undefined;

  onDestroy(() => clearTimeout(waiting));

  function begin() {
    open = true;
    query = name.slice(0, MAX_QUERY);
    found = null;
    problem = null;
    said = '';
  }

  async function search(event: SubmitEvent) {
    event.preventDefault();
    if (searching || !query.trim()) return;
    searching = true;
    problem = null;
    const result = await searchMetadata(query.trim(), kind || null);
    searching = false;
    if (result.kind === 'error') {
      problem = result.problem;
      return;
    }
    found = result.data;
  }

  /** Follow the artifact a few times while its new cover is fetched. */
  function follow(times: number) {
    clearTimeout(waiting);
    if (times <= 0) return;
    waiting = setTimeout(() => {
      void onchanged();
      follow(times - 1);
    }, 3_000);
  }

  async function pick(candidate: MetadataCandidate) {
    if (working) return;
    working = true;
    problem = null;
    const result = await pickArtwork(artifactId, candidate.refs);
    working = false;
    if (result.kind === 'error') {
      problem = result.problem;
      return;
    }
    open = false;
    said = `Picked ${candidate.title}. Its cover appears once Tangible has fetched it.`;
    await onchanged();
    follow(5);
  }

  async function remove() {
    if (working) return;
    working = true;
    problem = null;
    const result = await unpickArtwork(artifactId);
    working = false;
    if (result.kind === 'error') {
      problem = result.problem;
      return;
    }
    said = 'The picked cover is removed.';
    await onchanged();
  }

  /** Candidates a cover can be fetched for: those with a provider's reference. */
  function hasCover(candidate: MetadataCandidate): boolean {
    return candidate.refs.some((ref) =>
      ['tmdb', 'imdb', 'musicbrainz', 'igdb'].includes(ref.provider),
    );
  }
</script>

<section aria-labelledby="cover-heading" class="cover-picker">
  <h3 id="cover-heading">Cover</h3>
  <p class="quiet">
    {#if cover}
      {picked ? 'Picked by hand, from' : 'From'} {coverSourceLabel(cover.source)}.
    {:else if picked}
      Picked by hand; Tangible has not fetched it yet.
    {:else}
      None yet. A request's item brings its cover; any other can be picked.
    {/if}
  </p>
  {#if mayPick && !open}
    <div class="actions">
      <Button size="sm" icon="search" onclick={begin} disabled={working}>
        {cover ? 'Pick another cover…' : 'Find cover art…'}
      </Button>
      {#if picked}
        <Button size="sm" onclick={remove} disabled={working}>Remove the picked cover</Button>
      {/if}
    </div>
  {/if}
  {#if open}
    <form class="search" onsubmit={search} aria-label="Find cover art">
      <Field label="What is it?" hint="Its title, as the providers would name it.">
        {#snippet children({ id, describedBy })}
          <input
            {id}
            type="search"
            bind:value={query}
            maxlength={MAX_QUERY}
            autocomplete="off"
            aria-describedby={describedBy}
          />
        {/snippet}
      </Field>
      <Select
        label="Kind"
        options={[{ value: '', label: 'Any kind' }, ...KINDS]}
        bind:value={kind}
      />
      <div class="actions">
        <Button type="submit" variant="primary" icon="search" disabled={searching}>
          {searching ? 'Searching…' : 'Search'}
        </Button>
        <Button onclick={() => (open = false)}>Not now</Button>
      </div>
    </form>
    {#if found}
      {@const usable = found.candidates.filter(hasCover)}
      {#if usable.length === 0}
        <p class="quiet">Nothing found with a cover. Try other words, or another kind.</p>
      {:else}
        <ul class="candidates" aria-label="What it might be">
          {#each usable as candidate, index (index)}
            <li>
              <span class="title">{candidate.title}</span>
              <span class="quiet small">{candidateLine(candidate)}</span>
              <Button
                size="sm"
                onclick={() => pick(candidate)}
                disabled={working}
                label={`Use the cover of ${candidate.title}`}>Use this cover</Button
              >
            </li>
          {/each}
        </ul>
      {/if}
    {/if}
  {/if}
  <div role="status">
    {#if said}<p><StatusMark tone="ok" label={said} /></p>{/if}
  </div>
  <div role="alert">
    {#if problem}<p class="problem">{problem.detail}</p>{/if}
  </div>
</section>

<style>
  .cover-picker {
    display: grid;
    gap: var(--space-2);
    margin: 0 0 var(--space-4);
  }
  h3 {
    margin: 0;
    font-size: var(--text-base);
  }
  .quiet {
    margin: 0;
    color: var(--text-muted);
  }
  .small {
    font-size: var(--text-sm);
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
  }
  .search {
    display: grid;
    gap: var(--space-2);
    max-width: 36rem;
  }
  .candidates {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: var(--space-2);
    max-width: 48rem;
  }
  .candidates li {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    gap: 0 var(--space-3);
    align-items: center;
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--line);
    border-radius: var(--radius);
  }
  .candidates .small {
    grid-column: 1;
  }
  .candidates :global(button) {
    grid-column: 2;
    grid-row: 1 / span 2;
  }
  .title {
    font-weight: var(--weight-medium);
    overflow-wrap: anywhere;
  }
  .problem {
    margin: 0;
    color: var(--bad);
  }
</style>
