<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Finding a release for an approved request (ADR 0021,
  // docs/plans/requests.md Q4), for operators and administrators: search
  // Prowlarr, by the request's title or other words; the releases found,
  // best first, each saying why it ranks where it does; and Grab on one,
  // which asks first and names the download client it goes to. Nothing is
  // downloaded until a person grabs. The server holds a search for an hour,
  // so it is loaded again when the page is.
  import { onMount } from 'svelte';
  import ErrorPanel from './ErrorPanel.svelte';
  import Button from './ui/Button.svelte';
  import Callout from './ui/Callout.svelte';
  import Confirm from './ui/Confirm.svelte';
  import EmptyState from './ui/EmptyState.svelte';
  import Field from './ui/Field.svelte';
  import Panel from './ui/Panel.svelte';
  import Select from './ui/Select.svelte';
  import StatusMark from './ui/StatusMark.svelte';
  import Table from './ui/Table.svelte';
  import { formatBytes } from '$lib/library';
  import {
    clientsFor,
    grabOutcome,
    grabRelease,
    loadReleaseSearch,
    protocolLabel,
    releaseAge,
    releasePeers,
    releaseRefusal,
    searchReleases,
    searchedFor,
    type GrabView,
    type ReleaseSearchView,
    type ReleaseView,
    type RequestView,
  } from '$lib/requests';
  import type { Problem } from '$lib/session';

  let {
    request,
    ongrabbed,
  }: {
    /** The request, approved or with a failed download. */
    request: RequestView;
    /** Called with what a grab answered: the request, now downloading. */
    ongrabbed: (grab: GrabView) => void;
  } = $props();

  const MAX_WORDS = 200;

  let held = $state<ReleaseSearchView | null>(null);
  let words = $state('');
  let searching = $state(false);
  let problem = $state<{ problem: Problem; endpoint: string } | null>(null);
  let picked = $state<ReleaseView | null>(null);
  let client = $state('');
  let grabbing = $state(false);
  let said = $state<string | null>(null);

  const advice = $derived(problem ? releaseRefusal(problem.problem.code) : null);
  const filmById = $derived(
    request.kind === 'movie' && request.refs.some((ref) => ref.provider === 'imdb'),
  );
  const pickedClients = $derived(held && picked ? clientsFor(held, picked.protocol) : []);
  const pausedNames = $derived(held ? held.indexers_paused.map((paused) => paused.name) : []);

  onMount(async () => {
    const result = await loadReleaseSearch(request.id);
    // None held is the usual case, and says nothing to anybody.
    if (result.kind === 'ok') held = result.data;
  });

  async function search(event: SubmitEvent) {
    event.preventDefault();
    if (searching) return;
    searching = true;
    problem = null;
    said = null;
    picked = null;
    const result = await searchReleases(request.id, words);
    searching = false;
    if (result.kind === 'error') {
      problem = { problem: result.problem, endpoint: 'POST /api/v1/requests/{id}/search' };
      return;
    }
    held = result.data;
    const found = result.data.results.length;
    said = found === 1 ? 'Found one release.' : `Found ${found} releases.`;
  }

  function pick(release: ReleaseView) {
    picked = release;
    problem = null;
    said = null;
    client = held ? (clientsFor(held, release.protocol)[0]?.id ?? '') : '';
  }

  async function grab() {
    if (!picked || grabbing) return;
    grabbing = true;
    problem = null;
    const choices = pickedClients;
    // Named only when there is a choice: the server takes the only one.
    const result = await grabRelease(request.id, picked.id, choices.length > 1 ? client : null);
    grabbing = false;
    if (result.kind === 'error') {
      problem = { problem: result.problem, endpoint: 'POST /api/v1/requests/{id}/grab' };
      return;
    }
    const name =
      choices.find((choice) => choice.id === result.data.integration_id)?.name ??
      'the download client';
    said = grabOutcome(result.data, name);
    picked = null;
    ongrabbed(result.data);
  }

  function time(value: string): string {
    return new Date(value).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  }

  function size(bytes: number | null | undefined): string {
    return bytes == null ? 'Not said' : formatBytes(bytes);
  }
</script>

<Panel title="Find a release">
  <p class="intro">
    Searches the indexers in Prowlarr for this request. Nothing is downloaded until you pick a
    release and grab it.
  </p>
  <form class="search" onsubmit={search} aria-label="Search for releases">
    <Field
      label="Words to search for"
      hint={filmById
        ? 'Left empty, the film is searched for by its IMDb ID, then by its title.'
        : "Left empty, the request's title, without what is in brackets."}
    >
      {#snippet children({ id, describedBy })}
        <div class="search-row">
          <input
            {id}
            type="search"
            autocomplete="off"
            maxlength={MAX_WORDS}
            placeholder={request.title}
            bind:value={words}
            aria-describedby={describedBy}
          />
          <Button type="submit" variant="primary" icon="search" disabled={searching}>
            {searching ? 'Searching…' : 'Search'}
          </Button>
        </div>
      {/snippet}
    </Field>
  </form>

  {#if problem}
    {#if advice}<Callout tone="bad"><p>{advice}</p></Callout>{/if}
    <ErrorPanel problem={problem.problem} area="other" endpoint={problem.endpoint} refusal />
  {/if}

  {#if held}
    <p class="quiet held">
      {searchedFor(held)} at {time(held.searched_at)}. Held until {time(held.held_until)}; search
      again after that to grab one.
    </p>
    {#if held.indexers_enabled === 0}
      <Callout tone="warn" role="note">
        <p>Prowlarr has no indexer turned on, so nothing can be found. Add one in Prowlarr.</p>
      </Callout>
    {/if}
    {#if pausedNames.length > 0}
      <Callout tone="warn" role="note">
        <p>
          Prowlarr has paused {pausedNames.join(', ')} after
          {pausedNames.length === 1 ? 'it' : 'they'} failed, so what
          {pausedNames.length === 1 ? 'it has' : 'they have'} is missing here. Search again later.
        </p>
      </Callout>
    {/if}
    {#if held.results.length === 0}
      <EmptyState inset level={3} title="Nothing found" icon="search">
        <p>Try other words: fewer of them, or as a release would name it.</p>
      </EmptyState>
    {:else}
      <Table caption="Releases found, best first">
        <thead>
          <tr>
            <th scope="col">Release</th>
            <th scope="col">Indexer</th>
            <th scope="col">Size</th>
            <th scope="col">Peers</th>
            <th scope="col">Age</th>
            <th scope="col"><span class="visually-hidden">Actions</span></th>
          </tr>
        </thead>
        <tbody>
          {#each held.results as release (release.id)}
            <tr>
              <th scope="row" class="release">
                <span class="title">{release.title}</span>
                {#if release.categories.length > 0}
                  <span class="sub">{release.categories.join(', ')}</span>
                {/if}
                {#if release.reasons.length > 0}
                  <ul class="why" aria-label={`Why ${release.title} ranks here`}>
                    {#each release.reasons as reason (reason)}<li>{reason}</li>{/each}
                  </ul>
                {/if}
              </th>
              <td>
                {release.indexer}
                <span class="sub">{protocolLabel(release.protocol)}</span>
              </td>
              <td class="number">{size(release.size_bytes)}</td>
              <td>{releasePeers(release)}</td>
              <td>
                {#if release.published_at}
                  <time datetime={release.published_at}>{releaseAge(release.published_at)}</time>
                {:else}
                  <span class="quiet">Not said</span>
                {/if}
              </td>
              <td>
                {#if release.can_grab}
                  <Button
                    size="sm"
                    onclick={() => pick(release)}
                    disabled={grabbing}
                    label={`Grab: ${release.title}`}>Grab</Button
                  >
                {:else}
                  <span class="quiet small"
                    >No {release.protocol === 'usenet' ? 'Usenet' : 'torrent'} client</span
                  >
                {/if}
              </td>
            </tr>
          {/each}
        </tbody>
      </Table>
    {/if}
  {/if}

  {#if picked}
    {@const release = picked}
    <div class="padded">
      <Confirm title="Grab this release?">
        <p>
          <strong>{release.title}</strong>, {size(release.size_bytes)}, from {release.indexer}. Its
          {release.protocol === 'torrent' ? '.torrent or magnet link' : 'NZB'} is fetched through Prowlarr
          and handed to
          {pickedClients.length === 1
            ? (pickedClients[0]?.name ?? 'a download client')
            : 'the client chosen'}, which downloads it; the request is fulfilled once it is in the
          library.
        </p>
        {#if pickedClients.length > 1}
          <Select
            label="Download client"
            options={pickedClients.map((choice) => ({ value: choice.id, label: choice.name }))}
            bind:value={client}
          />
        {/if}
        {#snippet actions()}
          <Button variant="primary" onclick={grab} disabled={grabbing}>
            {grabbing ? 'Grabbing…' : 'Yes, grab it'}
          </Button>
          <Button onclick={() => (picked = null)} disabled={grabbing}>Not this one</Button>
        {/snippet}
      </Confirm>
    </div>
  {/if}

  <div aria-live="polite">
    {#if said}
      <p class="said"><StatusMark tone="ok" label={said} /></p>
    {/if}
  </div>
</Panel>

<style>
  .intro {
    margin: 0 0 var(--space-3);
    color: var(--text-muted);
    max-width: 75ch;
  }
  .search {
    max-width: 44rem;
    margin-bottom: var(--space-3);
  }
  .search-row {
    display: flex;
    gap: var(--space-2);
  }
  .search-row input {
    flex: 1;
    min-width: 0;
  }
  .held {
    margin: var(--space-3) 0;
  }
  .release {
    font-weight: var(--weight-regular);
    min-width: 16rem;
  }
  .title {
    display: block;
    font-weight: var(--weight-medium);
    overflow-wrap: anywhere;
  }
  .sub {
    display: block;
    font-size: var(--text-sm);
    color: var(--text-muted);
  }
  .why {
    margin: var(--space-1) 0 0;
    padding-left: var(--space-4);
    font-size: var(--text-sm);
    font-weight: var(--weight-regular);
    color: var(--text-muted);
  }
  .number {
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
  }
  .padded {
    padding: var(--space-3) 0;
  }
  .quiet {
    color: var(--text-muted);
  }
  .small {
    font-size: var(--text-sm);
  }
  .said {
    margin: var(--space-3) 0 0;
  }
  .said :global(.mark) {
    white-space: normal;
  }
</style>
