<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import CatalogEditor from '$lib/components/CatalogEditor.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import Panel from '$lib/components/ui/Panel.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import Table from '$lib/components/ui/Table.svelte';
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import { apiFetch } from '$lib/session';
  import RommExport from '$lib/components/RommExport.svelte';
  import { onMount } from 'svelte';
  import {
    compatibilityLabel,
    createDisc,
    createDiscSet,
    createEdition,
    createTitle,
    discName,
    isComplete,
    loadDiscSets,
    loadDiscs,
    loadEditions,
    loadTitles,
    mediaFamilyLabel,
    titleKindLabel,
    type DiscSetView,
    type DiscView,
    type EditionView,
    type TitlePage,
  } from '$lib/catalog';
  import type { LoadState, Problem } from '$lib/library';

  let view = $state<LoadState<TitlePage>>({ kind: 'loading' });
  let problem = $state<Problem | null>(null);
  let busy = $state(false);
  let search = $state('');

  /**
   * The chain, one level at a time.
   *
   * Progressive rather than four pages: a disc only means something as "disc
   * two of the two-disc special edition of this film", and choosing it that
   * way is how somebody actually knows which disc they mean.
   */
  let selectedTitle = $state<string | null>(null);
  let editions = $state<EditionView[]>([]);
  let selectedEdition = $state<string | null>(null);
  let sets = $state<DiscSetView[]>([]);
  let selectedSet = $state<string | null>(null);
  let discs = $state<DiscView[]>([]);

  /** What is being changed, one thing at a time. */
  let editing = $state<
    | { kind: 'title'; id: string }
    | { kind: 'edition'; id: string }
    | { kind: 'set'; id: string }
    | { kind: 'disc'; id: string }
    | null
  >(null);

  const editedTitle = $derived(
    editing?.kind === 'title' && view.kind === 'ready'
      ? view.data.items.find((title) => title.id === editing?.id)
      : undefined,
  );
  const editedEdition = $derived(
    editing?.kind === 'edition'
      ? editions.find((edition) => edition.id === editing?.id)
      : undefined,
  );
  const editedSet = $derived(
    editing?.kind === 'set' ? sets.find((set) => set.id === editing?.id) : undefined,
  );
  const editedDisc = $derived(
    editing?.kind === 'disc' ? discs.find((disc) => disc.id === editing?.id) : undefined,
  );

  /** Saved: show it as it now is, keeping what was chosen. */
  async function saved() {
    const was = editing;
    editing = null;
    if (was?.kind === 'title') await refreshTitles();
    if (was?.kind === 'edition' && selectedTitle) {
      const result = await loadEditions(selectedTitle);
      if (result.kind === 'ready') editions = result.data;
    }
    if (was?.kind === 'set' && selectedEdition) {
      const result = await loadDiscSets(selectedEdition);
      if (result.kind === 'ready') sets = result.data;
    }
    if (was?.kind === 'disc' && selectedSet) {
      const result = await loadDiscs(selectedSet);
      if (result.kind === 'ready') discs = result.data;
    }
  }

  let newTitle = $state('');
  let newTitleKind = $state('unknown');
  let newEdition = $state('');
  let newSet = $state('');
  let newSetCount = $state<number | null>(null);
  let newDiscNumber = $state(1);
  let newDiscName = $state('');
  let newDiscMedia = $state('unknown');

  async function refreshTitles() {
    view = await loadTitles(apiFetch, search.trim() || undefined);
  }

  onMount(refreshTitles);

  async function chooseTitle(id: string) {
    selectedTitle = id;
    selectedEdition = null;
    selectedSet = null;
    editions = [];
    sets = [];
    discs = [];
    const result = await loadEditions(id);
    if (result.kind === 'ready') editions = result.data;
    else problem = result.problem;
  }

  async function chooseEdition(id: string) {
    selectedEdition = id;
    selectedSet = null;
    sets = [];
    discs = [];
    const result = await loadDiscSets(id);
    if (result.kind === 'ready') sets = result.data;
    else problem = result.problem;
  }

  async function chooseSet(id: string) {
    selectedSet = id;
    discs = [];
    const result = await loadDiscs(id);
    if (result.kind === 'ready') {
      discs = result.data;
      // A helpful default rather than a decision: the next number in the set.
      newDiscNumber = discs.length + 1;
    } else {
      problem = result.problem;
    }
  }

  async function addTitle(event: SubmitEvent) {
    event.preventDefault();
    if (busy || !newTitle.trim()) return;
    busy = true;
    problem = null;
    const result = await createTitle({ display_title: newTitle.trim(), kind: newTitleKind });
    if (result.kind === 'error') {
      problem = result.problem;
    } else {
      newTitle = '';
      await refreshTitles();
      await chooseTitle(result.data.id);
    }
    busy = false;
  }

  async function addEdition(event: SubmitEvent) {
    event.preventDefault();
    if (busy || !selectedTitle || !newEdition.trim()) return;
    busy = true;
    problem = null;
    const result = await createEdition(selectedTitle, { display_name: newEdition.trim() });
    if (result.kind === 'error') {
      problem = result.problem;
    } else {
      newEdition = '';
      await chooseTitle(selectedTitle);
      await chooseEdition(result.data.id);
    }
    busy = false;
  }

  async function addSet(event: SubmitEvent) {
    event.preventDefault();
    if (busy || !selectedEdition || !newSet.trim()) return;
    busy = true;
    problem = null;
    const result = await createDiscSet(selectedEdition, {
      name: newSet.trim(),
      disc_count_expected: newSetCount,
    });
    if (result.kind === 'error') {
      problem = result.problem;
    } else {
      newSet = '';
      newSetCount = null;
      await chooseEdition(selectedEdition);
      await chooseSet(result.data.id);
    }
    busy = false;
  }

  async function addDisc(event: SubmitEvent) {
    event.preventDefault();
    if (busy || !selectedSet) return;
    busy = true;
    problem = null;
    const result = await createDisc(selectedSet, {
      sequence_number: newDiscNumber,
      display_name: newDiscName.trim() || null,
      media_family: newDiscMedia,
    });
    if (result.kind === 'error') {
      problem = result.problem;
    } else {
      newDiscName = '';
      await chooseSet(selectedSet);
    }
    busy = false;
  }
