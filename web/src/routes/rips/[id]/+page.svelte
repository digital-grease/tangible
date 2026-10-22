<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // One rip: what the drive held, how far reading has got, what the worker
  // said, and where the result went. Withdrawing is offered while the disc
  // is being read or sent; reading changes nothing on it.
  import { onDestroy, onMount } from 'svelte';
  import { page } from '$app/state';
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import { formatBytes } from '$lib/library';
  import {
    cancelRip,
    decryptionSummary,
    encryptionLabel,
    keepLabel,
    loadRip,
    loadRipEvents,
    ripProgress,
    ripRefusal,
    ripStateLabel,
    type RipEventView,
    type RipView,
  } from '$lib/rips';
  import type { Problem } from '$lib/session';

  type View =
    { kind: 'loading' } | { kind: 'error'; problem: Problem } | { kind: 'ready'; data: RipView };

  let view = $state<View>({ kind: 'loading' });
  let events = $state<RipEventView[]>([]);
  let refusal = $state<Problem | null>(null);
  let working = $state(false);
  let timer: ReturnType<typeof setInterval> | undefined;

  const RECONCILE_MS = 2_000;
  const id = $derived(page.params.id ?? '');
  const rip = $derived(view.kind === 'ready' ? view.data : null);
  const progress = $derived(ripProgress(events));

  async function refresh() {
    const [next, timeline] = await Promise.all([loadRip(id), loadRipEvents(id)]);
    if (next.kind === 'ok') view = { kind: 'ready', data: next.data };
    // A failed poll leaves what is on screen alone.
    else if (view.kind !== 'ready') view = { kind: 'error', problem: next.problem };
    if (timeline.kind === 'ok') events = timeline.data.items;
  }

  onMount(() => {
    void refresh();
    timer = setInterval(() => {
      if (rip?.is_open !== false) void refresh();
    }, RECONCILE_MS);
  });

  onDestroy(() => clearInterval(timer));

  async function withdraw() {
    working = true;
    refusal = null;
    const result = await cancelRip(id);
    working = false;
    if (result.kind === 'error') {
      refusal = result.problem;
      return;
    }
    view = { kind: 'ready', data: result.data };
  }

  function when(value: string | null | undefined): string {
    return value ? new Date(value).toLocaleString() : '—';
  }
</script>

<svelte:head><title>Rip — Tangible</title></svelte:head>

<p><a href="/rips">← Rips</a></p>

