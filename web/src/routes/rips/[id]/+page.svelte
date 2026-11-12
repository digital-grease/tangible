<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // One rip: what the drive held, how far reading has got, what the worker
  // said, and where the result went. Withdrawing is offered while the disc
  // is being read or sent; reading changes nothing on it.
  import { shortDigest } from '$lib/ids';
  import { onDestroy, onMount } from 'svelte';
  import { names, watch } from '$lib/events';
  import { page } from '$app/state';
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import Panel from '$lib/components/ui/Panel.svelte';
  import Progress from '$lib/components/ui/Progress.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import Table from '$lib/components/ui/Table.svelte';
  import { formatBytes } from '$lib/library';
  import {
    cancelRip,
    cdEngineLabel,
    decryptionSummary,
    encryptionLabel,
    genericDriveLabel,
    keepLabel,
    loadRip,
    loadRipEvents,
    ripProgress,
    ripRefusal,
    ripStateLabel,
    ripStateTone,
    readOffsetLabel,
    retriedLabel,
    writeOffsetLabel,
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
  let stopWatching: (() => void) | undefined;

  /** How often an open rip polls where the browser has no event stream. */
  const FALLBACK_MS = 2_000;
  const id = $derived(page.params.id ?? '');
  const rip = $derived(view.kind === 'ready' ? view.data : null);
  const progress = $derived(ripProgress(events));
  const readOffset = $derived(rip ? readOffsetLabel(rip.engine, rip.cd_read) : null);

  async function refresh() {
    const [next, timeline] = await Promise.all([loadRip(id), loadRipEvents(id)]);
    if (next.kind === 'ok') view = { kind: 'ready', data: next.data };
    // A failed poll leaves what is on screen alone.
    else if (view.kind !== 'ready') view = { kind: 'error', problem: next.problem };
    if (timeline.kind === 'ok') events = timeline.data.items;
  }

  onMount(() => {
    void refresh();
    stopWatching = watch(
      ['rips'],
      (reason) => {
        if (reason === 'event' || rip?.is_open !== false) void refresh();
      },
      { fallbackMs: FALLBACK_MS, relevant: (event) => names(event, id, 'rip_id') },
    );
  });

  onDestroy(() => stopWatching?.());

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

{#if view.kind === 'loading'}
  <p class="quiet">Loading rip…</p>
{:else if view.kind === 'error'}
  <p><a href="/rips">Rips</a></p>
  <ErrorPanel
    problem={view.problem}
    area="import"
    endpoint="GET /api/v1/rips/{'{id}'}"
    heading="Could not load this rip"
    level={1}
  />
{:else if rip}
  <PageHeader
    title={`Rip of the disc in ${rip.drive_name}`}
    crumbs={[{ label: 'Rips', href: '/rips' }, { label: rip.drive_name }]}
  >
    {#snippet actions()}
      {#if rip.can_cancel}
        <Button variant="danger" onclick={withdraw} disabled={working}>
          {working ? 'Withdrawing…' : 'Withdraw this rip'}
        </Button>
      {/if}
    {/snippet}
  </PageHeader>

  <section aria-labelledby="status-heading" class="status" aria-live="polite">
    <h2 id="status-heading">
      <StatusMark tone={ripStateTone(rip.state)} label={ripStateLabel(rip.state)} />
    </h2>
    {#if rip.state === 'reading' && progress !== null}
      <div class="progress">
        <Progress value={progress / 100} label="Reading the disc" tone="laser" />
        <span class="num">{progress}% read</span>
      </div>
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
    {#if rip.can_cancel}
      <p class="quiet small">
        Withdrawing stops reading and discards anything already sent. The disc is not affected.
      </p>
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

  <div class="columns">
    <Panel title="Details">
      <dl class="facts">
        <div>
          <dt>Asked for</dt>
          <dd>{when(rip.created_at)} by {rip.requested_by}</dd>
        </div>
        <div>
          <dt>Started</dt>
          <dd>{when(rip.started_at)}</dd>
        </div>
        <div>
          <dt>Ended</dt>
          <dd>{when(rip.completed_at)}</dd>
        </div>
        <div>
          <dt>Disc found</dt>
          <dd>{rip.medium ? rip.medium.profile : '·'}</dd>
        </div>
        {#if rip.cd_engine}
          <div>
            <dt>CD tool asked for</dt>
            <dd>{cdEngineLabel(rip.cd_engine)}</dd>
          </div>
        {/if}
        <div>
          <dt>Decryption</dt>
          <dd>{rip.decrypt ? `Asked for. Keep: ${keepLabel(rip.keep)}` : 'Not asked for'}</dd>
        </div>
        <div>
          <dt>Encryption found</dt>
          <dd>{encryptionLabel(rip.encryption)}</dd>
        </div>
        {#if rip.decryption}
          <div>
            <dt>Decrypted with</dt>
            <dd>{decryptionSummary(rip.decryption)}</dd>
          </div>
        {/if}
        <div>
          <dt>Read with</dt>
          <dd>{rip.engine ? `${rip.engine} ${rip.engine_version ?? ''}` : '·'}</dd>
        </div>
        {#if readOffset}
          <div>
            <dt>Read offset</dt>
            <dd>{readOffset}</dd>
          </div>
        {/if}
        {#if rip.cd_read}
          <div>
            <dt>Write offset</dt>
            <dd>{writeOffsetLabel(rip.cd_read)}</dd>
          </div>
          <div>
            <dt>Generic drive</dt>
            <dd>{genericDriveLabel(rip.cd_read.generic_drive)}</dd>
          </div>
        {/if}
        <div>
          <dt>Read errors</dt>
          <dd>
            {#if rip.read_errors}
              {rip.read_errors.unreadable_sectors} unreadable, {rip.read_errors.retried_sectors} retried
              {rip.read_errors.unreadable_sectors === 0 ? '' : '(the image is incomplete)'}
            {:else if rip.cd_read}
              {retriedLabel(rip.cd_read)}
            {:else}
              ·
            {/if}
          </dd>
        </div>
      </dl>
      {#if rip.files.length > 0}
        <div class="files">
          <Table caption="Files received" showCaption>
            <thead>
              <tr>
                <th scope="col">File</th>
                <th scope="col" class="num">Size</th>
                <th scope="col">SHA-256</th>
              </tr>
            </thead>
            <tbody>
              {#each rip.files as file (file.name)}
                <tr>
                  <th scope="row"><code>{file.name}</code></th>
                  <td class="num">{formatBytes(file.bytes)}</td>
                  <td><code title={file.sha256}>{shortDigest(file.sha256)}</code></td>
                </tr>
              {/each}
            </tbody>
          </Table>
        </div>
      {/if}
    </Panel>

    <Panel title="What the worker said">
      {#if events.length === 0}
        <p class="quiet">Nothing yet.</p>
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
    </Panel>
  </div>
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
  .progress {
    display: grid;
    grid-template-columns: minmax(0, 30rem) auto;
    gap: var(--space-3);
    align-items: center;
    font-size: var(--text-sm);
  }
  .columns {
    display: grid;
    grid-template-columns: minmax(0, 1.4fr) minmax(0, 1fr);
    gap: var(--space-4);
    align-items: start;
  }
  @media (max-width: 1099px) {
    .columns {
      grid-template-columns: minmax(0, 1fr);
    }
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
  .files {
    margin-top: var(--space-4);
  }
  .timeline {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    font-size: var(--text-sm);
  }
  .timeline li {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 0 var(--space-3);
    padding: var(--space-2) 0;
    border-bottom: 1px solid var(--line-2);
  }
  .timeline li:last-child {
    border-bottom: 0;
  }
  .timeline time {
    color: var(--text-muted);
  }
  .message {
    grid-column: 1 / -1;
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    color: var(--text-muted);
    overflow-wrap: anywhere;
  }
  .small {
    font-size: var(--text-sm);
  }
  .quiet {
    color: var(--text-muted);
  }
</style>
