<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Preservation DATs (docs/plans/dat-matching.md; UX.md 22): loading the
  // Logiqx XML DATs an administrator has, seeing what each identifies, and
  // removing one. Tangible never downloads a DAT, and a match renames
  // nothing. In the new design (docs/plans/ui-overhaul.md, U8); removing asks
  // first, under the list, and takes the keyboard to the question.
  import Confirm from '$lib/components/ui/Confirm.svelte';
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import EmptyState from '$lib/components/ui/EmptyState.svelte';
  import Field from '$lib/components/ui/Field.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import Panel from '$lib/components/ui/Panel.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import Table from '$lib/components/ui/Table.svelte';
  import { when } from '$lib/overview';
  import {
    MAX_DAT_BYTES,
    datSummary,
    loadDats,
    loadOutcome,
    removeDat,
    uploadDat,
    type Dat,
    type DatLoad,
  } from '$lib/identification';
  import { formatBytes } from '$lib/library';
  import { sessionRole, type Problem } from '$lib/session';
  import { onMount } from 'svelte';

  type View =
    { kind: 'loading' } | { kind: 'ready'; items: Dat[] } | { kind: 'error'; problem: Problem };

  let view = $state<View>({ kind: 'loading' });
  let chosen = $state<File | null>(null);
  let loading = $state(false);
  let outcome = $state<DatLoad | null>(null);
  let loadProblem = $state<Problem | null>(null);
  /** Removing is a second, deliberate action. */
  let confirmingRemove = $state<string | null>(null);
  let removeProblem = $state<Problem | null>(null);
  let done = $state<string | null>(null);

  const administrator = sessionRole() === 'administrator';

  const removing = $derived(
    view.kind === 'ready' ? (view.items.find((dat) => dat.id === confirmingRemove) ?? null) : null,
  );

  function startRemoving(dat: Dat) {
    confirmingRemove = dat.id;
    removeProblem = null;
  }

  async function refresh() {
    const result = await loadDats();
    view = result.kind === 'ok' ? { kind: 'ready', items: result.data.items } : result;
  }

  onMount(refresh);

  function choose(event: Event) {
    const input = event.currentTarget as HTMLInputElement;
    chosen = input.files?.[0] ?? null;
    outcome = null;
    loadProblem = null;
  }

  async function load(event: SubmitEvent) {
    event.preventDefault();
    if (!chosen || loading) return;
    loading = true;
    outcome = null;
    loadProblem = null;
    done = null;
    const result = await uploadDat(chosen);
    loading = false;
    if (result.kind === 'error') {
      loadProblem = result.problem;
      return;
    }
    outcome = result.data;
    await refresh();
  }

  async function remove(dat: Dat) {
    removeProblem = null;
    const result = await removeDat(dat.id);
    if (result.kind === 'error') {
      removeProblem = result.problem;
      return;
    }
    confirmingRemove = null;
    outcome = null;
    done = `Removed ${dat.name}, and the matches it made.`;
    await refresh();
  }

  /** The full date and time, for a time's tooltip. */
  function full(value: string): string {
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
  }
</script>

<svelte:head><title>DATs — Tangible</title></svelte:head>

<PageHeader
  title="DATs"
  description="A DAT lists the known good dumps of a system's discs, by the hashes of their files and tracks. Load the Logiqx XML DATs you use, from Redump, No-Intro or wherever you trust, and every artifact in the library is matched against them: whether it is a known good dump, and of what. Tangible never downloads a DAT, and a match renames and changes nothing."
/>

