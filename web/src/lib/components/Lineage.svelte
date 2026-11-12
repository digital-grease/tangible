<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { onDestroy } from 'svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import {
    isOpen,
    jobStateLabel,
    loadDerivationJob,
    derivativeOffer,
    loadLineage,
    lossLabel,
    requestDerivative,
    transformationLabel,
    type Lineage,
  } from '$lib/derivations';
  import type { Problem } from '$lib/session';

  let { artifactId }: { artifactId: string } = $props();

  let lineage = $state<Lineage | null>(null);
  let loadProblem = $state<Problem | null>(null);
  let askProblem = $state<Problem | null>(null);
  let asking = $state(false);
  let status = $state('');
  let timer: ReturnType<typeof setTimeout> | undefined;

  async function load(id: string) {
    const result = await loadLineage(id);
    if (result.kind === 'ok') {
      lineage = result.data;
      loadProblem = null;
      const open = result.data.jobs.find(isOpen);
      if (open) follow(id, open.id);
    } else {
      loadProblem = result.problem;
    }
  }

  $effect(() => {
    void load(artifactId);
  });
  onDestroy(() => clearTimeout(timer));

  /** Check a running job every few seconds until it finishes. */
  function follow(id: string, jobId: string) {
    clearTimeout(timer);
    timer = setTimeout(async () => {
      const result = await loadDerivationJob(jobId);
      if (result.kind === 'ok') {
        status = `${transformationLabel(result.data.transformation)}: ${jobStateLabel(result.data)}`;
        if (isOpen(result.data)) {
          follow(id, jobId);
          return;
        }
      }
      await load(id);
    }, 3000);
  }

  const offer = $derived(
    lineage?.suggested_transformation ? derivativeOffer(lineage.suggested_transformation) : null,
  );

  async function makeDerivative() {
    if (asking) return;
    asking = true;
    askProblem = null;
    const result = await requestDerivative(artifactId);
    asking = false;
    if (result.kind === 'error') {
      askProblem = result.problem;
      return;
    }
    if (result.data.result === 'exists') {
      status = offer?.exists ?? 'It already exists.';
    } else if (result.data.job) {
      status = `${transformationLabel(result.data.job.transformation)}: ${jobStateLabel(result.data.job)}`;
      follow(artifactId, result.data.job.id);
    }
    await load(artifactId);
  }
</script>

<section aria-labelledby="lineage-heading">
  <h2 id="lineage-heading">Derivatives</h2>
  {#if loadProblem}
    <p class="problem">Lineage could not be loaded: {loadProblem.detail}</p>
  {:else if !lineage}
    <p class="quiet">Loading lineage…</p>
  {:else}
    {#if lineage.derived_from}
      <p>
        Made from
        <a href={`/library/${lineage.derived_from.parent_artifact_id}`}>its original</a>
        by {lineage.derived_from.tool_name}
        {lineage.derived_from.tool_version}:
        <strong>{lossLabel(lineage.derived_from.loss_character)}</strong>.
      </p>
    {/if}

    {#if lineage.derivatives.length > 0}
      <ul class="list">
        {#each lineage.derivatives as derivative (derivative.child_artifact_id)}
          <li>
            <a href={`/library/${derivative.child_artifact_id}`}
              >{transformationLabel(derivative.transformation)}</a
            >
            <span class="quiet"
              >by {derivative.tool_name}
              {derivative.tool_version}: {lossLabel(derivative.loss_character)}</span
            >
          </li>
        {/each}
      </ul>
    {:else if !lineage.derived_from}
      <p class="quiet">Nothing has been made from this artifact.</p>
    {/if}

    {#if lineage.jobs.some((job) => job.state.startsWith('failed'))}
      <h3>Failed attempts</h3>
      <ul class="list">
        {#each lineage.jobs.filter((job) => job.state.startsWith('failed')) as job (job.id)}
          <li>
            <StatusMark
              tone="bad"
              label={`${transformationLabel(job.transformation)}: ${jobStateLabel(job)}`}
            />
            {#if job.error_detail}<span class="hint">{job.error_detail}</span>{/if}
          </li>
        {/each}
      </ul>
    {/if}

    {#if offer}
      <div class="action">
        <Button onclick={makeDerivative} disabled={asking}>
          {asking ? 'Asking…' : offer.action}
        </Button>
        <p class="hint">{offer.explanation}</p>
      </div>
    {/if}
    <div role="alert">
      {#if askProblem}<p class="problem"><strong>Not started.</strong> {askProblem.detail}</p>{/if}
    </div>
    <p aria-live="polite" class="status">{status}</p>
  {/if}
</section>

<style>
  .list {
    display: grid;
    gap: var(--space-2);
    margin: 0 0 var(--space-4);
    padding: 0;
    list-style: none;
  }
  .list li {
    display: grid;
    gap: 0.125rem;
    justify-items: start;
  }
  .list :global(.mark) {
    font-weight: var(--weight-medium);
  }
  h3 {
    margin: 0 0 var(--space-2);
    font-size: var(--text-base);
  }
  .action {
    display: grid;
    gap: var(--space-2);
    justify-items: start;
  }
  .hint {
    max-width: 75ch;
    margin: 0;
    font-size: var(--text-sm);
    color: var(--text-muted);
  }
  .problem {
    color: var(--bad);
  }
  .status {
    margin: 0;
  }
</style>
