<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import {
    canCancel,
    canRetry,
    cancelImport,
    importFromWatchedRoot,
    importStateLabel,
    importStateMark,
    loadImportSources,
    loadImports,
    retryImport,
    transferFraction,
    uploadImport,
    type ImportPage,
    type ImportSources,
  } from '$lib/imports';
  import { formatBytes, type LoadState, type Problem } from '$lib/library';

  let view = $state<LoadState<ImportPage>>({ kind: 'loading' });
  let sources = $state<ImportSources | null>(null);
  let problem = $state<Problem | null>(null);
  let busy = $state(false);

  let files = $state<FileList | null>(null);
  let pathId = $state('');
  let relativePath = $state('');

  let timer: ReturnType<typeof setInterval> | undefined;

  /**
   * How often the list reconciles itself.
   *
   * A poll, not a stream, and the same fallback the burn queue uses: the
   * event stream is the intended mechanism and this is what it replaces.
   */
  const RECONCILE_MS = 2_000;

  async function refresh() {
    const next = await loadImports();
    // A failed poll leaves what is on screen alone. Replacing a readable list
    // with an error because one refresh missed would be worse than stale.
    if (next.kind === 'error' && view.kind !== 'loading') return;
    view = next;
  }

  onMount(async () => {
    const found = await loadImportSources();
    if (found.kind === 'ready') {
      sources = found.data;
      pathId = found.data.watch_roots[0] ?? '';
    }
    await refresh();
    timer = setInterval(refresh, RECONCILE_MS);
  });

  onDestroy(() => {
    if (timer) clearInterval(timer);
  });

  async function upload(event: SubmitEvent) {
    event.preventDefault();
    const file = files?.[0];
    if (!file || busy) return;
    busy = true;
    problem = null;

    const result = await uploadImport(file);
    if (result.kind === 'error') {
      problem = result.problem;
    } else {
      files = null;
      await refresh();
    }
    busy = false;
  }

  async function importWatched(event: SubmitEvent) {
    event.preventDefault();
    if (busy) return;
    busy = true;
    problem = null;

    const result = await importFromWatchedRoot({
      path_id: pathId,
      relative_path: relativePath.trim(),
    });
    if (result.kind === 'error') {
      problem = result.problem;
    } else {
      relativePath = '';
      await refresh();
    }
    busy = false;
  }

  async function cancel(id: string) {
    busy = true;
    problem = null;
    const result = await cancelImport(id);
    if (result.kind === 'error') problem = result.problem;
    await refresh();
    busy = false;
  }

  async function retry(id: string) {
    busy = true;
    problem = null;
    const result = await retryImport(id);
    if (result.kind === 'error') problem = result.problem;
    await refresh();
    busy = false;
  }
</script>

<svelte:head><title>Imports — Tangible</title></svelte:head>

<h1>Imports</h1>

<p>
  Importing preserves what you give it. The bytes are stored exactly as received, hashed, and
  described; nothing is repaired, recompressed or renamed.
</p>

