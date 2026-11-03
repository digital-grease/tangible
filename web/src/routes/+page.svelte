<!-- SPDX-FileCopyrightText: 2026 digitalgrease
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { onMount } from 'svelte';
  import { attemptOutcomeLabel, jobStateLabel } from '$lib/burns';
  import {
    burnsSummary,
    importsSummary,
    integritySummary,
    loadDashboard,
    needsSomebody,
    storageSummary,
    workersSummary,
    type Dashboard,
  } from '$lib/dashboard';
  import { loadHealth, type HealthState } from '$lib/health';
  import { sessionRole } from '$lib/session';

  let health = $state<HealthState>({ kind: 'loading' });
  let board = $state<Dashboard | null>(null);
  let boardProblem = $state<string | null>(null);

  onMount(async () => {
    const [answer, dashboard] = await Promise.all([loadHealth(), loadDashboard()]);
    health = answer;
    // Signed out, the page is the health check alone; anything else is
    // said rather than hidden.
    if (dashboard.kind === 'ok') board = dashboard.data;
    else if (dashboard.problem.code !== 'UNAUTHENTICATED') boardProblem = dashboard.problem.detail;
  });

  function when(value: string): string {
    return new Date(value).toLocaleString();
  }
</script>

<svelte:head><title>Tangible — system health</title></svelte:head>

<h1>Tangible</h1>

{#if board}
  <section aria-labelledby="board-heading" class="board">
    <h2 id="board-heading">
      {needsSomebody(board) ? 'Needs somebody' : 'Nothing needs you'}
    </h2>
    <ul class="cards">
      <li>
        <h3><a href="/burns">Burns</a></h3>
        <p>{burnsSummary(board.burns)}</p>
      </li>
      <li>
        <h3><a href="/imports">Imports</a></h3>
        <p>{importsSummary(board.imports)}</p>
      </li>
      <li>
        <h3><a href="/workers">Workers</a></h3>
        <p>{workersSummary(board.workers)}</p>
      </li>
      <li>
        <h3><a href="/storage">Integrity</a></h3>
        <p>{integritySummary(board.integrity)}</p>
      </li>
      <li>
        <h3><a href="/storage">Storage</a></h3>
        <p>{storageSummary(board.storage)}</p>
      </li>
    </ul>

    {#if board.attention.length > 0}
      <h3>Needing attention</h3>
      <ul>
        {#each board.attention as burn (burn.burn_job_id)}
          <li>
            <a href={`/burns/${burn.burn_job_id}`}>{burn.title}, disc {burn.disc_number}</a>:
            {jobStateLabel(burn.state)}{burn.error_code ? ` (${burn.error_code})` : ''}, {when(
              burn.at,
            )}
          </li>
        {/each}
      </ul>
    {/if}

    {#if board.recent_burns.length > 0}
      <h3>Latest burns</h3>
      <ul>
        {#each board.recent_burns as burn (burn.burn_job_id + burn.at)}
          <li>
            <a href={`/burns/${burn.burn_job_id}`}>{burn.title}, disc {burn.disc_number}</a>:
            {attemptOutcomeLabel(burn.state)}, {when(burn.at)}
          </li>
        {/each}
      </ul>
    {/if}
  </section>
{:else if boardProblem}
  <div role="alert"><p>The dashboard could not be loaded: {boardProblem}</p></div>
{/if}

<!-- Every asynchronous view represents loading, empty, error and success.
       Status is never communicated by colour alone, so each state carries a
       text label. -->
<section aria-live="polite" aria-busy={health.kind === 'loading'}>
  {#if health.kind === 'loading'}
    <p>Checking system health…</p>
  {:else if health.kind === 'unreachable'}
    <h2>Server unreachable</h2>
    <p>The web UI could not reach the API. {health.reason}</p>
  {:else}
    <h2>
      {health.kind === 'ready' ? 'Ready' : 'Degraded'}
      {#if health.liveness}<small>version {health.liveness.version}</small>{/if}
    </h2>

    {#if health.readiness.checks.length === 0}
      <p>No dependencies reported.</p>
    {:else}
      <table>
        <caption>Dependency status</caption>
        <thead>
          <tr
            ><th scope="col">Dependency</th><th scope="col">Status</th><th scope="col">Detail</th
            ></tr
          >
        </thead>
        <tbody>
          {#each health.readiness.checks as check (check.name)}
            <tr>
              <th scope="row">{check.name}</th>
              <td>{check.status === 'up' ? 'Up' : 'Down'}</td>
              <td>{check.detail ?? '—'}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}
  {/if}
</section>

{#if sessionRole() === 'administrator'}
  <p>
    This is the quick view. <a href="/diagnostics">Diagnostics</a> checks storage, tools, workers, drives
    and recent failures as well.
  </p>
{/if}

<p><a href="/library">Browse the library</a></p>

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
  small {
    font-weight: normal;
    font-size: 0.8em;
  }
  .cards {
    list-style: none;
    padding: 0;
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(14rem, 1fr));
    gap: 1rem;
  }
  .cards li {
    border: 1px solid currentColor;
    padding: 0 1rem;
  }
</style>
