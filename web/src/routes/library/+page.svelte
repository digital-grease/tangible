<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import { bestMatchLabel } from '$lib/identification';
  import { apiFetch } from '$lib/session';
  import { onMount } from 'svelte';
  import {
    deletionLabel,
    filtersNothing,
    formatBytes,
    formatLabel,
    loadLibrary,
    validationLabel,
    validationMark,
    type ArtifactSummaryPage,
    type LibraryFilter,
    type LoadState,
  } from '$lib/library';

  let view = $state<LoadState<ArtifactSummaryPage>>({ kind: 'loading' });
  let loadingMore = $state(false);

  // What the form says, and what the list shown was loaded with: "Show
  // more" keeps the latter, whatever has been changed in the form since.
  let format = $state('');
  let origin = $state('');
  let validation = $state('');
  let integrity = $state('');
  let linked = $state('');
  let deleted = $state(false);
  let shown = $state<LibraryFilter>({});

  function chosen(): LibraryFilter {
    return {
      format,
      origin,
      validation_state: validation,
      integrity_state: integrity,
      linked: linked === '' ? null : linked === 'true',
      deleted,
    };
  }

  async function show(filter: LibraryFilter) {
    shown = filter;
    view = { kind: 'loading' };
    view = await loadLibrary(apiFetch, undefined, filter);
  }

  onMount(() => show({}));

  function filter(event: SubmitEvent) {
    event.preventDefault();
    void show(chosen());
  }

  function clear() {
    format = origin = validation = integrity = linked = '';
    deleted = false;
    void show({});
  }

  async function loadMore() {
    if (view.kind !== 'ready' || !view.data.next_cursor || loadingMore) return;
    loadingMore = true;
    const next = await loadLibrary(apiFetch, view.data.next_cursor, shown);
    if (next.kind === 'ready' && view.kind === 'ready') {
      // Append rather than replace: paging through a library should not lose
      // what the operator has already scrolled past.
      view = {
        kind: 'ready',
        data: {
          items: [...view.data.items, ...next.data.items],
          next_cursor: next.data.next_cursor,
        },
      };
    } else if (next.kind === 'error') {
      view = next;
    }
    loadingMore = false;
  }
</script>

<svelte:head><title>Library — Tangible</title></svelte:head>

<h1>Library</h1>

<form class="filters" onsubmit={filter} aria-label="Filter the library">
  <label>
    Format
    <select bind:value={format}>
      <option value="">Any</option>
      <option value="iso">ISO</option>
      <option value="cue_bin">CUE/BIN</option>
      <option value="toc_bin">TOC/BIN</option>
      <option value="ccd_img_sub">CloneCD (CCD/IMG/SUB)</option>
      <option value="mds_mdf">Alcohol 120% (MDS/MDF)</option>
      <option value="chd">CHD</option>
      <option value="video_ts_directory">VIDEO_TS</option>
      <option value="unknown">Unrecognised</option>
    </select>
  </label>
  <label>
    Where it came from
    <select bind:value={origin}>
      <option value="">Anywhere</option>
      <option value="imported_original">Imported</option>
      <option value="derived">Made here from another</option>
      <option value="physical_dump">Ripped from a disc</option>
    </select>
  </label>
  <label>
    Validation
    <select bind:value={validation}>
      <option value="">Any</option>
      <option value="valid">Valid</option>
      <option value="valid_with_warnings">Valid, with warnings</option>
      <option value="invalid">Invalid</option>
      <option value="pending">Not validated yet</option>
      <option value="unsupported">Not validated here</option>
    </select>
  </label>
  <label>
    Stored bytes
    <select bind:value={integrity}>
      <option value="">Any</option>
      <option value="intact">Checked and intact</option>
      <option value="damaged">Damaged</option>
      <option value="unchecked">Not checked yet</option>
    </select>
  </label>
  <label>
    Disc
    <select bind:value={linked}>
      <option value="">Linked or not</option>
      <option value="true">Linked to a disc</option>
      <option value="false">Not linked to any disc</option>
    </select>
  </label>
  <label>
    <input type="checkbox" bind:checked={deleted} />
    Only deleted artifacts
  </label>
  <button type="submit">Show</button>
  {#if !filtersNothing(shown)}
    <button type="button" onclick={clear}>Show everything</button>
  {/if}
</form>

<!-- One live region for the whole view, so view changes are announced once
     rather than per row. -->
<div aria-live="polite" aria-busy={view.kind === 'loading'}>
  {#if view.kind === 'loading'}
    <p>Loading artifacts…</p>
  {:else if view.kind === 'empty' && !filtersNothing(shown)}
    <p>Nothing in the library matches these filters.</p>
  {:else if view.kind === 'empty'}
    <section>
      <h2>No artifacts yet</h2>
      <p>Import an authorized disc image to create your first artifact.</p>
      <p><a href="/imports">Import one</a></p>
    </section>
  {:else if view.kind === 'error'}
    <section>
      <ErrorPanel
        problem={view.problem}
        area="library"
        endpoint="GET /api/v1/artifacts"
        heading="Could not load the library"
      />
    </section>
  {:else}
    <p>{view.data.items.length} artifact{view.data.items.length === 1 ? '' : 's'}.</p>

    <table>
      <caption class="visually-hidden">Artifacts in the library</caption>
      <thead>
        <tr>
          <th scope="col">Name</th>
          <th scope="col">Format</th>
          <th scope="col">Size</th>
          <th scope="col">Files</th>
          <th scope="col">Validation</th>
          <th scope="col">Identified as</th>
        </tr>
      </thead>
      <tbody>
        {#each view.data.items as artifact (artifact.id)}
          <tr>
            <th scope="row">
              <a href={`/library/${artifact.id}`}>
                {artifact.source_filename ?? artifact.id}
              </a>
            </th>
            <td>{formatLabel(artifact.format)}</td>
            <td>{formatBytes(artifact.total_bytes)}</td>
            <td>{artifact.component_count}</td>
            <td>
              <!-- A symbol carries the status alongside any styling, so it is
                   never communicated by colour alone. -->
              <span aria-hidden="true">{validationMark(artifact.validation_state)}</span>
              {validationLabel(artifact.validation_state)}
              {#if artifact.integrity_state === 'damaged'}
                <!-- Damage found by an integrity check outranks how the
                     bytes looked when they arrived, so it is said here. -->
                <br /><strong><span aria-hidden="true">✕</span> Damaged</strong>
              {/if}
              {#if deletionLabel(artifact.deletion)}
                <br /><strong>{deletionLabel(artifact.deletion)}</strong>
              {/if}
            </td>
            <td>
              {#if artifact.best_match}
                <!-- The DAT is named in the title as well as the cell, so the
                     column stays short. -->
                <span title={`In ${artifact.best_match.dat_name}`}
                  >{bestMatchLabel(artifact.best_match)}</span
                >
              {:else}
                <span class="none">Not identified</span>
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>

    {#if view.data.next_cursor}
      <button type="button" onclick={loadMore} disabled={loadingMore}>
        {loadingMore ? 'Loading…' : 'Load more'}
      </button>
    {/if}
  {/if}
</div>

<style>
  .filters {
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem 1rem;
    align-items: end;
    margin-bottom: 1rem;
  }
  .filters label {
    display: flex;
    flex-direction: column;
    font-weight: 600;
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
  .none {
    opacity: 0.75;
  }
  .visually-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
  }
  button {
    margin-top: 1rem;
    padding: 0.5rem 1rem;
  }
</style>
