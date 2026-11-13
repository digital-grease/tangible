<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // One request (ADR 0021): what was asked for, by whom, where it stands,
  // and what the person looking may do to it, as the server says in its
  // `can_*` fields. Declining asks why; the requester sees the reason.
  import { onDestroy, onMount } from 'svelte';
  import { names, watch } from '$lib/events';
  import { page } from '$app/state';
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import Confirm from '$lib/components/ui/Confirm.svelte';
  import Field from '$lib/components/ui/Field.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import Panel from '$lib/components/ui/Panel.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import {
    approveRequest,
    cancelRequest,
    declineRequest,
    detailLabel,
    kindLabel,
    loadRequest,
    refLabel,
    requestNext,
    requestStateLabel,
    requestStateTone,
    type RequestView,
  } from '$lib/requests';
  import type { Problem } from '$lib/session';

  type View =
    | { kind: 'loading' }
    | { kind: 'error'; problem: Problem }
    | { kind: 'ready'; data: RequestView };

  let view = $state<View>({ kind: 'loading' });
  let refusal = $state<{ problem: Problem; endpoint: string } | null>(null);
  let working = $state(false);
  let declining = $state(false);
  let withdrawing = $state(false);
  let reason = $state('');
  let reasonError = $state<string | null>(null);
  let stopWatching: (() => void) | undefined;

  /** How often to reconcile where the browser has no event stream. */
  const FALLBACK_MS = 15_000;
  const MAX_REASON = 2000;
  const id = $derived(page.params.id ?? '');
  const request = $derived(view.kind === 'ready' ? view.data : null);

  async function refresh() {
    const next = await loadRequest(id);
    if (next.kind === 'ok') view = { kind: 'ready', data: next.data };
    // A failed poll leaves what is on screen alone.
    else if (view.kind !== 'ready') view = { kind: 'error', problem: next.problem };
  }

  onMount(() => {
    void refresh();
    stopWatching = watch(['requests'], () => void refresh(), {
      fallbackMs: FALLBACK_MS,
      relevant: (event) => names(event, id),
    });
  });

  onDestroy(() => stopWatching?.());

  async function act(
    endpoint: string,
    call: () => ReturnType<typeof approveRequest>,
  ): Promise<boolean> {
    working = true;
    refusal = null;
    const result = await call();
    working = false;
    if (result.kind === 'error') {
      refusal = { problem: result.problem, endpoint };
      return false;
    }
    view = { kind: 'ready', data: result.data };
    return true;
  }

  function approve() {
    void act('POST /api/v1/requests/{id}/approve', () => approveRequest(id));
  }

  async function decline(event: SubmitEvent) {
    event.preventDefault();
    reasonError = reason.trim() ? null : 'Say why, for the requester.';
    if (reasonError) return;
    if (await act('POST /api/v1/requests/{id}/decline', () => declineRequest(id, reason.trim()))) {
      declining = false;
      reason = '';
    }
  }

  async function withdraw() {
    if (await act('POST /api/v1/requests/{id}/cancel', () => cancelRequest(id))) {
      withdrawing = false;
    }
  }

  function when(value: string | null | undefined): string {
    return value ? new Date(value).toLocaleString() : '—';
  }
</script>

<svelte:head><title>Request — Tangible</title></svelte:head>

