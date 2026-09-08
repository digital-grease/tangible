<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { onMount } from 'svelte';
  import { page } from '$app/state';
  import { createBurnJob, newIdempotencyKey, type BurnJobCreated } from '$lib/burns';
  import {
    formatBytes,
    formatLabel,
    loadArtifact,
    validationLabel,
    type ArtifactDetail,
    type LoadState,
    type Problem,
  } from '$lib/library';

  let artifactId = $state('');
  let discId = $state('');
  let verification = $state('full_sector_readback');
  let ejectPolicy = $state('eject_on_success');
  let priority = $state(0);

  let artifact = $state<LoadState<ArtifactDetail> | null>(null);
  let created = $state<BurnJobCreated | null>(null);
  let problem = $state<Problem | null>(null);
  let submitting = $state(false);

  /**
   * The key for the submission in flight.
   *
   * Kept across retries and cleared only once a job exists, so an operator who
   * presses the button again after a lost response gets the same burn rather
   * than a second disc. A new key is minted for the next burn.
   */
  let idempotencyKey = '';

  onMount(async () => {
    artifactId = page.url.searchParams.get('artifact') ?? '';
    // Prefilled when the operator arrived from the catalog, which is where
    // choosing a disc actually happens: a disc means "disc two of the
    // two-disc edition", not a UUID somebody remembered.
    discId = page.url.searchParams.get('disc') ?? '';
    if (artifactId) artifact = await loadArtifact(artifactId);
  });

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (submitting) return;
    submitting = true;
    problem = null;
    if (!idempotencyKey) idempotencyKey = newIdempotencyKey();

    const result = await createBurnJob(
      {
        artifact_id: artifactId.trim(),
        disc_id: discId.trim(),
        verification_policy: [verification],
        eject_policy: ejectPolicy,
        priority,
      },
      idempotencyKey,
    );

    if (result.kind === 'error') {
      problem = result.problem;
    } else {
      created = result.data;
      // Spent. The next burn is a different intent and needs its own key.
      idempotencyKey = '';
    }
    submitting = false;
  }
</script>

<svelte:head><title>Create burn job — Tangible</title></svelte:head>

<p><a href="/burns">← Burn queue</a></p>

<h1>Create burn job</h1>

{#if created}
  <section aria-live="polite" aria-labelledby="queued-heading" class="queued">
    <h2 id="queued-heading">Queued</h2>
    <!-- Queued is not started. Saying so avoids an operator standing at a
         drive waiting for a light that will not come on yet. -->
    <p>
      The burn is queued. Nothing is written until a worker with a compatible drive claims it and
      its preflight passes.
    </p>
    <ul>
      {#each created.warnings as warning, index (index)}
        <li>{warning}</li>
      {/each}
    </ul>
    <p><a href={`/burns/${created.id}`}>Follow this burn</a></p>
  </section>
{:else}
  <form onsubmit={submit}>
    <section aria-labelledby="what-heading">
      <h2 id="what-heading">What to write</h2>

      <p>
        <label for="artifact">Artifact identifier</label><br />
        <input id="artifact" name="artifact" bind:value={artifactId} required />
      </p>

      {#if artifact?.kind === 'ready'}
        <dl>
          <dt>Name</dt>
          <dd>{artifact.data.source_filename ?? artifact.data.id}</dd>
          <dt>Format</dt>
          <dd>{formatLabel(artifact.data.format)}</dd>
          <dt>Size</dt>
          <dd>{formatBytes(artifact.data.total_bytes)}</dd>
          <dt>Validation</dt>
          <dd>{validationLabel(artifact.data.validation_state)}</dd>
        </dl>
      {:else if artifact?.kind === 'error'}
        <p>That artifact could not be read: {artifact.problem.detail}</p>
      {/if}

      <p>
        <label for="disc">Disc identifier</label><br />
        <input id="disc" name="disc" bind:value={discId} required />
        <br />
        <small>
          The catalog entry this copy represents.
          <a href="/catalog">Find it in the catalog</a> and choose "Burn to this disc", or paste an identifier.
        </small>
      </p>
    </section>

    <section aria-labelledby="policy-heading">
      <h2 id="policy-heading">Policy</h2>

      <p>
        <label for="verification">Verification</label><br />
        <select id="verification" name="verification" bind:value={verification}>
          <option value="full_sector_readback">Read the whole disc back and compare</option>
          <option value="filesystem_compare">Compare the filesystem</option>
          <option value="tool_verify">Trust the engine's own check</option>
          <option value="none">Do not verify</option>
        </select>
        <br />
        <small>
          Only reading the disc back establishes that it holds what was intended. A successful
          engine exit does not.
        </small>
      </p>

      <p>
        <label for="eject">When the attempt ends</label><br />
        <select id="eject" name="eject" bind:value={ejectPolicy}>
          <option value="eject_on_success">Eject only a verified disc</option>
          <option value="never">Leave the disc in the drive</option>
          <option value="always">Always eject</option>
        </select>
        <br />
        <small>A disc that failed verification is left where you will find it.</small>
      </p>

      <p>
        <label for="priority">Priority</label><br />
        <input
          id="priority"
          name="priority"
          type="number"
          bind:value={priority}
          min="-100"
          max="100"
        />
        <br />
        <small>Higher is claimed first.</small>
      </p>
    </section>

    <section aria-labelledby="review-heading">
      <h2 id="review-heading">Before you queue this</h2>
      <ul>
        <li>Queueing does not start a write. A worker claims the job and checks the disc first.</li>
        <li>
          Target compatibility is unknown. A verified disc holds the same bytes, which is not a
          promise that any particular player, console or drive will accept it.
        </li>
        <li>A burn consumes a blank disc, including when it fails.</li>
      </ul>

      {#if problem}
        <div class="refusal" aria-live="assertive">
          <h3>{problem.title}</h3>
          <p>{problem.detail}</p>
          <p>Error code: <code>{problem.code}</code></p>
        </div>
      {/if}

      <button type="submit" disabled={submitting}>
        {submitting ? 'Queueing…' : 'Queue this burn'}
      </button>
    </section>
  </form>
{/if}

<style>
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
  label {
    font-weight: 600;
  }
  input,
  select {
    padding: 0.4rem;
    min-width: min(28rem, 100%);
  }
  small {
    display: inline-block;
    max-width: 40rem;
    font-size: 0.9em;
  }
  button {
    padding: 0.5rem 1rem;
  }
  .queued,
  .refusal {
    border: 1px solid currentColor;
    padding: 0.75rem 1rem;
  }
</style>
