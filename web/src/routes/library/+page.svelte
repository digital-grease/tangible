<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { apiFetch } from '$lib/session';
  import { onMount } from 'svelte';
  import {
    formatBytes,
    formatLabel,
    loadLibrary,
    validationLabel,
    validationMark,
    type ArtifactSummaryPage,
    type LoadState,
  } from '$lib/library';

  let view = $state<LoadState<ArtifactSummaryPage>>({ kind: 'loading' });
  let loadingMore = $state(false);

  onMount(async () => {
    view = await loadLibrary();
  });

  async function loadMore() {
    if (view.kind !== 'ready' || !view.data.next_cursor || loadingMore) return;
    loadingMore = true;
    const next = await loadLibrary(apiFetch, view.data.next_cursor);
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

<!-- One live region for the whole view, so view changes are announced once
     rather than per row. -->
<div aria-live="polite" aria-busy={view.kind === 'loading'}>
  {#if view.kind === 'loading'}
    <p>Loading artifacts…</p>
  {:else if view.kind === 'empty'}
    <section>
      <h2>No artifacts yet</h2>
      <p>Import an authorized disc image to create your first artifact.</p>
      <p><a href="/imports">Import one</a></p>
    </section>
  {:else if view.kind === 'error'}
    <section>
      <h2>Could not load the library</h2>
      <p>{view.problem.title}.</p>
      <!-- Plain language first; the exact detail is available but not in the
           way. -->
      <details>
        <summary>Technical detail</summary>
        <p>{view.problem.detail}</p>
        <p>Error code: <code>{view.problem.code}</code></p>
      </details>
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
