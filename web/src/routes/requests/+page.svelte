<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Requests for items (ADR 0021): a viewer's own, or everybody's for an
  // operator or administrator, newest first, narrowed by state. Asking for
  // an item is on its own page.
  import { onDestroy, onMount } from 'svelte';
  import { watch } from '$lib/events';
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import Panel from '$lib/components/ui/Panel.svelte';
  import Select from '$lib/components/ui/Select.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import Table from '$lib/components/ui/Table.svelte';
  import {
    STATE_FILTERS,
    kindLabel,
    loadRequests,
    requestStateLabel,
    requestStateTone,
    type RequestView,
  } from '$lib/requests';
  import type { Problem } from '$lib/session';

  type View =
    | { kind: 'loading' }
    | { kind: 'error'; problem: Problem }
    | { kind: 'ready'; items: RequestView[]; next: string | null };

  let view = $state<View>({ kind: 'loading' });
  let filter = $state('');
  let more = $state<Problem | null>(null);
  let loadingMore = $state(false);
  let stopWatching: (() => void) | undefined;

  /** How often to reconcile where the browser has no event stream. */
  const FALLBACK_MS = 15_000;

  async function refresh() {
    const page = await loadRequests({ state: filter });
    if (page.kind === 'ok') {
      view = { kind: 'ready', items: page.data.items, next: page.data.next_cursor ?? null };
    } else if (view.kind !== 'ready') {
      view = { kind: 'error', problem: page.problem };
    }
  }

  async function showMore() {
    if (view.kind !== 'ready' || !view.next) return;
    loadingMore = true;
    more = null;
    const page = await loadRequests({ state: filter, cursor: view.next });
    loadingMore = false;
    if (page.kind === 'error') {
      more = page.problem;
      return;
    }
    view = {
      kind: 'ready',
      items: [...view.items, ...page.data.items],
      next: page.data.next_cursor ?? null,
    };
  }

  function narrow() {
    view = { kind: 'loading' };
    void refresh();
  }

  onMount(() => {
    void refresh();
    stopWatching = watch(['requests'], () => void refresh(), { fallbackMs: FALLBACK_MS });
  });

  onDestroy(() => stopWatching?.());

  function when(value: string | null | undefined): string {
    return value ? new Date(value).toLocaleString() : '—';
  }

  function detailLine(request: RequestView): string {
    return [kindLabel(request.kind), ...Object.values(request.details)].join(' · ');
  }
</script>

<svelte:head><title>Requests — Tangible</title></svelte:head>

<PageHeader
  title="Requests"
  description="Items people have asked for, each attested by its requester as owned. An operator or administrator approves a request before anything is searched for or downloaded."
>
  {#snippet actions()}
    <Button variant="primary" icon="plus" href="/requests/new">Ask for an item</Button>
  {/snippet}
</PageHeader>

<div class="filters">
  <Select label="Show" options={STATE_FILTERS} bind:value={filter} onchange={narrow} />
</div>

<div aria-live="polite" aria-busy={view.kind === 'loading'}>
  <Panel title="Requests, newest first" flush>
    {#if view.kind === 'loading'}
      <p class="quiet padded">Loading requests…</p>
    {:else if view.kind === 'error'}
      <div class="padded">
        <ErrorPanel problem={view.problem} area="other" endpoint="GET /api/v1/requests" />
      </div>
    {:else if view.items.length === 0}
      <p class="quiet padded">
        {filter
          ? `No request is ${requestStateLabel(filter).toLowerCase()}.`
          : 'Nothing has been asked for yet.'}
      </p>
    {:else}
      <Table caption="Requests, newest first">
        <thead>
          <tr>
            <th scope="col">Item</th>
            <th scope="col">Asked by</th>
            <th scope="col">State</th>
            <th scope="col">Asked</th>
          </tr>
        </thead>
        <tbody>
          {#each view.items as request (request.id)}
            <tr>
              <th scope="row">
                <a href={`/requests/${request.id}`}>{request.title}</a>
                <span class="sub">{detailLine(request)}</span>
              </th>
              <td>{request.requested_by}</td>
              <td>
                <StatusMark
                  tone={requestStateTone(request.state)}
                  label={requestStateLabel(request.state)}
                  pill
                />
              </td>
              <td>{when(request.created_at)}</td>
            </tr>
          {/each}
        </tbody>
      </Table>
      {#if view.next}
        <div class="padded">
          <Button onclick={showMore} disabled={loadingMore}>
            {loadingMore ? 'Loading…' : 'Show older requests'}
          </Button>
        </div>
      {/if}
      {#if more}
        <div class="padded">
          <ErrorPanel problem={more} area="other" endpoint="GET /api/v1/requests" />
        </div>
      {/if}
    {/if}
  </Panel>
</div>

<style>
  .filters {
    max-width: 18rem;
    margin-bottom: var(--space-4);
  }
  .sub {
    display: block;
    font-weight: var(--weight-regular);
    font-size: var(--text-sm);
    color: var(--text-muted);
  }
  .padded {
    padding: var(--space-3) var(--space-4);
  }
  .quiet {
    color: var(--text-muted);
    margin: 0;
  }
</style>
