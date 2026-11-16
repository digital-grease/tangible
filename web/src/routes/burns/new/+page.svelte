<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import DiscPicker from '$lib/components/DiscPicker.svelte';
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import Disc from '$lib/components/ui/Disc.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import { onMount } from 'svelte';
  import { page } from '$app/state';
  import { createBurnJob, newIdempotencyKey, type BurnJobCreated } from '$lib/burns';
  import { discArtifacts } from '$lib/catalog';
  import {
    artifactName,
    formatBytes,
    formatLabel,
    loadArtifact,
    validationLabel,
    validationTone,
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
  /** Whether what the address named has been read, so the picker starts from it. */
  let ready = $state(false);
  /** Started again whenever the artifact changes, to search by the new one. */
  let pickerRun = $state(0);
  /** A chosen disc's artifacts, offered while no artifact is chosen. */
  let linked = $state<ArtifactDetail[]>([]);
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
    ready = true;
  });

  /** Read the artifact typed or pasted, and search for its disc again. */
  async function changeArtifact() {
    const id = artifactId.trim();
    artifact = id ? await loadArtifact(id) : null;
    pickerRun += 1;
  }

  function useArtifact(chosen: ArtifactDetail) {
    artifactId = chosen.id;
    artifact = { kind: 'ready', data: chosen };
    linked = [];
    pickerRun += 1;
  }

  // Arriving from the catalog with a disc and no artifact: offer what is
  // linked to that disc, so its identifier need not be found elsewhere.
  $effect(() => {
    const disc = discId;
    if (!disc || artifactId.trim()) {
      linked = [];
      return;
    }
    let current = true;
    void (async () => {
      const links = await discArtifacts(disc);
      if (links.kind !== 'ready') return;
      const read = await Promise.all(
        links.data.slice(0, 5).map((link) => loadArtifact(link.artifact_id)),
      );
      if (current) {
        linked = read.flatMap((state) => (state.kind === 'ready' ? [state.data] : []));
      }
    })();
    return () => {
      current = false;
    };
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

<PageHeader
  title="Create burn job"
  crumbs={[{ label: 'Burn queue', href: '/burns' }, { label: 'Create burn job' }]}
/>

{#if created}
  <section aria-live="polite" aria-labelledby="queued-heading" class="queued">
    <h2 id="queued-heading"><StatusMark tone="busy" label="Queued" /></h2>
    <!-- Queued is not started. Saying so avoids an operator standing at a
         drive waiting for a light that will not come on yet. -->
    <p>
      The burn is queued. Nothing is written until a worker with a compatible drive claims it and
      its preflight passes.
    </p>
    {#if created.warnings.length > 0}
      <ul class="warnings">
        {#each created.warnings as warning, index (index)}
          <li><StatusMark tone="warn" label={warning} /></li>
        {/each}
      </ul>
    {/if}
    <p><Button variant="primary" href={`/burns/${created.id}`}>Follow this burn</Button></p>
  </section>
{:else}
  <form class="layout" onsubmit={submit}>
    <div class="stack">
      <section aria-labelledby="what-heading" class="panel">
        <h2 id="what-heading">What to write</h2>
        <div class="what">
          <div class="fields">
            <label for="artifact">Artifact identifier</label>
            <input
              id="artifact"
              name="artifact"
              bind:value={artifactId}
              onchange={changeArtifact}
              required
            />
            {#if linked.length > 0}
              <div class="linked" role="group" aria-label="Artifacts linked to this disc">
                <p class="hint">Linked to this disc:</p>
                <ul>
                  {#each linked as candidate (candidate.id)}
                    <li>
                      <span class="name">{artifactName(candidate) ?? candidate.id}</span>
                      <span class="hint"
                        >{formatLabel(candidate.format)}, {formatBytes(candidate.total_bytes)}</span
                      >
                      <Button
                        size="sm"
                        onclick={() => useArtifact(candidate)}
                        label={`Use ${artifactName(candidate) ?? candidate.id}`}
                        >Use this artifact</Button
                      >
                    </li>
                  {/each}
                </ul>
              </div>
            {/if}

            {#if artifact?.kind === 'ready'}
              <dl class="facts">
                <div>
                  <dt>Name</dt>
                  <dd>{artifactName(artifact.data) ?? artifact.data.id}</dd>
                </div>
                <div>
                  <dt>Format</dt>
                  <dd>{formatLabel(artifact.data.format)}</dd>
                </div>
                <div>
                  <dt>Size</dt>
                  <dd class="num">{formatBytes(artifact.data.total_bytes)}</dd>
                </div>
                <div>
                  <dt>Validation</dt>
                  <dd>
                    <StatusMark
                      tone={validationTone(artifact.data.validation_state)}
                      label={validationLabel(artifact.data.validation_state)}
                    />
                  </dd>
                </div>
              </dl>
            {:else if artifact?.kind === 'error'}
              <p class="refused">That artifact could not be read: {artifact.problem.detail}</p>
            {/if}

            {#if ready}
              {#key pickerRun}
                <DiscPicker
                  artifactId={artifact?.kind === 'ready' ? artifact.data.id : ''}
                  artifact={artifact?.kind === 'ready' ? artifact.data : null}
                  bind:discId
                />
              {/key}
            {/if}
          </div>
          {#if artifact?.kind === 'ready'}
            <Disc shape={artifact.data.disc_shape} size={120} hole="var(--surface)" />
          {/if}
        </div>
      </section>

      <section aria-labelledby="policy-heading" class="panel">
        <h2 id="policy-heading">Policy</h2>
        <div class="fields">
          <label for="verification">Verification</label>
          <select
            id="verification"
            name="verification"
            bind:value={verification}
            aria-describedby="verification-hint"
          >
            <option value="full_sector_readback">Read the whole disc back and compare</option>
            <option value="filesystem_compare">Compare the filesystem</option>
            <option value="tool_verify">Trust the engine's own check</option>
            <option value="none">Do not verify</option>
          </select>
          <p class="hint" id="verification-hint">
            Only reading the disc back establishes that it holds what was intended. A successful
            engine exit does not.
          </p>

          <label for="eject">When the attempt ends</label>
          <select id="eject" name="eject" bind:value={ejectPolicy} aria-describedby="eject-hint">
            <option value="eject_on_success">Eject only a verified disc</option>
            <option value="never">Leave the disc in the drive</option>
            <option value="always">Always eject</option>
          </select>
          <p class="hint" id="eject-hint">
            A disc that failed verification is left where you will find it.
          </p>

          <label for="priority">Priority</label>
          <input
            id="priority"
            name="priority"
            type="number"
            bind:value={priority}
            min="-100"
            max="100"
            aria-describedby="priority-hint"
          />
          <p class="hint" id="priority-hint">Higher is claimed first.</p>
        </div>
      </section>
    </div>

    <section aria-labelledby="review-heading" class="panel review">
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
        <ErrorPanel {problem} area="other" endpoint="POST /api/v1/burn-jobs" level={3} refusal />
      {/if}

      {#if !artifactId.trim() || !discId}
        <p class="hint missing">
          {!artifactId.trim() ? 'Choose the artifact to write' : 'Choose the disc it is a copy of'} to
          queue this burn.
        </p>
      {/if}
      <Button
        type="submit"
        variant="primary"
        icon="burn"
        disabled={submitting || !artifactId.trim() || !discId}
      >
        {submitting ? 'Queueing…' : 'Queue this burn'}
      </Button>
    </section>
  </form>
{/if}

<style>
  .layout {
    display: grid;
    grid-template-columns: minmax(0, 1.6fr) minmax(0, 1fr);
    gap: var(--space-4);
    align-items: start;
  }
  @media (max-width: 1099px) {
    .layout {
      grid-template-columns: minmax(0, 1fr);
    }
  }
  .stack {
    display: grid;
    gap: var(--space-4);
  }
  /* A panel's look, kept on the form's own sections so their headings and
     the form stay one piece of markup. */
  .panel {
    padding: var(--space-3) var(--space-4) var(--space-4);
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--radius-lg);
  }
  .panel h2 {
    margin: 0 0 var(--space-3);
    font-size: var(--text-base);
  }
  .what {
    display: flex;
    gap: var(--space-5);
    align-items: flex-start;
  }
  .fields {
    flex: 1;
    display: grid;
    gap: var(--space-1);
    min-width: 0;
  }
  .fields label {
    margin-top: var(--space-2);
    font-size: var(--text-sm);
  }
  .fields label:first-child {
    margin-top: 0;
  }
  .hint {
    margin: 0;
    font-size: var(--text-sm);
    color: var(--text-muted);
  }
  .facts {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(10rem, 1fr));
    gap: var(--space-2) var(--space-4);
    margin: var(--space-2) 0;
    padding: var(--space-3);
    background: var(--surface-2);
    border-radius: var(--radius);
  }
  dt {
    color: var(--text-muted);
    font-size: var(--text-xs);
  }
  dd {
    margin: 0;
    font-weight: var(--weight-medium);
    overflow-wrap: anywhere;
  }
  .review ul {
    margin: 0 0 var(--space-4);
    padding-left: 1.25rem;
    display: grid;
    gap: var(--space-2);
  }
  .refused {
    color: var(--bad);
  }
  .linked ul {
    list-style: none;
    margin: var(--space-1) 0 0;
    padding: 0;
    display: grid;
    gap: var(--space-1);
  }
  .linked li {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-1) var(--space-3);
  }
  .linked .name {
    font-weight: var(--weight-medium);
    overflow-wrap: anywhere;
  }
  .missing {
    margin-bottom: var(--space-2);
  }
  .queued {
    padding: var(--space-4);
    background: var(--surface);
    border: 1px solid var(--line);
    border-left: 4px solid var(--accent);
    border-radius: var(--radius-lg);
    max-width: 75ch;
  }
  .queued h2 {
    margin: 0 0 var(--space-2);
    font-size: var(--text-md);
  }
  .warnings {
    list-style: none;
    padding: 0;
    display: grid;
    gap: var(--space-1);
  }
  .warnings :global(.mark) {
    white-space: normal;
    font-weight: var(--weight-medium);
  }
</style>
