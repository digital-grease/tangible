<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { page } from '$app/state';
  import {
    attemptOutcomeLabel,
    canCancel,
    canRetry,
    cancelBurnJob,
    isLive,
    jobStateLabel,
    jobStateMark,
    loadAttemptEvents,
    loadBurnJob,
    progressPercent,
    progressSentence,
    resolveAttention,
    retryBurnJob,
    stageLabel,
    type BurnEventView,
    type BurnJobDetail,
  } from '$lib/burns';
  import type { LoadState, Problem } from '$lib/library';

  let view = $state<LoadState<BurnJobDetail>>({ kind: 'loading' });
  let events = $state<BurnEventView[]>([]);
  /** A refused command, kept beside the button that was pressed. */
  let refusal = $state<Problem | null>(null);
  let working = $state(false);
  let timer: ReturnType<typeof setInterval> | undefined;

  /** As on the queue: a poll now, the event stream's fallback later. */
  const RECONCILE_MS = 3_000;

  const id = $derived(page.params.id ?? '');
  const job = $derived(view.kind === 'ready' ? view.data : null);
  const latestAttempt = $derived(job ? job.attempts[job.attempts.length - 1] : undefined);

  async function refresh() {
    const next = await loadBurnJob(id);
    // A failed poll leaves what is on screen alone; a burn in progress is
    // exactly when an operator least wants the page to blank itself.
    if (next.kind === 'error' && view.kind === 'ready') return;
    view = next;

    const attempt = next.kind === 'ready' ? next.data.attempts.at(-1) : undefined;
    if (attempt) {
      const timeline = await loadAttemptEvents(attempt.id);
      if (timeline.kind === 'ready') events = timeline.data.items;
    }

    if (next.kind === 'ready' && !isLive(next.data.state) && timer) {
      // Nothing further will change on its own.
      clearInterval(timer);
      timer = undefined;
    }
  }

  onMount(async () => {
    await refresh();
    timer = setInterval(refresh, RECONCILE_MS);
  });

  onDestroy(() => {
    if (timer) clearInterval(timer);
  });

  async function cancel() {
    working = true;
    refusal = null;
    const result = await cancelBurnJob(id);
    if (result.kind === 'error') {
      // A refusal is an answer, not a failure of the page: the server declined
      // because honouring it would ruin a disc, and it says so.
      refusal = result.problem;
    } else {
      view = result;
    }
    working = false;
  }

  async function resolve() {
    working = true;
    refusal = null;
    const result = await resolveAttention(id);
    if (result.kind === 'error') {
      refusal = result.problem;
    } else {
      view = result;
    }
    working = false;
  }

  async function retry() {
    working = true;
    refusal = null;
    const result = await retryBurnJob(id);
    if (result.kind === 'error') {
      refusal = result.problem;
    } else {
      view = result;
      if (!timer) timer = setInterval(refresh, RECONCILE_MS);
    }
    working = false;
  }
</script>

<svelte:head>
  <title>{job ? `Burn ${job.id.slice(0, 8)} — Tangible` : 'Burn — Tangible'}</title>
</svelte:head>

<p><a href="/burns">← Burn queue</a></p>

