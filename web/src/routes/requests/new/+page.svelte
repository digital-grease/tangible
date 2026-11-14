<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Ask for an item (ADR 0021). The requester finds it in the metadata
  // (loaded DATs, and MusicBrainz, TMDB or IGDB where configured) or
  // describes it by hand, and attests, by a box they tick themselves on
  // every request, that they own it and want a digital image of it. The
  // server refuses a request without it and does not keep it.
  //
  // The search is a field of the request's form, not a form of its own
  // (forms do not nest): its button is not a submit button, and Enter in it
  // searches rather than asking.
  import { goto } from '$app/navigation';
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import Callout from '$lib/components/ui/Callout.svelte';
  import Checkbox from '$lib/components/ui/Checkbox.svelte';
  import Field from '$lib/components/ui/Field.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import Badge from '$lib/components/ui/Badge.svelte';
  import Select from '$lib/components/ui/Select.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import {
    ATTESTATION,
    DETAIL_FIELDS,
    KINDS,
    candidateLine,
    createRequest,
    detailLabel,
    imdbId,
    isFormDetail,
    kindLabel,
    providerLabel,
    requestFields,
    requestRefusal,
    searchMetadata,
    sourceNote,
    type DetailName,
    type MetadataCandidate,
    type MetadataSearchView,
  } from '$lib/requests';
  import type { Problem } from '$lib/session';

  /** As the server bounds them. */
  const MAX_TITLE = 300;
  const MAX_DETAIL = 200;
  const MAX_NOTE = 2000;

  let kind = $state('game');
  let title = $state('');
  let details = $state<Record<DetailName, string>>({
    platform: '',
    year: '',
    region: '',
    edition: '',
  });
  let imdb = $state('');
  let note = $state('');
  let attests = $state(false);
  let submitting = $state(false);
  let tried = $state(false);
  let problem = $state<Problem | null>(null);

  let query = $state('');
  let searching = $state(false);
  let found = $state<MetadataSearchView | null>(null);
  let searchProblem = $state<Problem | null>(null);
  /** The candidate the request is made from, if one was picked. */
  let picked = $state<MetadataCandidate | null>(null);
  /** The title field, to take the keyboard there once one is picked. */
  let titleInput = $state<HTMLInputElement | null>(null);

  async function find() {
    const words = query.trim();
    if (!words || searching) return;
    searching = true;
    searchProblem = null;
    found = null;
    // An IMDb ID or address names its own kind.
    const result = await searchMetadata(words, imdbId(words) ? null : kind);
    searching = false;
    if (result.kind === 'error') {
      searchProblem = result.problem;
      return;
    }
    found = result.data;
  }

  function searchKey(event: KeyboardEvent) {
    if (event.key === 'Enter') {
      event.preventDefault();
      void find();
    }
  }

  function pick(candidate: MetadataCandidate) {
    picked = candidate;
    kind = candidate.kind;
    title = candidate.title;
    for (const name of Object.keys(details) as DetailName[]) {
      details[name] = candidate.details[name] ?? '';
    }
    const fromImdb = candidate.refs.find((ref) => ref.provider === 'imdb');
    imdb = fromImdb ? fromImdb.external_id : '';
    found = null;
    titleInput?.focus();
  }

  function forget() {
    picked = null;
  }

  /** A picked candidate's details the form has no field for. */
  const extra = $derived(
    picked ? Object.entries(picked.details).filter(([name]) => !isFormDetail(name)) : [],
  );

  const screen = $derived(kind === 'movie' || kind === 'television');
  const imdbFound = $derived(screen ? imdbId(imdb) : null);
  const titleError = $derived(tried && !title.trim() ? 'Say what the item is.' : null);
  const imdbError = $derived(
    imdbFound === undefined
      ? 'Not an IMDb title ID (tt followed by digits) or IMDb address.'
      : null,
  );
  const attestError = $derived(tried && !attests ? 'A request cannot be made without this.' : null);

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (submitting) return;
    tried = true;
    problem = null;
    if (!title.trim() || imdbError || !attests) return;
    const fields = requestFields(picked, details, imdbFound ?? null);
    submitting = true;
    const result = await createRequest({
      kind,
      title: title.trim(),
      details: fields.details,
      refs: fields.refs,
      note: note.trim() || null,
      attests_ownership: attests,
    });
    submitting = false;
    if (result.kind === 'error') {
      problem = result.problem;
      return;
    }
    await goto(`/requests/${result.data.id}`);
  }
</script>

<svelte:head><title>Ask for an item — Tangible</title></svelte:head>

