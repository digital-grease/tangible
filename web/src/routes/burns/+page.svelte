<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import EmptyState from '$lib/components/ui/EmptyState.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import Panel from '$lib/components/ui/Panel.svelte';
  import Progress from '$lib/components/ui/Progress.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import Table from '$lib/components/ui/Table.svelte';
  import { apiFetch } from '$lib/session';
  import { onDestroy, onMount } from 'svelte';
  import {
    isLive,
    jobStateLabel,
    jobStateTone,
    loadBurnQueue,
    progressPercent,
    stageLabel,
    type BurnJobPage,
  } from '$lib/burns';
  import type { LoadState } from '$lib/library';
  import { watch } from '$lib/events';

  let view = $state<LoadState<BurnJobPage>>({ kind: 'loading' });
  let loadingMore = $state(false);
  let stopWatching: (() => void) | undefined;

  /**
   * How often the queue polls where the browser has no event stream. With
   * one, it refreshes as burns change and reconciles every minute.
   */
  const FALLBACK_MS = 10_000;

  async function refresh() {
    const next = await loadBurnQueue();
    // A failed poll leaves what is on screen alone. Replacing a readable queue
    // with an error because one refresh missed would be worse than stale.
    if (next.kind === 'error' && view.kind === 'ready') return;
    view = next;
  }

  onMount(async () => {
    await refresh();
    stopWatching = watch(['burns'], refresh, { fallbackMs: FALLBACK_MS });
  });

  onDestroy(() => stopWatching?.());

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

<PageHeader title="Burn queue">
  {#snippet actions()}
    <Button variant="primary" icon="burn" href="/burns/new">Create burn job</Button>
  {/snippet}
</PageHeader>

<!-- One live region for the whole view, so a change is announced once rather
     than per row. -->
<div aria-live="polite" aria-busy={view.kind === 'loading'}>
  {#if view.kind === 'loading'}
    <p class="quiet">Loading the burn queue…</p>
  {:else if view.kind === 'empty'}
    <EmptyState title="No burn jobs" icon="burn">
      <p>Select a compatible artifact and choose <strong>Create burn job</strong>.</p>
      {#snippet action()}<Button href="/library" icon="library">Browse the library</Button
        >{/snippet}
    </EmptyState>
  {:else if view.kind === 'error'}
    <ErrorPanel
      problem={view.problem}
      area="other"
      endpoint="GET /api/v1/burn-jobs"
      heading="Could not load the burn queue"
    />
  {:else}
    {#if waiting.length > 0}
      <!-- The one thing on this page that needs a person to get up and do
           something. It goes first, and says which job. -->
      <section class="callout" aria-labelledby="waiting-heading">
        <h2 id="waiting-heading">
          <StatusMark
            tone="warn"
            label={waiting.length === 1
              ? 'A burn is waiting for a disc'
              : 'Burns are waiting for a disc'}
          />
        </h2>
        <ul>
          {#each waiting as job (job.id)}
            <li>
              <a href={`/burns/${job.id}`}>Insert a disc for burn {job.id.slice(0, 8)}</a>
              {#if job.requested_media_profile}
                <span class="quiet">: {job.requested_media_profile}</span>
              {/if}
            </li>
          {/each}
        </ul>
      </section>
    {/if}

    <Panel
      title="Burn jobs"
      aside={`${view.data.items.length} shown, ${active.length} running or queued`}
      flush
    >
      <Table caption="Burn jobs, newest first">
        <thead>
          <tr>
            <th scope="col">Burn</th>
            <th scope="col">State</th>
            <th scope="col">Stage</th>
            <th scope="col" class="num">Attempts</th>
            <th scope="col">Queued</th>
          </tr>
        </thead>
        <tbody>
          {#each view.data.items as job (job.id)}
            {@const percent = job.progress ? progressPercent(job.progress.fraction) : null}
            <tr>
              <th scope="row"><a href={`/burns/${job.id}`}><code>{job.id.slice(0, 8)}</code></a></th
              >
              <td>
                <StatusMark tone={jobStateTone(job.state)} label={jobStateLabel(job.state)} pill />
              </td>
              <td>
                {#if job.progress}
                  <div class="stage">
                    <span
                      >{stageLabel(job.progress.stage)}{percent !== null
                        ? `, ${percent}%`
                        : ''}</span
                    >
                    {#if percent !== null}
                      <Progress
                        value={percent / 100}
                        label={`Burn ${job.id.slice(0, 8)}: ${stageLabel(job.progress.stage)}`}
                        tone="laser"
                      />
                    {/if}
                  </div>
                {:else}
                  <span class="quiet">·</span>
                {/if}
              </td>
              <td class="num">
                {job.attempt_count}
                {#if job.attempt_count > 1}
                  <small class="quiet">({job.attempt_count} discs)</small>
                {/if}
              </td>
              <td
                ><time datetime={job.created_at}>{new Date(job.created_at).toLocaleString()}</time
                ></td
              >
            </tr>
          {/each}
        </tbody>
      </Table>
    </Panel>

    {#if view.data.next_cursor}
      <div class="more">
        <Button onclick={loadMore} disabled={loadingMore}>
          {loadingMore ? 'Loading…' : 'Load more'}
        </Button>
      </div>
    {/if}
  {/if}
</div>

<style>
  .callout {
    padding: var(--space-3) var(--space-4);
    margin-bottom: var(--space-4);
    background: var(--surface);
    border: 1px solid var(--line);
    border-left: 4px solid var(--warn);
    border-radius: var(--radius);
  }
  .callout h2 {
    margin: 0 0 var(--space-2);
    font-size: var(--text-md);
  }
  .callout ul {
    margin: 0;
    padding-left: 1.25rem;
  }
  .stage {
    display: grid;
    gap: var(--space-1);
    max-width: 12rem;
  }
  .more {
    margin-top: var(--space-4);
  }
  .quiet {
    color: var(--text-muted);
  }
</style>