{#if view.kind === 'loading'}
  <p>Loading rip…</p>
{:else if view.kind === 'error'}
  <ErrorPanel
    problem={view.problem}
    area="import"
    endpoint="GET /api/v1/rips/{'{id}'}"
    heading="Could not load this rip"
    level={1}
  />
{:else if rip}
  <h1>Rip of the disc in {rip.drive_name}</h1>

  <section aria-labelledby="status-heading" class="status" aria-live="polite">
    <h2 id="status-heading">{ripStateLabel(rip.state)}</h2>
    {#if rip.state === 'reading' && progress !== null}
      <p>
        <progress max="100" value={progress} aria-label="Reading the disc"></progress>
        {progress}% read
      </p>
    {/if}
    {#if rip.bytes_received > 0}
      <p>{formatBytes(rip.bytes_received)} received.</p>
    {/if}
    {#if rip.state === 'complete' && rip.artifact_id}
      <p><a href={`/library/${rip.artifact_id}`}>Open it in the library</a></p>
      {#if rip.copy_artifact_id}
        <p><a href={`/library/${rip.copy_artifact_id}`}>Open the decrypted copy</a></p>
      {/if}
      {#if rip.keep === 'decrypted_original' && rip.decryption}
        <p>
          Only a decrypted image was kept. It was decrypted when ripped and is not a faithful copy
          of the disc.
        </p>
      {/if}
    {/if}
    {#if rip.error_code}
      <p>{ripRefusal(rip.error_code) ?? rip.error_detail}</p>
      <details>
        <summary>Technical detail</summary>
        <p>{rip.error_detail}</p>
        <p>Error code: <code>{rip.error_code}</code></p>
      </details>
    {/if}
  </section>

  {#if refusal}
    <ErrorPanel
      problem={refusal}
      area="import"
      endpoint="POST /api/v1/rips/{'{id}'}/cancel"
      refusal
    />
  {/if}

  {#if rip.can_cancel}
    <p>
      <button type="button" onclick={withdraw} disabled={working}>
        {working ? 'Withdrawing…' : 'Withdraw this rip'}
      </button>
      Stops reading and discards anything already sent. The disc is not affected.
    </p>
  {/if}

  <section aria-labelledby="details-heading">
    <h2 id="details-heading">Details</h2>
    <dl>
      <dt>Asked for</dt>
      <dd>{when(rip.created_at)} by {rip.requested_by}</dd>
      <dt>Started</dt>
      <dd>{when(rip.started_at)}</dd>
      <dt>Ended</dt>
      <dd>{when(rip.completed_at)}</dd>
      <dt>Disc found</dt>
      <dd>{rip.medium ? rip.medium.profile : '—'}</dd>
      <dt>Decryption</dt>
      <dd>{rip.decrypt ? `Asked for. Keep: ${keepLabel(rip.keep)}` : 'Not asked for'}</dd>
      <dt>Encryption found</dt>
      <dd>{encryptionLabel(rip.encryption)}</dd>
      {#if rip.decryption}
        <dt>Decrypted with</dt>
        <dd>{decryptionSummary(rip.decryption)}</dd>
      {/if}
      <dt>Read with</dt>
      <dd>{rip.engine ? `${rip.engine} ${rip.engine_version ?? ''}` : '—'}</dd>
      <dt>Read errors</dt>
      <dd>
        {#if rip.read_errors}
          {rip.read_errors.unreadable_sectors} unreadable, {rip.read_errors.retried_sectors} retried
          {rip.read_errors.unreadable_sectors === 0 ? '' : '(the image is incomplete)'}
        {:else}
          —
        {/if}
      </dd>
    </dl>
    {#if rip.files.length > 0}
      <table>
        <caption>Files received</caption>
        <thead>
          <tr><th scope="col">File</th><th scope="col">Size</th><th scope="col">SHA-256</th></tr>
        </thead>
        <tbody>
          {#each rip.files as file (file.name)}
            <tr>
              <th scope="row"><code>{file.name}</code></th>
              <td>{formatBytes(file.bytes)}</td>
              <td><code title={file.sha256}>{file.sha256.slice(0, 12)}…</code></td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}
  </section>

  <section aria-labelledby="events-heading">
    <h2 id="events-heading">What the worker said</h2>
    {#if events.length === 0}
      <p>Nothing yet.</p>
    {:else}
      <ol class="timeline">
        {#each events as event (event.sequence)}
          <li>
            <time datetime={event.worker_time}>{when(event.worker_time)}</time>
            <code>{event.code}</code>
            {#if event.message}<span class="message">{event.message}</span>{/if}
          </li>
        {/each}
      </ol>
    {/if}
  </section>
{/if}

<style>
  .status {
    border: 1px solid currentColor;
    padding: 0.75rem 1rem;
  }
  dl {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 0.25rem 1rem;
  }
  dt {
    font-weight: 600;
  }
  dd {
    margin: 0;
  }
  table {
    border-collapse: collapse;
    width: 100%;
    margin-top: 1rem;
  }
  th,
  td {
    text-align: left;
    padding: 0.5rem;
    border-bottom: 1px solid currentColor;
  }
  caption {
    text-align: left;
    font-weight: 600;
  }
  button {
    padding: 0.5rem 1rem;
  }
  progress {
    width: min(30rem, 100%);
  }
  .timeline {
    padding-left: 1.25rem;
  }
  .message {
    display: block;
    font-family: monospace;
    font-size: 0.9em;
    overflow-wrap: anywhere;
  }
</style>