<PageHeader
  title="Ask for an item"
  crumbs={[{ label: 'Requests', href: '/requests' }, { label: 'Ask for an item' }]}
  description="Ask for a digital image of an item you own. Find it, or describe it yourself. An operator or administrator approves the request before any release of it is searched for or downloaded."
/>

<form class="layout" onsubmit={submit} novalidate>
  <section aria-labelledby="item-heading" class="panel">
    <h2 id="item-heading">The item</h2>
    <div class="fields">
      <Select label="Kind" options={KINDS} bind:value={kind} />
      <div class="find" role="search" aria-label="Find the item">
        <Field
          label="Find it"
          hint="Words of its title (and, for music, the artist), or an IMDb ID or address for a film or programme."
        >
          {#snippet children({ id, describedBy })}
            <div class="find-row">
              <input
                {id}
                type="search"
                bind:value={query}
                maxlength={200}
                autocomplete="off"
                aria-describedby={describedBy}
                onkeydown={searchKey}
              />
              <Button
                icon="search"
                onclick={find}
                disabled={searching || !query.trim()}
                label={`Search for it among ${kindLabel(kind).toLowerCase()} sources`}
                >{searching ? 'Searching…' : 'Search'}</Button
              >
            </div>
          {/snippet}
        </Field>
        <div aria-live="polite" aria-busy={searching}>
          {#if searchProblem}
            <ErrorPanel
              problem={searchProblem}
              area="other"
              endpoint="GET /api/v1/metadata/search"
              level={3}
              refusal
            />
          {/if}
          {#if found}
            {@const notes = found.sources
              .map(sourceNote)
              .filter((note): note is string => note !== null)}
            {#if found.candidates.length === 0}
              <p class="quiet">Nothing was found. Try other words, or describe it below.</p>
            {:else}
              <p class="quiet">
                {found.candidates.length} found. Use one to fill in the request, or describe it below.
              </p>
              <ul class="candidates" aria-label="What was found">
                {#each found.candidates as candidate, index (index)}
                  <li>
                    <div class="candidate">
                      <span class="candidate-title">{candidate.title}</span>
                      <span class="sub">{candidateLine(candidate)}</span>
                    </div>
                    <div class="candidate-actions">
                      {#if candidate.link}
                        <a href={candidate.link} rel="noreferrer noopener" target="_blank"
                          >See it at {providerLabel(candidate.provider)}<span
                            class="visually-hidden"
                          >
                            (opens a new tab)</span
                          ></a
                        >
                      {/if}
                      <Button
                        size="sm"
                        onclick={() => pick(candidate)}
                        label={`Use this: ${candidate.title}, ${candidateLine(candidate)}`}
                        >Use this</Button
                      >
                    </div>
                  </li>
                {/each}
              </ul>
            {/if}
            {#if notes.length > 0}
              <ul class="notes" aria-label="Sources that did not answer">
                {#each notes as note (note)}
                  <li><StatusMark tone="warn" label={note} /></li>
                {/each}
              </ul>
            {/if}
          {/if}
        </div>
      </div>
      {#if picked}
        <div class="picked">
          <p>
            <Badge>{providerLabel(picked.provider)}</Badge>
            Filled in from {providerLabel(picked.provider)}. Change anything below as you need.
          </p>
          {#if extra.length > 0}
            <dl class="extra">
              {#each extra as [name, value] (name)}
                <div>
                  <dt>{detailLabel(name)}</dt>
                  <dd>{value}</dd>
                </div>
              {/each}
            </dl>
          {/if}
          <Button size="sm" variant="quiet" onclick={forget}>Do not use what was found</Button>
        </div>
      {/if}
      <Field label="Title" required error={titleError} hint="As it appears on the item.">
        {#snippet children({ id, describedBy, invalid })}
          <input
            {id}
            bind:this={titleInput}
            bind:value={title}
            required
            maxlength={MAX_TITLE}
            autocomplete="off"
            aria-describedby={describedBy}
            aria-invalid={invalid || undefined}
          />
        {/snippet}
      </Field>
      <div class="pairs">
        {#each DETAIL_FIELDS as field (field.name)}
          <Field label={field.label} hint={field.hint}>
            {#snippet children({ id, describedBy })}
              <input
                {id}
                bind:value={details[field.name]}
                maxlength={MAX_DETAIL}
                autocomplete="off"
                aria-describedby={describedBy}
              />
            {/snippet}
          </Field>
        {/each}
      </div>
      {#if screen}
        <Field
          label="IMDb title"
          hint="Optional: its IMDb ID, such as tt0083658, or its IMDb address. It makes finding the right release likelier."
          error={imdbError}
        >
          {#snippet children({ id, describedBy, invalid })}
            <input
              {id}
              bind:value={imdb}
              autocomplete="off"
              spellcheck="false"
              aria-describedby={describedBy}
              aria-invalid={invalid || undefined}
            />
          {/snippet}
        </Field>
      {/if}
      <Field label="Note" hint="Optional: anything that helps, such as which disc of a set.">
        {#snippet children({ id, describedBy })}
          <textarea
            {id}
            bind:value={note}
            rows="3"
            maxlength={MAX_NOTE}
            aria-describedby={describedBy}></textarea>
        {/snippet}
      </Field>
    </div>
  </section>

  <section aria-labelledby="attest-heading" class="panel">
    <h2 id="attest-heading">Before you ask</h2>
    <Callout tone="info">
      Whether making a copy of an item is lawful where you are is for you to determine. Tangible
      does not check what you attest.
    </Callout>
    <div class="attest" class:invalid={!!attestError}>
      <Checkbox label={ATTESTATION} bind:checked={attests} />
      {#if attestError}<p class="error"><StatusMark tone="bad" label={attestError} /></p>{/if}
    </div>
    {#if problem}
      {@const advice = requestRefusal(problem.code)}
      {#if advice}<p class="advice">{advice}</p>{/if}
      <ErrorPanel {problem} area="other" endpoint="POST /api/v1/requests" level={3} refusal />
    {/if}
    <Button type="submit" variant="primary" icon="request" disabled={submitting}>
      {submitting ? 'Asking…' : 'Ask for this item'}
    </Button>
  </section>
</form>

<style>
  .layout {
    display: grid;
    grid-template-columns: minmax(0, 1.6fr) minmax(0, 1fr);
    gap: var(--space-4);
    align-items: start;
  }
  @media (max-width: 1099px) {
    .layout {
      grid-template-columns: minmax(0, 1fr);
    }
  }
  .panel {
    display: grid;
    gap: var(--space-3);
    padding: var(--space-3) var(--space-4) var(--space-4);
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--radius-lg);
  }
  .panel h2 {
    margin: 0;
    font-size: var(--text-base);
  }
  .fields {
    display: grid;
    gap: var(--space-3);
  }
  .pairs {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(14rem, 1fr));
    gap: var(--space-3);
  }
  .attest {
    padding: var(--space-3);
    border: 1px solid var(--line);
    border-radius: var(--radius);
  }
  .attest.invalid {
    border-color: var(--bad);
  }
  .find {
    display: grid;
    gap: var(--space-2);
    padding: var(--space-3);
    background: var(--surface-2);
    border-radius: var(--radius);
  }
  .find-row {
    display: flex;
    gap: var(--space-2);
  }
  .find-row input {
    flex: 1;
    min-width: 0;
  }
  .candidates,
  .notes {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: var(--space-2);
  }
  .candidates li {
    display: flex;
    flex-wrap: wrap;
    justify-content: space-between;
    gap: var(--space-2);
    padding: var(--space-2) 0;
    border-bottom: 1px solid var(--line-2);
  }
  .candidates li:last-child {
    border-bottom: 0;
  }
  .candidate {
    display: grid;
    min-width: 0;
  }
  .candidate-title {
    font-weight: var(--weight-medium);
    overflow-wrap: anywhere;
  }
  .candidate-actions {
    display: flex;
    gap: var(--space-3);
    align-items: center;
    font-size: var(--text-sm);
  }
  .sub {
    font-size: var(--text-sm);
    color: var(--text-muted);
    overflow-wrap: anywhere;
  }
  .notes :global(.mark) {
    white-space: normal;
  }
  .quiet {
    margin: 0;
    color: var(--text-muted);
  }
  .picked {
    display: grid;
    gap: var(--space-2);
    padding: var(--space-3);
    border: 1px solid var(--line);
    border-left: 4px solid var(--accent);
    border-radius: var(--radius);
  }
  .picked p {
    margin: 0;
  }
  .extra {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(12rem, 1fr));
    gap: var(--space-2) var(--space-4);
    margin: 0;
  }
  .extra dt {
    font-size: var(--text-sm);
    color: var(--text-muted);
  }
  .extra dd {
    margin: 0;
    overflow-wrap: anywhere;
  }
  .error {
    margin: var(--space-2) 0 0 1.5rem;
    font-size: var(--text-sm);
  }
  .advice {
    margin: 0;
  }
</style>