</script>

<svelte:head><title>Catalog — Tangible</title></svelte:head>

<PageHeader
  title="Catalog"
  description="A disc is the thing this system is about. A title is the work, an edition is a particular release of it, a set is what that release shipped as, and a disc is one physical thing in that set."
/>

{#if problem}
  <ErrorPanel {problem} area="catalog" refusal />
{/if}

<!-- The chain as columns, each choice opening the next. The chosen item is
     a pressed button, so the choice is said, not only shown. -->
<div class="columns">
  <Panel title="Titles">
    <form
      class="search"
      onsubmit={(event) => {
        event.preventDefault();
        refreshTitles();
      }}
    >
      <label for="search" class="visually-hidden">Search</label>
      <input id="search" type="search" bind:value={search} placeholder="Part of a name" />
      <Button type="submit" size="sm">Search</Button>
    </form>

    <div aria-live="polite" aria-busy={view.kind === 'loading'}>
      {#if view.kind === 'loading'}
        <p class="quiet">Loading titles…</p>
      {:else if view.kind === 'empty'}
        <p class="quiet">Nothing catalogued yet. Add the first title below.</p>
      {:else if view.kind === 'error'}
        <p>{view.problem.title}. {view.problem.detail}</p>
      {:else}
        <ul class="chain">
          {#each view.data.items as title (title.id)}
            <li>
              <button
                type="button"
                aria-pressed={selectedTitle === title.id}
                onclick={() => chooseTitle(title.id)}
              >
                <span class="name">{title.display_title}</span>
                <span class="meta">
                  {titleKindLabel(title.kind)}{title.release_year ? `, ${title.release_year}` : ''} ·
                  {title.edition_count} edition{title.edition_count === 1 ? '' : 's'}
                </span>
              </button>
            </li>
          {/each}
        </ul>
        {#if selectedTitle && editing?.kind !== 'title'}
          <Button
            size="sm"
            variant="quiet"
            onclick={() => (editing = { kind: 'title', id: selectedTitle! })}
          >
            Change this title…
          </Button>
        {/if}
        {#if editedTitle}
          <CatalogEditor
            kind="title"
            item={editedTitle}
            onsaved={saved}
            oncancel={() => (editing = null)}
          />
        {/if}
      {/if}
    </div>

    <form class="add" onsubmit={addTitle}>
      <label for="new-title">New title</label>
      <input id="new-title" bind:value={newTitle} placeholder="Example Film" />
      <select bind:value={newTitleKind} aria-label="Kind of work">
        <option value="unknown">Unspecified</option>
        <option value="movie">Film</option>
        <option value="television">Television</option>
        <option value="game">Game</option>
        <option value="software">Software</option>
        <option value="operating_system">Operating system</option>
        <option value="music">Music</option>
        <option value="data_archive">Data archive</option>
      </select>
      <Button type="submit" size="sm" disabled={busy || !newTitle.trim()}>Add</Button>
    </form>
  </Panel>

  <Panel title="Editions">
    {#if !selectedTitle}
      <p class="quiet">Choose a title to see its editions.</p>
    {:else}
      {#if editions.length === 0}
        <p class="quiet">No editions yet. A release of this title goes here.</p>
      {:else}
        <ul class="chain">
          {#each editions as edition (edition.id)}
            <li>
              <button
                type="button"
                aria-pressed={selectedEdition === edition.id}
                onclick={() => chooseEdition(edition.id)}
              >
                <span class="name">{edition.display_name}</span>
                <span class="meta">
                  {edition.region ?? 'Region unspecified'} · {edition.set_count} set{edition.set_count ===
                  1
                    ? ''
                    : 's'}
                </span>
              </button>
            </li>
          {/each}
        </ul>
        {#if selectedEdition && editing?.kind !== 'edition'}
          <Button
            size="sm"
            variant="quiet"
            onclick={() => (editing = { kind: 'edition', id: selectedEdition! })}
          >
            Change this edition…
          </Button>
        {/if}
        {#if editedEdition}
          <CatalogEditor
            kind="edition"
            item={editedEdition}
            onsaved={saved}
            oncancel={() => (editing = null)}
          />
        {/if}
      {/if}

      <form class="add" onsubmit={addEdition}>
        <label for="new-edition">New edition</label>
        <input id="new-edition" bind:value={newEdition} placeholder="Special Edition" />
        <Button type="submit" size="sm" disabled={busy || !newEdition.trim()}>Add</Button>
      </form>
    {/if}
  </Panel>

  <Panel title="Disc sets">
    {#if !selectedEdition}
      <p class="quiet">Choose an edition to see what it shipped as.</p>
    {:else}
      {#if sets.length === 0}
        <p class="quiet">No sets yet. What this edition shipped as goes here.</p>
      {:else}
        <ul class="chain">
          {#each sets as set (set.id)}
            <li>
              <button
                type="button"
                aria-pressed={selectedSet === set.id}
                onclick={() => chooseSet(set.id)}
              >
                <span class="name">{set.name}</span>
                <span class="meta">
                  {set.disc_count} of {set.disc_count_expected ?? '?'} discs catalogued
                </span>
              </button>
              {#if !isComplete(set)}
                <StatusMark tone="warn" label="Incomplete" />
              {/if}
            </li>
          {/each}
        </ul>
        {#if selectedSet && editing?.kind !== 'set'}
          <Button
            size="sm"
            variant="quiet"
            onclick={() => (editing = { kind: 'set', id: selectedSet! })}
          >
            Change this set…
          </Button>
        {/if}
        {#if editedSet}
          <CatalogEditor
            kind="set"
            item={editedSet}
            onsaved={saved}
            oncancel={() => (editing = null)}
          />
        {/if}
      {/if}

      <form class="add" onsubmit={addSet}>
        <label for="new-set">New set</label>
        <input id="new-set" bind:value={newSet} placeholder="Two-disc set" />
        <input
          type="number"
          bind:value={newSetCount}
          min="1"
          placeholder="Discs"
          aria-label="How many discs it shipped with"
        />
        <Button type="submit" size="sm" disabled={busy || !newSet.trim()}>Add</Button>
      </form>
    {/if}
  </Panel>
</div>

{#if selectedEdition}
  <div class="romm">
    <RommExport editionId={selectedEdition} />
  </div>
{/if}

{#if selectedSet}
  <div class="discs">
    <Panel title="Discs" aside={String(discs.length)} flush>
      {#if discs.length === 0}
        <p class="quiet padded">No discs catalogued in this set yet.</p>
      {:else}
        <Table caption="Discs in this set">
          <thead>
            <tr>
              <th scope="col">Disc</th>
              <th scope="col">Medium</th>
              <th scope="col" class="num">Images</th>
              <th scope="col" class="num">Copies made</th>
              <th scope="col">Reproduction</th>
              <th scope="col"><span class="visually-hidden">Actions</span></th>
            </tr>
          </thead>
          <tbody>
            {#each discs as disc (disc.id)}
              <tr>
                <th scope="row">{discName(disc)}</th>
                <td>{mediaFamilyLabel(disc.media_family)}</td>
                <td class="num">{disc.artifact_count}</td>
                <td class="num">{disc.copy_count}</td>
                <td>{compatibilityLabel(disc.compatibility_claim)}</td>
                <td class="actions">
                  <Button size="sm" icon="burn" href={`/burns/new?disc=${disc.id}`}
                    >Burn to this disc</Button
                  >
                  <Button
                    size="sm"
                    variant="quiet"
                    onclick={() => (editing = { kind: 'disc', id: disc.id })}
                  >
                    Change {discName(disc)}…
                  </Button>
                </td>
              </tr>
            {/each}
          </tbody>
        </Table>
      {/if}
      {#if editedDisc}
        <div class="padded">
          <CatalogEditor
            kind="disc"
            item={editedDisc}
            onsaved={saved}
            oncancel={() => (editing = null)}
          />
        </div>
      {/if}

      <form class="add padded" onsubmit={addDisc}>
        <label for="new-disc-number">New disc</label>
        <input
          id="new-disc-number"
          type="number"
          bind:value={newDiscNumber}
          min="1"
          aria-label="Disc number"
        />
        <input
          bind:value={newDiscName}
          placeholder="Feature, Supplements…"
          aria-label="Disc name"
        />
        <select bind:value={newDiscMedia} aria-label="Medium">
          <option value="unknown">Unspecified</option>
          <option value="cd">CD</option>
          <option value="dvd">DVD</option>
          <option value="bluray">Blu-ray</option>
          <option value="uhd_bluray">UHD Blu-ray</option>
          <option value="gd_rom">GD-ROM</option>
        </select>
        <Button type="submit" size="sm" disabled={busy}>Add</Button>
      </form>
    </Panel>
  </div>
{/if}

<style>
  .columns {
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: var(--space-4);
    align-items: start;
  }
  @media (max-width: 1099px) {
    .columns {
      grid-template-columns: minmax(0, 1fr);
    }
  }
  .search,
  .add {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    align-items: center;
  }
  .search {
    margin-bottom: var(--space-3);
  }
  .search input,
  .add input:not([type='number']) {
    flex: 1 1 8rem;
    min-width: 0;
  }
  .add input[type='number'] {
    width: 5rem;
  }
  .add {
    margin-top: var(--space-3);
    padding-top: var(--space-3);
    border-top: 1px solid var(--line-2);
  }
  .add label {
    width: 100%;
    font-size: var(--text-sm);
    color: var(--text-muted);
  }
  ul.chain {
    list-style: none;
    margin: 0 0 var(--space-2);
    padding: 0;
    display: grid;
    gap: 2px;
  }
  ul.chain li {
    display: flex;
    gap: var(--space-2);
    align-items: center;
  }
  ul.chain button {
    flex: 1;
    display: grid;
    justify-content: start;
    justify-items: start;
    gap: 0;
    min-height: 0;
    padding: var(--space-2) var(--space-3);
    border: 1px solid transparent;
    border-radius: var(--radius);
    background: none;
    text-align: left;
  }
  ul.chain button:hover {
    background: var(--surface-2);
  }
  /* aria-pressed is the truth; the background, bar and weight echo it. */
  ul.chain button[aria-pressed='true'] {
    background: var(--surface-2);
    border-color: var(--line);
    box-shadow: inset 3px 0 0 var(--accent);
  }
  ul.chain button[aria-pressed='true'] .name {
    font-weight: var(--weight-strong);
  }
  .name {
    font-weight: var(--weight-medium);
  }
  .meta {
    font-size: var(--text-xs);
    font-weight: var(--weight-regular);
    color: var(--text-muted);
  }
  .actions {
    display: flex;
    gap: var(--space-2);
    justify-content: flex-end;
    white-space: nowrap;
  }
  .romm,
  .discs {
    margin-top: var(--space-5);
  }
  .padded {
    padding: var(--space-3) var(--space-4);
  }
  .quiet {
    color: var(--text-muted);
    margin: 0;
  }
</style>
