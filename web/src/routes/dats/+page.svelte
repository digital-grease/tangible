<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Preservation DATs (docs/plans/dat-matching.md): loading the Logiqx XML
  // DATs an administrator has, seeing what each identifies, and removing
  // one. Tangible never downloads a DAT, and a match renames nothing.
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import {
    MAX_DAT_BYTES,
    datSummary,
    loadDats,
    loadOutcome,
    removeDat,
    uploadDat,
    type Dat,
    type DatLoad,
  } from '$lib/identification';
  import { formatBytes } from '$lib/library';
  import { sessionRole, type Problem } from '$lib/session';
  import { onMount } from 'svelte';

  type View =
    { kind: 'loading' } | { kind: 'ready'; items: Dat[] } | { kind: 'error'; problem: Problem };

  let view = $state<View>({ kind: 'loading' });
  let chosen = $state<File | null>(null);
  let loading = $state(false);
  let outcome = $state<DatLoad | null>(null);
  let loadProblem = $state<Problem | null>(null);
  /** Removing is a second, deliberate action. */
  let confirmingRemove = $state<string | null>(null);
  let removeProblem = $state<Problem | null>(null);
  let done = $state<string | null>(null);

  const administrator = sessionRole() === 'administrator';

  async function refresh() {
    const result = await loadDats();
    view = result.kind === 'ok' ? { kind: 'ready', items: result.data.items } : result;
  }

  onMount(refresh);

  function choose(event: Event) {
    const input = event.currentTarget as HTMLInputElement;
    chosen = input.files?.[0] ?? null;
    outcome = null;
    loadProblem = null;
  }

  async function load(event: SubmitEvent) {
    event.preventDefault();
    if (!chosen || loading) return;
    loading = true;
    outcome = null;
    loadProblem = null;
    done = null;
    const result = await uploadDat(chosen);
    loading = false;
    if (result.kind === 'error') {
      loadProblem = result.problem;
      return;
    }
    outcome = result.data;
    await refresh();
  }

  async function remove(dat: Dat) {
    removeProblem = null;
    const result = await removeDat(dat.id);
    if (result.kind === 'error') {
      removeProblem = result.problem;
      return;
    }
    confirmingRemove = null;
    outcome = null;
    done = `Removed ${dat.name}, and the matches it made.`;
    await refresh();
  }

  function when(value: string): string {
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
  }
</script>

<svelte:head><title>DATs — Tangible</title></svelte:head>

<h1>DATs</h1>

<p>
  A DAT lists the known good dumps of a system's discs, by the hashes of their files and tracks.
  Load the Logiqx XML DATs you use, from Redump, No-Intro or wherever you trust, and every artifact
  in the library is matched against them: whether it is a known good dump, and of what. Tangible
  never downloads a DAT, and a match renames and changes nothing.
</p>

{#if administrator}
  <section aria-labelledby="load-heading">
    <h2 id="load-heading">Load a DAT</h2>
    <form onsubmit={load}>
      <p>
        <label for="dat-file">Logiqx XML DAT</label><br />
        <input
          id="dat-file"
          type="file"
          accept=".dat,.xml,application/xml,text/xml"
          aria-describedby="dat-file-help"
          onchange={choose}
        />
        <br />
        <small id="dat-file-help"
          >Up to {formatBytes(MAX_DAT_BYTES)}, unzipped. A DAT with the name of one already loaded
          replaces it, so loading a newer one updates the old.</small
        >
      </p>
      <p>
        <button type="submit" disabled={!chosen || loading}>
          {loading ? 'Loading and matching…' : 'Load'}
        </button>
      </p>
    </form>
    <div aria-live="polite">
      {#if outcome}
        <p><strong>{loadOutcome(outcome)}</strong></p>
        {#if outcome.warnings.length > 0}
          <details>
            <summary>What was left out</summary>
            <ul>
              {#each outcome.warnings as warning, index (index)}
                <li>{warning}</li>
              {/each}
            </ul>
            {#if outcome.warning_count > outcome.warnings.length}
              <p>And {outcome.warning_count - outcome.warnings.length} more.</p>
            {/if}
          </details>
        {/if}
      {/if}
      {#if done}<p>{done}</p>{/if}
    </div>
    {#if loadProblem}
      <ErrorPanel problem={loadProblem} area="library" refusal />
    {/if}
  </section>
{/if}

<section aria-labelledby="loaded-heading" aria-live="polite" aria-busy={view.kind === 'loading'}>
  <h2 id="loaded-heading">Loaded DATs</h2>
  {#if view.kind === 'loading'}
    <p>Loading DATs…</p>
  {:else if view.kind === 'error'}
    <ErrorPanel
      problem={view.problem}
      area="library"
      endpoint="GET /api/v1/dats"
      heading="DATs could not be loaded"
      level={3}
    />
  {:else if view.items.length === 0}
    <p>
      No DATs loaded yet{#if administrator}; load one above{:else}; an administrator loads them{/if}.
    </p>
  {:else}
    <table>
      <caption class="visually-hidden">Loaded DATs</caption>
      <thead>
        <tr>
          <th scope="col">Name</th>
          <th scope="col">Version</th>
          <th scope="col">Holds</th>
          <th scope="col">Loaded</th>
          {#if administrator}<th scope="col">Actions</th>{/if}
        </tr>
      </thead>
      <tbody>
        {#each view.items as dat (dat.id)}
          <tr>
            <th scope="row">
              {dat.name}
              {#if dat.description && dat.description !== dat.name}<br /><small
                  >{dat.description}</small
                >{/if}
            </th>
            <td>{dat.version ?? '—'}</td>
            <td>{datSummary(dat)}</td>
            <td>{when(dat.loaded_at)}<br /><small>by {dat.loaded_by}</small></td>
            {#if administrator}
              <td>
                {#if confirmingRemove === dat.id}
                  <p>
                    <strong>Remove {dat.name}?</strong> Its games and the matches it made are deleted.
                    Nothing in the library is touched, and loading it again restores them.
                  </p>
                  <button type="button" onclick={() => remove(dat)}>Yes, remove {dat.name}</button>
                  <button type="button" onclick={() => (confirmingRemove = null)}>Keep it</button>
                  {#if removeProblem}
                    <ErrorPanel problem={removeProblem} area="library" refusal />
                  {/if}
                {:else}
                  <button
                    type="button"
                    onclick={() => {
                      confirmingRemove = dat.id;
                      removeProblem = null;
                    }}
                    aria-label={`Remove ${dat.name}`}>Remove</button
                  >
                {/if}
              </td>
            {/if}
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</section>

<style>
  table {
    border-collapse: collapse;
    width: 100%;
  }
  th,
  td {
    text-align: left;
    padding: 0.5rem;
    border-bottom: 1px solid currentColor;
    vertical-align: top;
  }
  .visually-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
  }
</style>