{#if view.kind === 'loading'}
  <p class="quiet">Loading request…</p>
{:else if view.kind === 'error'}
  <p><a href="/requests">Requests</a></p>
  <ErrorPanel
    problem={view.problem}
    area="other"
    endpoint="GET /api/v1/requests/{'{id}'}"
    heading="Could not load this request"
    level={1}
  />
{:else if request}
  <PageHeader
    title={request.title}
    crumbs={[{ label: 'Requests', href: '/requests' }, { label: request.title }]}
  >
    {#snippet actions()}
      {#if request.can_approve}
        <Button variant="primary" icon="check" onclick={approve} disabled={working}>
          {working ? 'Approving…' : 'Approve'}
        </Button>
      {/if}
      {#if request.can_decline && !declining}
        <Button onclick={() => (declining = true)} disabled={working}>Decline…</Button>
      {/if}
      {#if request.can_cancel && !withdrawing}
        <Button variant="danger" onclick={() => (withdrawing = true)} disabled={working}>
          Withdraw…
        </Button>
      {/if}
    {/snippet}
  </PageHeader>

  <section aria-labelledby="status-heading" class="status" aria-live="polite">
    <h2 id="status-heading">
      <StatusMark tone={requestStateTone(request.state)} label={requestStateLabel(request.state)} />
    </h2>
    <p>{requestNext(request)}</p>
    {#if request.state === 'declined' && request.decline_reason}
      <p class="reason">Why: {request.decline_reason}</p>
    {/if}
    {#if request.state === 'fulfilled' && request.artifact_id}
      <p><a href={`/library/${request.artifact_id}`}>Open it in the library</a></p>
    {/if}
    {#if request.import_id && request.state === 'downloading'}
      <p><a href="/imports">Follow the download on the Imports page</a></p>
    {/if}
    {#if request.error_code}
      <details>
        <summary>Why the download failed</summary>
        <p>{request.error_detail}</p>
        <p>Error code: <code>{request.error_code}</code></p>
      </details>
    {/if}
  </section>

  {#if declining}
    <div class="question">
      <Confirm title="Decline this request?" onsubmit={decline}>
        <p>
          Nothing is searched for or downloaded for a declined request, and it cannot be approved
          later. The requester sees the reason.
        </p>
        <Field label="Why" required error={reasonError}>
          {#snippet children({ id: control, describedBy, invalid })}
            <textarea
              id={control}
              bind:value={reason}
              rows="3"
              maxlength={MAX_REASON}
              aria-describedby={describedBy}
              aria-invalid={invalid || undefined}></textarea>
          {/snippet}
        </Field>
        {#snippet actions()}
          <Button type="submit" variant="danger" disabled={working}>
            {working ? 'Declining…' : 'Decline this request'}
          </Button>
          <Button onclick={() => (declining = false)} disabled={working}>Keep it</Button>
        {/snippet}
      </Confirm>
    </div>
  {/if}

  {#if withdrawing}
    <div class="question">
      <Confirm title="Withdraw this request?">
        <p>
          Nothing more is searched for or downloaded for it, and it cannot be taken up again. Ask
          again to want it later.
        </p>
        {#snippet actions()}
          <Button variant="danger" onclick={withdraw} disabled={working}>
            {working ? 'Withdrawing…' : 'Withdraw this request'}
          </Button>
          <Button onclick={() => (withdrawing = false)} disabled={working}>Keep it</Button>
        {/snippet}
      </Confirm>
    </div>
  {/if}

  {#if refusal}
    <ErrorPanel problem={refusal.problem} area="other" endpoint={refusal.endpoint} refusal />
  {/if}

  <Panel title="Details">
    <dl class="facts">
      <div>
        <dt>Kind</dt>
        <dd>{kindLabel(request.kind)}</dd>
      </div>
      {#each Object.entries(request.details) as [name, value] (name)}
        <div>
          <dt>{detailLabel(name)}</dt>
          <dd>{value}</dd>
        </div>
      {/each}
      <div>
        <dt>Asked for</dt>
        <dd>{when(request.created_at)} by {request.requested_by}</dd>
      </div>
      {#if request.decided_by}
        <div>
          <dt>{request.state === 'declined' ? 'Declined' : 'Approved'}</dt>
          <dd>{when(request.decided_at)} by {request.decided_by}</dd>
        </div>
      {/if}
      {#each request.refs as ref (`${ref.provider}:${ref.external_id}`)}
        {@const shown = refLabel(ref)}
        <div>
          <dt>Found in</dt>
          <dd>
            {#if shown.href}
              <a href={shown.href} rel="noreferrer noopener" target="_blank">{shown.label}</a>
            {:else}
              {shown.label}
            {/if}
          </dd>
        </div>
      {/each}
    </dl>
    {#if request.note}
      <h3 class="note-heading">Note</h3>
      <p class="note">{request.note}</p>
    {/if}
  </Panel>
{/if}

<style>
  .status {
    padding: var(--space-3) var(--space-4);
    margin-bottom: var(--space-4);
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--radius-lg);
  }
  .status h2 {
    margin: 0 0 var(--space-2);
    font-size: var(--text-md);
  }
  .status p {
    margin: var(--space-2) 0 0;
  }
  .reason {
    overflow-wrap: anywhere;
    white-space: pre-line;
  }
  .question {
    margin-bottom: var(--space-4);
    max-width: 75ch;
  }
  .facts {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(13rem, 1fr));
    gap: var(--space-3) var(--space-5);
    margin: 0;
  }
  dt {
    color: var(--text-muted);
    font-size: var(--text-sm);
  }
  dd {
    margin: 0;
    font-weight: var(--weight-medium);
    overflow-wrap: anywhere;
  }
  .note-heading {
    margin: var(--space-4) 0 var(--space-1);
    font-size: var(--text-sm);
    color: var(--text-muted);
    font-weight: var(--weight-regular);
  }
  .note {
    margin: 0;
    white-space: pre-line;
    overflow-wrap: anywhere;
  }
  .quiet {
    color: var(--text-muted);
  }
</style>
