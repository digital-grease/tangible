<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // The disc a burn is a copy of, chosen from the catalog rather than typed
  // as an identifier. A disc already linked to the artifact is chosen for
  // you when there is exactly one; otherwise the catalog is searched by
  // what the artifact is (its request's title), every word matching where a
  // disc sits. When the catalog has no such disc, an operator catalogues
  // one here in one step: a single-disc release, linked to the artifact,
  // started from the request's title, kind and year and the artifact's
  // likely medium.
  //
  // It sits inside the burn form, so it has no form of its own: Enter in its
  // fields searches or catalogues, never queues the burn.
  import { onMount } from 'svelte';
  import Badge from './ui/Badge.svelte';
  import Button from './ui/Button.svelte';
  import Field from './ui/Field.svelte';
  import Select from './ui/Select.svelte';
  import {
    MEDIA_FAMILIES,
    TITLE_KINDS,
    catalogueDisc,
    discPlace,
    findDiscs,
    likelyMedium,
    mediaFamilyLabel,
    titleKindOfRequest,
    type CatalogDiscPage,
    type CatalogDiscView,
  } from '$lib/catalog';
  import { artifactName, type ArtifactDetail, type Problem } from '$lib/library';
  import { sessionRole } from '$lib/session';

  let {
    artifactId,
    artifact,
    discId = $bindable(''),
  }: {
    /** The artifact the burn is for, when one is chosen. */
    artifactId: string;
    /** Its details, to search and catalogue by. */
    artifact: ArtifactDetail | null;
    /** The disc chosen; given to start with one. */
    discId?: string;
  } = $props();

  const mayCatalogue = sessionRole() !== 'viewer';
  const MAX_SEARCH = 200;

  let chosen = $state<CatalogDiscView | null>(null);
  let query = $state('');
  let found = $state<CatalogDiscPage | null>(null);
  let searching = $state(false);
  let problem = $state<Problem | null>(null);
  let said = $state('');

  let creating = $state(false);
  let working = $state(false);
  let title = $state('');
  let kind = $state('unknown');
  let year = $state('');
  let edition = $state('');
  let medium = $state('unknown');

  onMount(async () => {
    if (discId) {
      const result = await findDiscs({ id: discId, artifact: artifactId || undefined });
      const disc = result.kind === 'ready' ? result.data.items[0] : undefined;
      if (disc) {
        chosen = disc;
        return;
      }
      if (result.kind === 'error') problem = result.problem;
      else said = 'No disc in the catalog has that identifier. Choose one here.';
      discId = '';
    }
    // A disc already linked to this artifact is the one it is a copy of.
    if (artifactId) {
      const linked = await findDiscs({ artifact: artifactId, limit: 5 });
      const discs = linked.kind === 'ready' ? linked.data.items.filter((d) => d.linked) : [];
      if (discs.length === 1 && discs[0]) {
        choose(discs[0]);
        return;
      }
    }
    query = (artifact?.request?.title ?? '').slice(0, MAX_SEARCH);
    await search();
  });

  async function search() {
    if (searching) return;
    searching = true;
    problem = null;
    const result = await findDiscs({
      search: query,
      artifact: artifactId || undefined,
      limit: 20,
    });
    searching = false;
    if (result.kind === 'error') {
      problem = result.problem;
      return;
    }
    found = result.data;
  }

  function choose(disc: CatalogDiscView) {
    chosen = disc;
    discId = disc.disc.id;
    creating = false;
    problem = null;
  }

  function chooseAnother() {
    chosen = null;
    discId = '';
    said = '';
    if (!found) void search();
  }

  function beginCreate() {
    creating = true;
    problem = null;
    said = '';
    const request = artifact?.request;
    title = (request?.title ?? (artifact ? artifactName(artifact) : null) ?? query).slice(0, 500);
    kind = titleKindOfRequest(request?.kind);
    year = request?.year && /^\d{4}$/.test(request.year) ? request.year : '';
    medium = artifact ? likelyMedium(artifact) : 'unknown';
    edition = medium === 'unknown' ? '' : mediaFamilyLabel(medium);
  }

  async function create() {
    if (working || !title.trim() || !edition.trim()) return;
    working = true;
    problem = null;
    const result = await catalogueDisc({
      title: title.trim(),
      kind,
      release_year: year ? Number(year) : null,
      edition: edition.trim(),
      media_family: medium,
      artifact_id: artifactId || null,
    });
    working = false;
    if (result.kind === 'error') {
      problem = result.problem;
      return;
    }
    choose(result.data);
    said = `Catalogued ${discPlace(result.data)}.`;
  }

  /** Enter does what this part of the form is for, not queue the burn. */
  function enter(event: KeyboardEvent, then: () => void) {
    if (event.key !== 'Enter') return;
    event.preventDefault();
    then();
  }

  function copies(disc: CatalogDiscView): string {
    const made = disc.disc.copy_count;
    return made === 0 ? 'no copies yet' : made === 1 ? '1 copy made' : `${made} copies made`;
  }
</script>

