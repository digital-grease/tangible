<!-- SPDX-FileCopyrightText: 2026 digitalgrease
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { onMount } from 'svelte';
  import { loadHealth, type HealthState } from '$lib/health';

  let state = $state<HealthState>({ kind: 'loading' });

  onMount(async () => {
    state = await loadHealth();
  });
</script>

<svelte:head><title>Tangible — system health</title></svelte:head>

<main>
  <h1>Tangible</h1>

  <!-- Every asynchronous view represents loading, empty, error and success.
       Status is never communicated by colour alone, so each state carries a
       text label. -->
  <section aria-live="polite" aria-busy={state.kind === 'loading'}>
    {#if state.kind === 'loading'}
      <p>Checking system health…</p>
    {:else if state.kind === 'unreachable'}
      <h2>Server unreachable</h2>
      <p>The web UI could not reach the API. {state.reason}</p>
    {:else}
      <h2>
        {state.kind === 'ready' ? 'Ready' : 'Degraded'}
        {#if state.liveness}<small>version {state.liveness.version}</small>{/if}
      </h2>

      {#if state.readiness.checks.length === 0}
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
            {#each state.readiness.checks as check (check.name)}
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
</main>

<style>
  main {
    max-width: 48rem;
    margin: 2rem auto;
    padding: 0 1rem;
    font-family: system-ui, sans-serif;
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
  small {
    font-weight: normal;
    font-size: 0.8em;
  }
</style>
