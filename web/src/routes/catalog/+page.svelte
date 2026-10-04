<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
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

<h1>Catalog</h1>

<p>
  A disc is the thing this system is about. A title is the work, an edition is a particular release
  of it, a set is what that release shipped as, and a disc is one physical thing in that set.
</p>

{#if problem}
  <section class="refusal" aria-live="assertive" aria-labelledby="refusal-heading">
    <h2 id="refusal-heading">{problem.title}</h2>
    <p>{problem.detail}</p>
    <p>Error code: <code>{problem.code}</code></p>
  </section>
{/if}

<section aria-labelledby="titles-heading">
  <h2 id="titles-heading">Titles</h2>

  <form
    onsubmit={(event) => {
      event.preventDefault();
      refreshTitles();
    }}
  >
    <label for="search">Search</label>
    <input id="search" bind:value={search} placeholder="Part of a name" />
    <button type="submit">Search</button>
  </form>

  <div aria-live="polite" aria-busy={view.kind === 'loading'}>
    {#if view.kind === 'loading'}
      <p>Loading titles…</p>
    {:else if view.kind === 'empty'}
      <p>Nothing catalogued yet. Add the first title below.</p>
    {:else if view.kind === 'error'}
      <p>{view.problem.title}. {view.problem.detail}</p>
    {:else}
      <ul class="chain">
        {#each view.data.items as title (title.id)}
          <li>
            <button
              type="button"
              class:selected={selectedTitle === title.id}
              onclick={() => chooseTitle(title.id)}
            >
              {title.display_title}
            </button>
            <small>
              {titleKindLabel(title.kind)}{title.release_year ? `, ${title.release_year}` : ''} —
              {title.edition_count} edition{title.edition_count === 1 ? '' : 's'}
            </small>
          </li>
        {/each}
      </ul>
    {/if}
  </div>

  <form onsubmit={addTitle}>
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
    <button type="submit" disabled={busy || !newTitle.trim()}>Add</button>
  </form>
</section>

{#if selectedTitle}
  <section aria-labelledby="editions-heading">
    <h2 id="editions-heading">Editions</h2>
    {#if editions.length === 0}
      <p>No editions yet. A release of this title goes here.</p>
    {:else}
      <ul class="chain">
        {#each editions as edition (edition.id)}
          <li>
            <button
              type="button"
              class:selected={selectedEdition === edition.id}
              onclick={() => chooseEdition(edition.id)}
            >
              {edition.display_name}
            </button>
            <small>
              {edition.region ?? 'Region unspecified'} — {edition.set_count} set{edition.set_count ===
              1
                ? ''
                : 's'}
            </small>
          </li>
        {/each}
      </ul>
    {/if}

    <form onsubmit={addEdition}>
      <label for="new-edition">New edition</label>
      <input id="new-edition" bind:value={newEdition} placeholder="Special Edition" />
      <button type="submit" disabled={busy || !newEdition.trim()}>Add</button>
    </form>
  </section>
{/if}

{#if selectedEdition}
  <RommExport editionId={selectedEdition} />

  <section aria-labelledby="sets-heading">
    <h2 id="sets-heading">Disc sets</h2>
    {#if sets.length === 0}
      <p>No sets yet. What this edition shipped as goes here.</p>
    {:else}
      <ul class="chain">
        {#each sets as set (set.id)}
          <li>
            <button
              type="button"
              class:selected={selectedSet === set.id}
              onclick={() => chooseSet(set.id)}
            >
              {set.name}
            </button>
            <small>
              {set.disc_count} of {set.disc_count_expected ?? '?'} discs catalogued
              {#if !isComplete(set)}
                — incomplete
              {/if}
            </small>
          </li>
        {/each}
      </ul>
    {/if}

    <form onsubmit={addSet}>
      <label for="new-set">New set</label>
      <input id="new-set" bind:value={newSet} placeholder="Two-disc set" />
      <input
        type="number"
        bind:value={newSetCount}
        min="1"
        placeholder="Discs"
        aria-label="How many discs it shipped with"
      />
      <button type="submit" disabled={busy || !newSet.trim()}>Add</button>
    </form>
  </section>
{/if}

{#if selectedSet}
  <section aria-labelledby="discs-heading">
    <h2 id="discs-heading">Discs</h2>
    {#if discs.length === 0}
      <p>No discs catalogued in this set yet.</p>
    {:else}
      <table>
        <caption class="visually-hidden">Discs in this set</caption>
        <thead>
          <tr>
            <th scope="col">Disc</th>
            <th scope="col">Medium</th>
            <th scope="col">Images</th>
            <th scope="col">Copies made</th>
            <th scope="col">Reproduction</th>
            <th scope="col"></th>
          </tr>
        </thead>
        <tbody>
          {#each discs as disc (disc.id)}
            <tr>
              <th scope="row">{discName(disc)}</th>
              <td>{mediaFamilyLabel(disc.media_family)}</td>
              <td>{disc.artifact_count}</td>
              <td>{disc.copy_count}</td>
              <td>{compatibilityLabel(disc.compatibility_claim)}</td>
              <td>
                <a href={`/burns/new?disc=${disc.id}`}>Burn to this disc</a>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}

    <form onsubmit={addDisc}>
      <label for="new-disc-number">New disc</label>
      <input
        id="new-disc-number"
        type="number"
        bind:value={newDiscNumber}
        min="1"
        aria-label="Disc number"
      />
      <input bind:value={newDiscName} placeholder="Feature, Supplements…" aria-label="Disc name" />
      <select bind:value={newDiscMedia} aria-label="Medium">
        <option value="unknown">Unspecified</option>
        <option value="cd">CD</option>
        <option value="dvd">DVD</option>
        <option value="bluray">Blu-ray</option>
        <option value="uhd_bluray">UHD Blu-ray</option>
        <option value="gd_rom">GD-ROM</option>
      </select>
      <button type="submit" disabled={busy}>Add</button>
    </form>
  </section>
{/if}

<style>
  ul.chain {
    list-style: none;
    margin: 0 0 1rem;
    padding: 0;
  }
  ul.chain li {
    padding: 0.25rem 0;
  }
  ul.chain button {
    font: inherit;
    padding: 0.25rem 0.5rem;
    background: none;
    border: 1px solid currentColor;
  }
  /* The selection is carried by more than styling: the button keeps its own
     pressed state for assistive technology. */
  ul.chain button.selected {
    font-weight: 700;
    text-decoration: underline;
  }
  table {
    border-collapse: collapse;
    width: 100%;
  }
  th,
  td {
    text-align: left;
    padding: 0.5rem;
    border-bottom: 1px solid currentColor;
  }
  form {
    margin: 0.75rem 0;
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
    align-items: center;
  }
  label {
    font-weight: 600;
  }
  input,
  select {
    padding: 0.35rem;
  }
  button[type='submit'] {
    padding: 0.35rem 0.75rem;
  }
  .refusal {
    border: 2px solid currentColor;
    padding: 0.75rem 1rem;
  }
  .visually-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
  }
  small {
    margin-left: 0.5rem;
  }
</style>