<div class="stack">
  {#if administrator}
    <Panel title="Load a DAT">
      <form class="load" onsubmit={load}>
        <Field
          label="Logiqx XML DAT"
          hint={`Up to ${formatBytes(MAX_DAT_BYTES)}, unzipped. A DAT with the name of one already loaded replaces it, so loading a newer one updates the old.`}
        >
          {#snippet children({ id, describedBy })}
            <input
              {id}
              type="file"
              accept=".dat,.xml,application/xml,text/xml"
              aria-describedby={describedBy}
              onchange={choose}
            />
          {/snippet}
        </Field>
        <div>
          <Button type="submit" variant="primary" disabled={!chosen || loading}>
            {loading ? 'Loading and matching…' : 'Load'}
          </Button>
        </div>
      </form>
      <div aria-live="polite">
        {#if outcome}
          <div class="outcome">
            <StatusMark
              tone={outcome.warning_count > 0 ? 'warn' : 'ok'}
              label={loadOutcome(outcome)}
            />
            {#if outcome.warnings.length > 0}
              <details>
                <summary>What was left out</summary>
                <ul>
                  {#each outcome.warnings as warning, index (index)}
                    <li>{warning}</li>
                  {/each}
                </ul>
                {#if outcome.warning_count > outcome.warnings.length}
                  <p>And {outcome.warning_count - outcome.warnings.length} more.</p>
                {/if}
              </details>
            {/if}
          </div>
        {/if}
        {#if done}<p class="outcome"><StatusMark tone="ok" label={done} /></p>{/if}
      </div>
      {#if loadProblem}
        <div class="refusal">
          <ErrorPanel problem={loadProblem} area="library" refusal />
        </div>
      {/if}
    </Panel>
  {/if}

  <div aria-live="polite" aria-busy={view.kind === 'loading'}>
    <Panel
      title="Loaded DATs"
      aside={view.kind === 'ready' ? `${view.items.length}` : undefined}
      flush
    >
      {#if view.kind === 'loading'}
        <p class="padded quiet">Loading DATs…</p>
      {:else if view.kind === 'error'}
        <div class="padded">
          <ErrorPanel
            problem={view.problem}
            area="library"
            endpoint="GET /api/v1/dats"
            heading="DATs could not be loaded"
            level={3}
          />
        </div>
      {:else if view.items.length === 0}
        <EmptyState inset level={3} title="No DATs yet" icon="dat">
          <p>
            No DATs loaded yet{#if administrator}; load one above{:else}; an administrator loads
              them{/if}.
          </p>
        </EmptyState>
      {:else}
        <Table caption="Loaded DATs">
          <thead>
            <tr>
              <th scope="col">Name</th>
              <th scope="col">Version</th>
              <th scope="col">Holds</th>
              <th scope="col">Loaded</th>
              {#if administrator}<th scope="col"><span class="visually-hidden">Actions</span></th
                >{/if}
            </tr>
          </thead>
          <tbody>
            {#each view.items as dat (dat.id)}
              <tr>
                <th scope="row">
                  <div class="cell">
                    <span>{dat.name}</span>
                    {#if dat.description && dat.description !== dat.name}
                      <span class="small quiet">{dat.description}</span>
                    {/if}
                  </div>
                </th>
                <td
                  >{#if dat.version}<code>{dat.version}</code>{:else}<span class="quiet"
                      >None given</span
                    >{/if}</td
                >
                <td>{datSummary(dat)}</td>
                <td>
                  <time datetime={dat.loaded_at} title={full(dat.loaded_at)}
                    >{when(dat.loaded_at)}</time
                  >
                  <span class="quiet">by {dat.loaded_by}</span>
                </td>
                {#if administrator}
                  <td>
                    <div class="row-actions">
                      <Button
                        size="sm"
                        variant="danger"
                        onclick={() => startRemoving(dat)}
                        label={`Remove ${dat.name}`}>Remove</Button
                      >
                    </div>
                  </td>
                {/if}
              </tr>
            {/each}
          </tbody>
        </Table>
      {/if}

      {#if administrator && removing}
        {@const dat = removing}
        {#key dat.id}
          <Confirm title={`Remove ${dat.name}?`}>
            <p>
              Its games and the matches it made are deleted. Nothing in the library is touched, and
              loading it again restores them.
            </p>
            {#if removeProblem}
              <ErrorPanel problem={removeProblem} area="library" refusal />
            {/if}

            {#snippet actions()}
              <Button variant="danger" onclick={() => remove(dat)}>Yes, remove {dat.name}</Button>
              <Button onclick={() => (confirmingRemove = null)}>Keep it</Button>
            {/snippet}
          </Confirm>
        {/key}
      {/if}
    </Panel>
  </div>
</div>

<style>
  .stack {
    display: grid;
    gap: var(--space-5);
  }
  .small {
    font-size: var(--text-xs);
    font-weight: var(--weight-regular);
  }
  .padded {
    margin: 0;
    padding: var(--space-3) var(--space-4);
  }
  .cell {
    display: grid;
    gap: 0.125rem;
  }
  .row-actions {
    display: flex;
    justify-content: flex-end;
  }
  .load {
    display: grid;
    gap: var(--space-3);
    justify-items: start;
    max-width: 40rem;
  }
  .outcome {
    display: grid;
    gap: var(--space-2);
    justify-items: start;
    margin: var(--space-4) 0 0;
  }
  .outcome :global(.mark) {
    font-weight: var(--weight-medium);
  }
  .outcome ul {
    margin: var(--space-2) 0 0;
    font-size: var(--text-sm);
  }
  summary {
    cursor: pointer;
    color: var(--text-muted);
  }
  .refusal {
    margin-top: var(--space-4);
  }
</style>
