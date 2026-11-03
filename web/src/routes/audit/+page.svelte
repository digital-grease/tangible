<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { onMount } from 'svelte';
  import {
    ACTION_KINDS,
    actionLabel,
    actorLabel,
    loadAuditEvents,
    metadataText,
    outcomeLabel,
    type AuditEvent,
    type AuditFilter,
  } from '$lib/audit';
  import type { Problem } from '$lib/session';

  type View =
    | { kind: 'loading' }
    | { kind: 'ready'; events: AuditEvent[]; nextCursor: string | null }
    | { kind: 'forbidden' }
    | { kind: 'error'; problem: Problem };

  let view = $state<View>({ kind: 'loading' });
  let action = $state('');
  let targetId = $state('');
  let actorId = $state('');
  let loadingMore = $state(false);
  let moreProblem = $state<Problem | null>(null);

  function filter(): AuditFilter {
    return { action, targetId, actorId };
  }

  async function refresh() {
    view = { kind: 'loading' };
    moreProblem = null;
    const result = await loadAuditEvents(filter());
    if (result.kind === 'ok') {
      view = {
        kind: 'ready',
        events: result.data.items,
        nextCursor: result.data.next_cursor ?? null,
      };
    } else if (result.problem.code === 'FORBIDDEN') {
      view = { kind: 'forbidden' };
    } else {
      view = { kind: 'error', problem: result.problem };
    }
  }

  onMount(refresh);

  async function loadMore() {
    if (view.kind !== 'ready' || !view.nextCursor || loadingMore) return;
    loadingMore = true;
    moreProblem = null;
    const next = await loadAuditEvents(filter(), view.nextCursor);
    loadingMore = false;
    if (next.kind === 'error') {
      // The events already shown stay; only the older ones are missing.
      moreProblem = next.problem;
      return;
    }
    if (view.kind === 'ready') {
      view = {
        kind: 'ready',
        events: [...view.events, ...next.data.items],
        nextCursor: next.data.next_cursor ?? null,
      };
    }
  }

  function search(event: SubmitEvent) {
    event.preventDefault();
    void refresh();
  }
</script>

<svelte:head><title>Audit log — Tangible</title></svelte:head>

<h1>Audit log</h1>
<p>Who did what, newest first. Nothing here changes; the log is only read.</p>

<form onsubmit={search} aria-label="Filter the audit log">
  <p>
    <label for="kind">What</label><br />
    <select id="kind" bind:value={action}>
      {#each ACTION_KINDS as kind (kind.value)}
        <option value={kind.value}>{kind.label}</option>
      {/each}
    </select>
  </p>
  <p>
    <label for="actor">Who (a username, or a worker's or integration's identifier)</label><br />
    <input id="actor" bind:value={actorId} maxlength="200" autocomplete="off" />
  </p>
  <p>
    <label for="target">What it was done to (an identifier)</label><br />
    <input id="target" bind:value={targetId} maxlength="200" autocomplete="off" />
  </p>
  <button type="submit">Show</button>
</form>

<section aria-labelledby="events-heading" aria-busy={view.kind === 'loading'}>
  <h2 id="events-heading">Events</h2>
  {#if view.kind === 'loading'}
    <p>Loading the audit log…</p>
  {:else if view.kind === 'forbidden'}
    <p>Only an administrator can read the audit log.</p>
  {:else if view.kind === 'error'}
    <div role="alert"><p>{view.problem.detail}</p></div>
  {:else if view.events.length === 0}
    <p>Nothing recorded matches.</p>
  {:else}
    <table>
      <caption>Newest first</caption>
      <thead>
        <tr>
          <th scope="col">When</th>
          <th scope="col">Who</th>
          <th scope="col">What</th>
          <th scope="col">To what</th>
          <th scope="col">Outcome</th>
          <th scope="col">Details</th>
        </tr>
      </thead>
      <tbody>
        {#each view.events as event (event.id)}
          <tr>
            <td>{new Date(event.occurred_at).toLocaleString()}</td>
            <td>{actorLabel(event)}</td>
            <th scope="row">{actionLabel(event.action)}</th>
            <td
              >{event.target_type}{#if event.target_id}<br /><small>{event.target_id}</small
                >{/if}</td
            >
            <td>{outcomeLabel(event.outcome)}</td>
            <td><small><code>{metadataText(event.metadata)}</code></small></td>
          </tr>
        {/each}
      </tbody>
    </table>
    {#if view.nextCursor}
      <p>
        <button type="button" disabled={loadingMore} onclick={loadMore}>
          {loadingMore ? 'Loading…' : 'Show older events'}
        </button>
      </p>
    {/if}
    <div role="alert">
      {#if moreProblem}
        <p>Older events could not be loaded: {moreProblem.detail}</p>
      {/if}
    </div>
  {/if}
</section>
