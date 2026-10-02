<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { apiFetch } from '$lib/session';
  import { onMount } from 'svelte';
  import { conditionLabel, conditionMark, loadDiscs, type PhysicalCopyPage } from '$lib/discs';
  import type { LoadState } from '$lib/library';

  let view = $state<LoadState<PhysicalCopyPage>>({ kind: 'loading' });
  let status = $state('');

  async function refresh() {
    view = await loadDiscs(apiFetch, { status: status || undefined });
  }

  onMount(refresh);

  const attention = $derived(
    view.kind === 'ready' ? view.data.items.filter((copy) => copy.should_be_destroyed) : [],
  );
</script>

<svelte:head><title>Discs — Tangible</title></svelte:head>

<h1>Discs</h1>

<p>
  Every burn that consumed media produced a disc, including the burns that failed. A disc nobody
  tracked is the one that gets shelved, found later, and trusted.
</p>

<div aria-live="polite" aria-busy={view.kind === 'loading'}>
  {#if attention.length > 0}
    <!-- The line on this page that needs somebody to go and do something. -->
    <section class="callout" aria-labelledby="attention-heading">
      <h2 id="attention-heading">
        {attention.length}
        {attention.length === 1 ? 'disc should be destroyed' : 'discs should be destroyed'}
      </h2>
      <p>
        These do not hold what was intended, or are no longer trusted to. Destroy them rather than
        returning them to a shelf.
      </p>
      <ul>
        {#each attention as copy (copy.id)}
          <li>
            <a href={`/discs/${copy.id}`}>{copy.label ?? copy.id.slice(0, 8)}</a>
            — {conditionLabel(copy.status)}
          </li>
        {/each}
      </ul>
    </section>
  {/if}

  <p>
    <label for="filter">Condition</label>
    <select id="filter" bind:value={status} onchange={refresh}>
      <option value="">All</option>
      <option value="verified">Verified</option>
      <option value="produced_unverified">Written, not verified</option>
      <option value="verification_failed">Failed verification</option>
      <option value="degraded">Degraded</option>
      <option value="lost">Lost</option>
      <option value="destroyed">Destroyed</option>
    </select>
  </p>

  {#if view.kind === 'loading'}
    <p>Loading discs…</p>
  {:else if view.kind === 'empty'}
    <section>
      <h2>No discs yet</h2>
      <p>A disc appears here the moment a burn starts consuming one.</p>
      <p><a href="/burns">Burn queue</a></p>
    </section>
  {:else if view.kind === 'error'}
    <section>
      <h2>Could not load the disc inventory</h2>
      <p>{view.problem.title}.</p>
      <details>
        <summary>Technical detail</summary>
        <p>{view.problem.detail}</p>
        <p>Error code: <code>{view.problem.code}</code></p>
      </details>
    </section>
  {:else}
    <table>
      <caption class="visually-hidden">Discs, newest first</caption>
      <thead>
        <tr>
          <th scope="col">Disc</th>
          <th scope="col">Condition</th>
          <th scope="col">Media</th>
          <th scope="col">Where</th>
          <th scope="col">Last checked</th>
        </tr>
      </thead>
      <tbody>
        {#each view.data.items as copy (copy.id)}
          <tr>
            <th scope="row">
              <a href={`/discs/${copy.id}`}>{copy.label ?? copy.id.slice(0, 8)}</a>
            </th>
            <td>
              <!-- A symbol carries the status alongside any styling, so it is
                   never communicated by colour alone. -->
              <span aria-hidden="true">{conditionMark(copy.status)}</span>
              {conditionLabel(copy.status)}
            </td>
            <td>{copy.media_profile}</td>
            <td>{copy.storage_location ?? '—'}</td>
            <td>
              {#if copy.last_checked_at}
                {new Date(copy.last_checked_at).toLocaleDateString()}
                <small>({copy.check_count})</small>
              {:else}
                Never
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
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
  label {
    font-weight: 600;
  }
  select {
    padding: 0.3rem;
  }
  .visually-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
  }
</style>
