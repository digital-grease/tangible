<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import Callout from '$lib/components/ui/Callout.svelte';
  import { shortId } from '$lib/ids';
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import EmptyState from '$lib/components/ui/EmptyState.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import Panel from '$lib/components/ui/Panel.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import Table from '$lib/components/ui/Table.svelte';
  import { apiFetch } from '$lib/session';
  import { onMount } from 'svelte';
  import { conditionLabel, conditionTone, loadDiscs, type PhysicalCopyPage } from '$lib/discs';
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

<svelte:head><title>Physical copies — Tangible</title></svelte:head>

<PageHeader
  title="Physical copies"
  description="Every burn that consumed media produced a disc, including the burns that failed. A disc nobody tracked is the one that gets shelved, found later, and trusted."
/>

<div aria-live="polite" aria-busy={view.kind === 'loading'}>
  {#if attention.length > 0}
    <!-- The line on this page that needs somebody to go and do something. -->
    <Callout
      tone="bad"
      marked
      title={`${attention.length} ${attention.length === 1 ? 'disc should be destroyed' : 'discs should be destroyed'}`}
    >
      <p>
        These do not hold what was intended, or are no longer trusted to. Destroy them rather than
        returning them to a shelf.
      </p>
      <ul>
        {#each attention as copy (copy.id)}
          <li>
            <a href={`/discs/${copy.id}`}>{copy.label ?? shortId(copy.id)}</a>:
            {conditionLabel(copy.status)}
          </li>
        {/each}
      </ul>
    </Callout>
  {/if}

  <div class="filter">
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
  </div>

  {#if view.kind === 'loading'}
    <p class="quiet">Loading discs…</p>
  {:else if view.kind === 'empty'}
    <EmptyState title="No discs yet" icon="disc">
      <p>A disc appears here the moment a burn starts consuming one.</p>
      {#snippet action()}<Button href="/burns" icon="burn">Burn queue</Button>{/snippet}
    </EmptyState>
  {:else if view.kind === 'error'}
    <ErrorPanel
      problem={view.problem}
      area="other"
      endpoint="GET /api/v1/physical-copies"
      heading="Could not load the disc inventory"
    />
  {:else}
    <Panel title="Discs" aside={String(view.data.items.length)} flush>
      <Table caption="Discs, newest first">
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
                <a href={`/discs/${copy.id}`}>{copy.label ?? shortId(copy.id)}</a>
              </th>
              <td>
                <StatusMark
                  tone={conditionTone(copy.status)}
                  label={conditionLabel(copy.status)}
                  pill
                />
              </td>
              <td>{copy.media_profile}</td>
              <td>{copy.storage_location ?? '·'}</td>
              <td>
                {#if copy.last_checked_at}
                  <time datetime={copy.last_checked_at}
                    >{new Date(copy.last_checked_at).toLocaleDateString()}</time
                  >
                  <span class="quiet">({copy.check_count})</span>
                {:else}
                  <span class="quiet">Never</span>
                {/if}
              </td>
            </tr>
          {/each}
        </tbody>
      </Table>
    </Panel>
  {/if}
</div>

<style>
  .filter {
    display: flex;
    gap: var(--space-2);
    align-items: center;
    margin-bottom: var(--space-4);
  }
  .filter label {
    color: var(--text-muted);
    font-size: var(--text-sm);
  }
</style>
