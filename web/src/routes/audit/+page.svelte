<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // The audit log, for administrators (UX.md 11; docs/plans/ui-overhaul.md,
  // U8): who did what, newest first, filtered by the kind of action, who
  // did it and what it was done to. Read only.
  import { onMount } from 'svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import EmptyState from '$lib/components/ui/EmptyState.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import Panel from '$lib/components/ui/Panel.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import Table from '$lib/components/ui/Table.svelte';
  import { when } from '$lib/overview';
  import {
    ACTION_KINDS,
    actionLabel,
    actorLabel,
    loadAuditEvents,
    metadataText,
    outcomeLabel,
    outcomeTone,
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

  /** The full date and time, for a time's tooltip. */
  function full(value: string): string {
    return new Date(value).toLocaleString();
  }

  function search(event: SubmitEvent) {
    event.preventDefault();
    void refresh();
  }
</script>

<svelte:head><title>Audit log — Tangible</title></svelte:head>

<PageHeader
  title="Audit log"
  description="Who did what, newest first. Nothing here changes; the log is only read."
/>

<form class="filters" onsubmit={search} aria-label="Filter the audit log">
  <div class="filter">
    <label for="kind">What</label>
    <select id="kind" bind:value={action}>
      {#each ACTION_KINDS as kind (kind.value)}
        <option value={kind.value}>{kind.label}</option>
      {/each}
    </select>
  </div>
  <div class="filter wide">
    <label for="actor">Who</label>
    <input
      id="actor"
      bind:value={actorId}
      maxlength="200"
      autocomplete="off"
      spellcheck="false"
      aria-describedby="actor-hint"
    />
    <span class="hint" id="actor-hint">A username, or a worker's or integration's identifier</span>
  </div>
  <div class="filter wide">
    <label for="target">To what</label>
    <input
      id="target"
      bind:value={targetId}
      maxlength="200"
      autocomplete="off"
      spellcheck="false"
      aria-describedby="target-hint"
    />
    <span class="hint" id="target-hint">An identifier</span>
  </div>
  <div class="submit">
    <Button type="submit" variant="primary">Show</Button>
  </div>
</form>

<div aria-busy={view.kind === 'loading'}>
  {#if view.kind === 'loading'}
    <p class="quiet">Loading the audit log…</p>
  {:else if view.kind === 'forbidden'}
    <EmptyState title="Administrators only" icon="lock">
      <p>Only an administrator can read the audit log.</p>
    </EmptyState>
  {:else if view.kind === 'error'}
    <div class="problem" role="alert">
      <StatusMark tone="bad" label={view.problem.detail} />
    </div>
  {:else}
    <Panel title="Events" aside={`${view.events.length}${view.nextCursor ? '+' : ''}`} flush>
      {#if view.events.length === 0}
        <p class="padded quiet">Nothing recorded matches.</p>
      {:else}
        <Table caption="Audit events, newest first">
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
                <td>
                  <time datetime={event.occurred_at} title={full(event.occurred_at)}
                    >{when(event.occurred_at)}</time
                  >
                </td>
                <td>{actorLabel(event)}</td>
                <th scope="row">{actionLabel(event.action)}</th>
                <td>
                  <div class="cell">
                    <span>{event.target_type}</span>
                    {#if event.target_id}<code class="small">{event.target_id}</code>{/if}
                  </div>
                </td>
                <td>
                  <StatusMark
                    tone={outcomeTone(event.outcome)}
                    label={outcomeLabel(event.outcome)}
                  />
                </td>
                <td><code class="small metadata">{metadataText(event.metadata)}</code></td>
              </tr>
            {/each}
          </tbody>
        </Table>
      {/if}
    </Panel>
    {#if view.nextCursor}
      <div class="more">
        <Button disabled={loadingMore} onclick={loadMore}>
          {loadingMore ? 'Loading…' : 'Show older events'}
        </Button>
      </div>
    {/if}
    <div role="alert">
      {#if moreProblem}
        <p class="problem-text">Older events could not be loaded: {moreProblem.detail}</p>
      {/if}
    </div>
  {/if}
</div>

<style>
  .filters {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-3) var(--space-4);
    align-items: start;
    margin-bottom: var(--space-5);
    padding: var(--space-3) var(--space-4);
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--radius-lg);
  }
  .filter {
    display: grid;
    gap: var(--space-1);
  }
  .filter label {
    font-size: var(--text-sm);
    color: var(--text-muted);
  }
  .filter.wide {
    flex: 1 1 14rem;
  }
  .filter input {
    width: 100%;
  }
  .hint {
    font-size: var(--text-xs);
    color: var(--text-muted);
  }
  /* In line with the fields, not their hints. */
  .submit {
    padding-top: calc(var(--text-sm) * 1.5 + var(--space-1));
  }
  .quiet {
    color: var(--text-muted);
  }
  .padded {
    margin: 0;
    padding: var(--space-3) var(--space-4);
  }
  .cell {
    display: grid;
    gap: 0.125rem;
  }
  .small {
    font-size: var(--text-xs);
  }
  .metadata {
    color: var(--text-muted);
    overflow-wrap: anywhere;
  }
  .more {
    margin-top: var(--space-4);
  }
  .problem {
    padding: var(--space-3) var(--space-4);
    background: var(--surface);
    border: 1px solid var(--line);
    border-left: 4px solid var(--bad);
    border-radius: var(--radius);
  }
  .problem-text {
    margin: var(--space-3) 0 0;
    color: var(--bad);
  }
</style>