{#if problem}
  <section class="refusal" aria-live="assertive" aria-labelledby="refusal-heading">
    <h2 id="refusal-heading">{problem.title}</h2>
    <p>{problem.detail}</p>
    <p>Error code: <code>{problem.code}</code></p>
  </section>
{/if}

<section aria-labelledby="upload-heading">
  <h2 id="upload-heading">Upload a file</h2>
  <form onsubmit={upload}>
    <p>
      <label for="file">Disc image</label><br />
      <input id="file" type="file" bind:files />
    </p>
    {#if sources}
      <p>
        <small>This server accepts uploads up to {formatBytes(sources.max_upload_bytes)}.</small>
      </p>
    {/if}
    <button type="submit" disabled={busy || !files?.length}>
      {busy ? 'Working…' : 'Upload and import'}
    </button>
  </form>
</section>

<section aria-labelledby="watched-heading">
  <h2 id="watched-heading">Import from a watched folder</h2>
  {#if !sources || sources.watch_roots.length === 0}
    <!-- Configuration, not a failure: an operator who has not set any roots
         should be told how, not shown a broken form. -->
    <p>
      No watched folders are configured. An administrator can add them with
      <code>TANGIBLE_WATCH_ROOTS</code>, as <code>id=/path,id=/path</code>.
    </p>
  {:else}
    <form onsubmit={importWatched}>
      <p>
        <label for="root">Folder</label><br />
        <select id="root" bind:value={pathId}>
          {#each sources.watch_roots as root (root)}
            <option value={root}>{root}</option>
          {/each}
        </select>
      </p>
      <p>
        <label for="relative">Path within the folder</label><br />
        <input id="relative" bind:value={relativePath} placeholder="Example Disc/disc.iso" />
        <br />
        <!-- The server never accepts a host path, and saying so here means an
             operator does not try one. -->
        <small>Relative to the folder above. The file is copied; your original is left alone.</small
        >
      </p>
      <button type="submit" disabled={busy || relativePath.trim() === ''}>
        {busy ? 'Working…' : 'Import this file'}
      </button>
    </form>
  {/if}
</section>

<section aria-labelledby="queue-heading">
  <h2 id="queue-heading">Recent imports</h2>

  <div aria-live="polite" aria-busy={view.kind === 'loading'}>
    {#if view.kind === 'loading'}
      <p>Loading imports…</p>
    {:else if view.kind === 'empty'}
      <p>Nothing has been imported yet.</p>
    {:else if view.kind === 'error'}
      <p>{view.problem.title}.</p>
      <details>
        <summary>Technical detail</summary>
        <p>{view.problem.detail}</p>
        <p>Error code: <code>{view.problem.code}</code></p>
      </details>
    {:else}
      <table>
        <caption class="visually-hidden">Imports, newest first</caption>
        <thead>
          <tr>
            <th scope="col">File</th>
            <th scope="col">Source</th>
            <th scope="col">State</th>
            <th scope="col">Size</th>
            <th scope="col">Result</th>
            <th scope="col">Actions</th>
          </tr>
        </thead>
        <tbody>
          {#each view.data.items as job (job.id)}
            <tr>
              <th scope="row">{job.source_filename ?? job.id.slice(0, 8)}</th>
              <td>{job.source_kind.replace(/_/g, ' ')}</td>
              <td>
                <!-- A symbol carries the status alongside any styling, so it
                     is never communicated by colour alone. -->
                <span aria-hidden="true">{importStateMark(job.state)}</span>
                {importStateLabel(job.state)}
                {#if transferFraction(job) !== null}
                  <progress max="100" value={(transferFraction(job) ?? 0) * 100}></progress>
                {/if}
                {#if job.error_code}
                  <br /><small><code>{job.error_code}</code></small>
                {/if}
              </td>
              <td>{formatBytes(job.bytes_received || (job.bytes_expected ?? 0))}</td>
              <td>
                {#if job.artifact_id}
                  <a href={`/library/${job.artifact_id}`}>Open in library</a>
                {:else if job.error_detail}
                  {job.error_detail}
                {:else}
                  —
                {/if}
                {#if job.warnings.length > 0}
                  <details>
                    <summary
                      >{job.warnings.length} finding{job.warnings.length === 1 ? '' : 's'}</summary
                    >
                    <ul>
                      {#each job.warnings as warning, index (index)}
                        <li>{warning}</li>
                      {/each}
                    </ul>
                  </details>
                {/if}
              </td>
              <td>
                {#if canCancel(job)}
                  <button type="button" onclick={() => cancel(job.id)} disabled={busy}>
                    Cancel
                  </button>
                {/if}
                {#if canRetry(job)}
                  <button type="button" onclick={() => retry(job.id)} disabled={busy}>
                    {job.resumes_from ? `Retry from ${job.resumes_from}` : 'Retry'}
                  </button>
                {/if}
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}
  </div>
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
  label {
    font-weight: 600;
  }
  input:not([type]),
  select {
    padding: 0.4rem;
    min-width: min(28rem, 100%);
  }
  button {
    padding: 0.4rem 0.8rem;
    margin-right: 0.25rem;
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
    display: inline-block;
    max-width: 40rem;
  }
  progress {
    display: block;
    width: 8rem;
  }
</style>
