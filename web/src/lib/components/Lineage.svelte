<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { onDestroy } from 'svelte';
  import {
    isOpen,
    jobStateLabel,
    loadDerivationJob,
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

  async function makeChd() {
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
      status = 'A CHD of this already exists.';
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
    <p>Lineage could not be loaded: {loadProblem.detail}</p>
  {:else if !lineage}
    <p>Loading lineage…</p>
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
      <ul>
        {#each lineage.derivatives as derivative (derivative.child_artifact_id)}
          <li>
            <a href={`/library/${derivative.child_artifact_id}`}
              >{transformationLabel(derivative.transformation)}</a
            >, by {derivative.tool_name}
            {derivative.tool_version}: {lossLabel(derivative.loss_character)}
          </li>
        {/each}
      </ul>
    {:else if !lineage.derived_from}
      <p>Nothing has been made from this artifact.</p>
    {/if}

    {#if lineage.jobs.some((job) => job.state.startsWith('failed'))}
      <h3>Failed attempts</h3>
      <ul>
        {#each lineage.jobs.filter((job) => job.state.startsWith('failed')) as job (job.id)}
          <li>
            {transformationLabel(job.transformation)}: {jobStateLabel(job)}
            {#if job.error_detail}<br /><small>{job.error_detail}</small>{/if}
          </li>
        {/each}
      </ul>
    {/if}

    {#if lineage.suggested_transformation}
      <p>
        <button type="button" onclick={makeChd} disabled={asking}>
          {asking ? 'Asking…' : 'Make a CHD'}
        </button>
        <br />
        <small
          >Makes a new, compressed copy that RomM and its emulators read. The original is not
          changed. The copy is kept only if it extracts back identical to this one, track by track.</small
        >
      </p>
    {/if}
    <div role="alert">
      {#if askProblem}<p><strong>Not started.</strong> {askProblem.detail}</p>{/if}
    </div>
    <p aria-live="polite">{status}</p>
  {/if}
</section>