<div class="picker" role="group" aria-labelledby="disc-picker-label">
  <span id="disc-picker-label" class="label">Disc</span>
  <p class="hint">The catalog disc this copy is of; the burn is recorded against it.</p>

  {#if chosen}
    <div class="chosen">
      <div class="what">
        <span class="place">{discPlace(chosen)}</span>
        <span class="quiet">{mediaFamilyLabel(chosen.disc.media_family)} · {copies(chosen)}</span>
      </div>
      {#if chosen.linked}<Badge icon="link">Linked to this artifact</Badge>{/if}
      <Button size="sm" onclick={chooseAnother}>Choose another disc</Button>
    </div>
  {:else if creating}
    <div class="create" role="group" aria-label="Catalogue a new disc">
      <p class="quiet">
        A single-disc release{artifactId ? ', linked to this artifact' : ''}. A title already in the
        catalog by the same name, kind and year gains this edition.
      </p>
      <Field label="Title">
        {#snippet children({ id, describedBy })}
          <input
            {id}
            bind:value={title}
            maxlength="500"
            aria-describedby={describedBy}
            onkeydown={(event) => enter(event, create)}
          />
        {/snippet}
      </Field>
      <div class="row">
        <Select label="Kind of work" options={TITLE_KINDS} bind:value={kind} />
        <Field label="Year">
          {#snippet children({ id, describedBy })}
            <input
              {id}
              type="number"
              min="1870"
              max="2200"
              bind:value={year}
              aria-describedby={describedBy}
              onkeydown={(event) => enter(event, create)}
            />
          {/snippet}
        </Field>
      </div>
      <div class="row">
        <Field label="Edition" hint="What this release is called, such as DVD or PAL DVD.">
          {#snippet children({ id, describedBy })}
            <input
              {id}
              bind:value={edition}
              maxlength="500"
              aria-describedby={describedBy}
              onkeydown={(event) => enter(event, create)}
            />
          {/snippet}
        </Field>
        <Select label="Medium" options={MEDIA_FAMILIES} bind:value={medium} />
      </div>
      <div class="actions">
        <Button
          variant="primary"
          onclick={create}
          disabled={working || !title.trim() || !edition.trim()}
        >
          {working ? 'Cataloguing…' : 'Catalogue and use this disc'}
        </Button>
        <Button onclick={() => (creating = false)}>Not now</Button>
      </div>
    </div>
  {:else}
    <div class="search" role="search">
      <Field label="Find it in the catalog">
        {#snippet children({ id, describedBy })}
          <input
            {id}
            type="search"
            bind:value={query}
            maxlength={MAX_SEARCH}
            autocomplete="off"
            aria-describedby={describedBy}
            onkeydown={(event) => enter(event, search)}
          />
        {/snippet}
      </Field>
      <Button icon="search" onclick={search} disabled={searching}>
        {searching ? 'Searching…' : 'Search'}
      </Button>
    </div>
    <p class="hint">Every word must be in its title, edition, set, name or year.</p>
    {#if found}
      {#if found.items.length === 0}
        <p class="quiet">No disc in the catalog matches.</p>
      {:else}
        <ul class="found" aria-label="Discs found">
          {#each found.items as disc (disc.disc.id)}
            <li>
              <div class="what">
                <span class="place">{discPlace(disc)}</span>
                <span class="quiet"
                  >{mediaFamilyLabel(disc.disc.media_family)} · {copies(disc)}</span
                >
              </div>
              {#if disc.linked}<Badge icon="link">Linked to this artifact</Badge>{/if}
              <Button size="sm" onclick={() => choose(disc)} label={`Use ${discPlace(disc)}`}
                >Use this disc</Button
              >
            </li>
          {/each}
        </ul>
        {#if found.more}
          <p class="quiet">More discs match. Add a word to narrow the search.</p>
        {/if}
      {/if}
    {/if}
    {#if mayCatalogue}
      <p>
        <Button size="sm" icon="plus" onclick={beginCreate}>Catalogue a new disc…</Button>
      </p>
    {/if}
  {/if}

  <div role="status">
    {#if said}<p class="quiet">{said}</p>{/if}
  </div>
  <div role="alert">
    {#if problem}<p class="problem">{problem.detail}</p>{/if}
  </div>
</div>

<style>
  .picker {
    display: grid;
    gap: var(--space-2);
    margin-top: var(--space-2);
  }
  .label,
  .picker :global(label) {
    font-size: var(--text-sm);
  }
  .hint,
  .quiet {
    margin: 0;
    font-size: var(--text-sm);
    color: var(--text-muted);
  }
  .chosen,
  .found li {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2) var(--space-3);
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--line);
    border-radius: var(--radius);
  }
  .chosen {
    border-left: 4px solid var(--accent);
    background: var(--surface-2);
  }
  .what {
    flex: 1;
    display: grid;
    min-width: 12rem;
  }
  .place {
    font-weight: var(--weight-medium);
    overflow-wrap: anywhere;
  }
  .search {
    display: flex;
    flex-wrap: wrap;
    align-items: flex-end;
    gap: var(--space-2);
  }
  .search :global(.field) {
    flex: 1;
    min-width: 14rem;
  }
  .found {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: var(--space-2);
  }
  .create {
    display: grid;
    gap: var(--space-2);
    padding: var(--space-3);
    background: var(--surface-2);
    border-radius: var(--radius);
  }
  .row {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(12rem, 1fr));
    gap: var(--space-2) var(--space-3);
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
  }
  .problem {
    margin: 0;
    color: var(--bad);
  }
  p {
    margin: 0;
  }
</style>
