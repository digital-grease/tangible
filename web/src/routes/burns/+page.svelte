<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { apiFetch } from '$lib/session';
  import { onDestroy, onMount } from 'svelte';
  import {
    isLive,
    jobStateLabel,
    jobStateMark,
    loadBurnQueue,
    progressPercent,
    stageLabel,
    type BurnJobPage,
  } from '$lib/burns';
  import type { LoadState } from '$lib/library';

  let view = $state<LoadState<BurnJobPage>>({ kind: 'loading' });
  let loadingMore = $state(false);
  let timer: ReturnType<typeof setInterval> | undefined;

  /**
   * How often the queue reconciles itself.
   *
   * A poll, not a stream. The event stream is the intended mechanism and this
   * becomes its low-frequency fallback once it exists; until then a burn takes
   * tens of minutes, so ten seconds is attentive enough and costs almost
   * nothing.
   */
  const RECONCILE_MS = 10_000;

  async function refresh() {
    const next = await loadBurnQueue();
    // A failed poll leaves what is on screen alone. Replacing a readable queue
    // with an error because one refresh missed would be worse than stale.
    if (next.kind === 'error' && view.kind === 'ready') return;
    view = next;
  }

  onMount(async () => {
    await refresh();
    timer = setInterval(refresh, RECONCILE_MS);
  });

  onDestroy(() => {
    if (timer) clearInterval(timer);
  });

  async function loadMore() {
    if (view.kind !== 'ready' || !view.data.next_cursor || loadingMore) return;
    loadingMore = true;
    const next = await loadBurnQueue(apiFetch, view.data.next_cursor);
    if (next.kind === 'ready' && view.kind === 'ready') {
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

  const active = $derived(
    view.kind === 'ready' ? view.data.items.filter((job) => isLive(job.state)) : [],
  );
  const waiting = $derived(active.filter((job) => job.state === 'waiting_for_media'));
</script>

<svelte:head><title>Burn queue — Tangible</title></svelte:head>

<h1>Burn queue</h1>

<!-- One live region for the whole view, so a change is announced once rather
     than per row. -->
<div aria-live="polite" aria-busy={view.kind === 'loading'}>
  {#if view.kind === 'loading'}
    <p>Loading the burn queue…</p>
  {:else if view.kind === 'empty'}
    <section>
      <h2>No burn jobs</h2>
      <p>Select a compatible artifact and choose <strong>Create burn job</strong>.</p>
      <p><a href="/library">Browse the library</a></p>
    </section>
  {:else if view.kind === 'error'}
    <section>
      <h2>Could not load the burn queue</h2>
      <p>{view.problem.title}.</p>
      <details>
        <summary>Technical detail</summary>
        <p>{view.problem.detail}</p>
        <p>Error code: <code>{view.problem.code}</code></p>
      </details>
    </section>
  {:else}
    {#if waiting.length > 0}
      <!-- The one thing on this page that needs a person to get up and do
           something. It goes first, and says which job. -->
      <section class="callout" aria-labelledby="waiting-heading">
        <h2 id="waiting-heading">
          {waiting.length === 1 ? 'A burn is waiting for a disc' : 'Burns are waiting for a disc'}
        </h2>
        <ul>
          {#each waiting as job (job.id)}
            <li>
              <a href={`/burns/${job.id}`}>Insert a disc for burn {job.id.slice(0, 8)}</a>
              {#if job.requested_media_profile}
                — {job.requested_media_profile}
              {/if}
            </li>
          {/each}
        </ul>
      </section>
    {/if}

    <p>
      {view.data.items.length} burn job{view.data.items.length === 1 ? '' : 's'}, {active.length}
      running or queued.
    </p>

    <table>
      <caption class="visually-hidden">Burn jobs, newest first</caption>
      <thead>
        <tr>
          <th scope="col">Burn</th>
          <th scope="col">State</th>
          <th scope="col">Stage</th>
          <th scope="col">Attempts</th>
          <th scope="col">Queued</th>
        </tr>
      </thead>
      <tbody>
        {#each view.data.items as job (job.id)}
          <tr>
            <th scope="row"><a href={`/burns/${job.id}`}><code>{job.id.slice(0, 8)}</code></a></th>
            <td>
              <!-- A symbol carries the status alongside any styling, so it is
                   never communicated by colour alone. -->
              <span aria-hidden="true">{jobStateMark(job.state)}</span>
              {jobStateLabel(job.state)}
            </td>
            <td>
              {#if job.progress}
                {stageLabel(job.progress.stage)}
                {#if progressPercent(job.progress.fraction) !== null}
                  — {progressPercent(job.progress.fraction)}%
                {/if}
              {:else}
                —
              {/if}
            </td>
            <td>
              {job.attempt_count}
              {#if job.attempt_count > 1}
                <small>({job.attempt_count} discs)</small>
              {/if}
            </td>
            <td>{new Date(job.created_at).toLocaleString()}</td>
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
  .callout {
    border: 2px solid currentColor;
    padding: 0.75rem 1rem;
    margin-bottom: 1.5rem;
  }
  .callout ul {
    margin: 0.5rem 0 0;
    padding-left: 1.25rem;
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
  small {
    font-size: 0.85em;
  }
</style>