<div aria-busy={view.kind === 'loading'}>
  {#if view.kind === 'loading'}
    <p>Loading burn…</p>
  {:else if view.kind === 'error'}
    <h1>Could not load this burn</h1>
    <p>{view.problem.title}.</p>
    <details>
      <summary>Technical detail</summary>
      <p>{view.problem.detail}</p>
      <p>Error code: <code>{view.problem.code}</code></p>
    </details>
  {:else if job}
    <h1>Burn <code>{job.id.slice(0, 8)}</code></h1>

    <!-- The live region is this section alone. Progress changes every few
         seconds, and announcing the whole page each time would be unusable. -->
    <section aria-live="polite" aria-labelledby="status-heading" class="status">
      <h2 id="status-heading">
        <span aria-hidden="true">{jobStateMark(job.state)}</span>
        {jobStateLabel(job.state)}
      </h2>

      <p class="visually-hidden">{progressSentence(job)}</p>

      {#if job.state === 'waiting_for_media'}
        <p class="instruction">
          <strong>Insert a disc.</strong>
          {#if job.requested_media_profile}
            This burn asked for {job.requested_media_profile}.
          {:else}
            Any blank disc the drive can write and the image fits on.
          {/if}
        </p>
      {/if}

      {#if job.progress}
        <p aria-hidden="true">
          {stageLabel(job.progress.stage)}
          {#if progressPercent(job.progress.fraction) !== null}
            — {progressPercent(job.progress.fraction)}%
          {/if}
        </p>
        {#if progressPercent(job.progress.fraction) !== null}
          <!-- A native progress element, so assistive technology reads it
               without any ARIA of ours. -->
          <progress max="100" value={progressPercent(job.progress.fraction)}></progress>
        {/if}
      {/if}

      {#if job.has_started_writing && isLive(job.state)}
        <p class="note">
          A disc is being written. The worker finishes locally even if this page or the network goes
          away, and it will report the outcome when it can.
        </p>
      {/if}
    </section>

    {#if refusal}
      <section class="refusal" aria-live="assertive" aria-labelledby="refusal-heading">
        <h2 id="refusal-heading">{refusal.title}</h2>
        <p>{refusal.detail}</p>
        <p>Error code: <code>{refusal.code}</code></p>
      </section>
    {/if}

    <section aria-labelledby="actions-heading">
      <h2 id="actions-heading">Actions</h2>
      {#if canCancel(job)}
        <!-- Offered only while nothing physical has happened. Once a disc is
             being consumed the control is gone rather than disabled: an
             option that cannot be honoured should not be presented as one. -->
        <button type="button" onclick={cancel} disabled={working}>
          Cancel this burn — no disc has been written yet
        </button>
      {:else if isLive(job.state)}
        <p>
          This burn cannot be cancelled: a disc is being consumed. Stopping a write part-way ruins
          the disc, so the attempt finishes and its outcome is recorded.
        </p>
      {/if}

      {#if canRetry(job)}
        <button type="button" onclick={retry} disabled={working}>
          Retry — queues another attempt and spends another disc
        </button>
      {:else if job.state === 'complete'}
        <p>This burn is finished. Producing another copy is a new burn job.</p>
      {:else if job.state === 'needs_attention'}
        <p>
          This burn needs a person. A disc it may have written is unaccounted for, so no further
          attempt is offered until somebody has looked.
        </p>
        <ol>
          <li>Check the drive and find the disc, if there is one.</li>
          <li>
            Record what you found against it in <a href="/discs">the disc inventory</a>, including
            destroying it if it cannot be trusted.
          </li>
          <li>Then release this burn, which makes it retryable.</li>
        </ol>
        <button type="button" onclick={resolve} disabled={working}>
          I have accounted for the disc
        </button>
      {/if}
    </section>

    <section aria-labelledby="details-heading">
      <h2 id="details-heading">Details</h2>
      <dl>
        <dt>Artifact</dt>
        <dd><a href={`/library/${job.artifact_id}`}><code>{job.artifact_id}</code></a></dd>
        <dt>Disc</dt>
        <dd><code>{job.disc_id}</code></dd>
        <dt>Verification</dt>
        <dd>{job.verification_policy.join(', ').replace(/_/g, ' ')}</dd>
        <dt>Eject policy</dt>
        <dd>{job.eject_policy.replace(/_/g, ' ')}</dd>
        <dt>Media requested</dt>
        <dd>{job.requested_media_profile ?? 'Any the drive can write'}</dd>
        <dt>Priority</dt>
        <dd>{job.priority}</dd>
        <dt>Queued</dt>
        <dd>{new Date(job.created_at).toLocaleString()}</dd>
        {#if job.completed_at}
          <dt>Finished</dt>
          <dd>{new Date(job.completed_at).toLocaleString()}</dd>
        {/if}
      </dl>
      <p class="note">
        Target compatibility is unknown. A verified disc holds the same bytes, which is not a
        promise that any particular player, console or drive will accept it.
      </p>
    </section>

    <section aria-labelledby="attempts-heading">
      <h2 id="attempts-heading">Attempts</h2>
      {#if job.attempts.length === 0}
        <p>No worker has taken this burn yet.</p>
      {:else}
        <p>
          {job.attempts.filter((attempt) => attempt.consumed_media).length} of {job.attempts.length}
          attempt{job.attempts.length === 1 ? '' : 's'} consumed a disc.
        </p>
        <table>
          <caption class="visually-hidden">Attempts made at this burn</caption>
          <thead>
            <tr>
              <th scope="col">#</th>
              <th scope="col">Outcome</th>
              <th scope="col">Disc consumed</th>
              <th scope="col">Engine</th>
              <th scope="col">Started</th>
            </tr>
          </thead>
          <tbody>
            {#each job.attempts as attempt (attempt.id)}
              <tr>
                <th scope="row">{attempt.attempt_number}</th>
                <td>
                  {attemptOutcomeLabel(attempt.state)}
                  {#if attempt.error_code}
                    <br /><small><code>{attempt.error_code}</code></small>
                  {/if}
                </td>
                <td>
                  {#if attempt.consumed_media}
                    {#if attempt.physical_copy_id}
                      <a href={`/discs/${attempt.physical_copy_id}`}>Yes — the disc</a>
                    {:else}
                      Yes
                    {/if}
                  {:else}
                    No
                  {/if}
                </td>
                <td>{attempt.engine} {attempt.engine_version}</td>
                <td>{new Date(attempt.started_at).toLocaleString()}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
    </section>

    {#if latestAttempt}
      <section aria-labelledby="timeline-heading">
        <h2 id="timeline-heading">Latest attempt timeline</h2>
        {#if events.length === 0}
          <p>The worker has not reported anything yet.</p>
        {:else}
          <ol class="timeline">
            {#each events as event (event.sequence)}
              <li>
                <strong>{stageLabel(event.stage)}</strong>
                <code>{event.code}</code>
                {#if progressPercent(event.fraction) !== null}
                  — {progressPercent(event.fraction)}%
                {/if}
                <small>{new Date(event.observed_at).toLocaleTimeString()}</small>
              </li>
            {/each}
          </ol>
        {/if}
      </section>
    {/if}
  {/if}
</div>

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
  table {
    border-collapse: collapse;
    width: 100%;
  }
  table th,
  table td {
    text-align: left;
    padding: 0.5rem;
    border-bottom: 1px solid currentColor;
  }
  .status {
    border: 1px solid currentColor;
    padding: 0.75rem 1rem;
  }
  .refusal {
    border: 2px solid currentColor;
    padding: 0.75rem 1rem;
  }
  .instruction {
    font-size: 1.1em;
  }
  .note {
    font-size: 0.9em;
  }
  progress {
    width: 100%;
  }
  button {
    display: block;
    margin: 0.5rem 0;
    padding: 0.5rem 1rem;
  }
  .timeline {
    padding-left: 1.25rem;
  }
  .timeline li {
    margin-bottom: 0.25rem;
  }
  .visually-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
  }
</style>
