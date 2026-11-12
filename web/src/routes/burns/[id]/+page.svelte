<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { shortId } from '$lib/ids';
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import Disc from '$lib/components/ui/Disc.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import Panel from '$lib/components/ui/Panel.svelte';
  import Progress from '$lib/components/ui/Progress.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import Table from '$lib/components/ui/Table.svelte';
  import type { DiscShape } from '$lib/disc';
  import { onDestroy, onMount } from 'svelte';
  import { page } from '$app/state';
  import {
    attemptOutcomeLabel,
    attemptOutcomeTone,
    burnDrawing,
    canCancel,
    canRetry,
    cancelBurnJob,
    eventMessage,
    isLive,
    jobStateLabel,
    jobStateTone,
    loadAttempt,
    loadAttemptEvents,
    loadBurnJob,
    mediumLabel,
    progressPercent,
    progressSentence,
    resolveAttention,
    retryBurnJob,
    stageLabel,
    type BurnAttemptDetail,
    type BurnEventView,
    type BurnJobDetail,
  } from '$lib/burns';
  import BurnReport from '$lib/components/BurnReport.svelte';
  import { names, watch } from '$lib/events';
  import { loadArtifact, type LoadState, type Problem } from '$lib/library';
  import { loadReportSettings, type ReportSettings } from '$lib/report';

  let view = $state<LoadState<BurnJobDetail>>({ kind: 'loading' });
  let events = $state<BurnEventView[]>([]);
  /** The latest attempt in full: what preflight found in the tray. */
  let latestDetail = $state<BurnAttemptDetail | null>(null);
  /** The disc the burn's artifact describes, for the drawing; read once. */
  let shape = $state<DiscShape | null>(null);
  let shapeAsked = false;
  /** Whether this server offers issue reports; null until known, or if not. */
  let reportSettings = $state<ReportSettings | null>(null);
  /** A refused command, kept beside the button that was pressed. */
  let refusal = $state<Problem | null>(null);
  let working = $state(false);
  let stopWatching: (() => void) | undefined;

  /** How often a live burn polls where the browser has no event stream. */
  const FALLBACK_MS = 3_000;

  /** Refresh as this burn changes, while it is live. */
  function startWatching() {
    stopWatching ??= watch(['burns'], refresh, {
      fallbackMs: FALLBACK_MS,
      relevant: (event) => names(event, id, 'burn_job_id'),
    });
  }

  function stopWatchingNow() {
    stopWatching?.();
    stopWatching = undefined;
  }

  const id = $derived(page.params.id ?? '');
  const job = $derived(view.kind === 'ready' ? view.data : null);
  const latestAttempt = $derived(job ? job.attempts[job.attempts.length - 1] : undefined);

  async function refresh() {
    const next = await loadBurnJob(id);
    // A failed poll leaves what is on screen alone; a burn in progress is
    // exactly when an operator least wants the page to blank itself.
    if (next.kind === 'error' && view.kind === 'ready') return;
    view = next;

    if (next.kind === 'ready' && !shapeAsked) {
      // What the disc will hold does not change, so it is read once; a burn
      // whose artifact cannot be read is drawn without its layout.
      shapeAsked = true;
      void loadArtifact(next.data.artifact_id).then((artifact) => {
        if (artifact.kind === 'ready') shape = artifact.data.disc_shape;
      });
    }

    const attempt = next.kind === 'ready' ? next.data.attempts.at(-1) : undefined;
    if (attempt) {
      const [timeline, detail] = await Promise.all([
        loadAttemptEvents(attempt.id),
        loadAttempt(attempt.id),
      ]);
      if (timeline.kind === 'ready') events = timeline.data.items;
      if (detail.kind === 'ready') latestDetail = detail.data;
    }

    if (next.kind === 'ready' && !isLive(next.data.state)) {
      // Nothing further will change on its own.
      stopWatchingNow();
    }
  }

  onMount(async () => {
    await refresh();
    if (view.kind !== 'ready' || isLive(view.data.state)) startWatching();
    // Once: whether to offer reports does not change while a page is open,
    // and a server that cannot say simply offers none.
    const settings = await loadReportSettings();
    if (settings.kind === 'ok') reportSettings = settings.data;
  });

  onDestroy(stopWatchingNow);

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
      startWatching();
    }
    working = false;
  }
</script>

<svelte:head>
  <title>{job ? `Burn ${shortId(job.id)} — Tangible` : 'Burn — Tangible'}</title>
</svelte:head>

<div aria-busy={view.kind === 'loading'}>
  {#if view.kind === 'loading'}
    <p class="quiet">Loading burn…</p>
  {:else if view.kind === 'error'}
    <p><a href="/burns">Burn queue</a></p>
    <ErrorPanel
      problem={view.problem}
      area="other"
      endpoint="GET /api/v1/burn-jobs/{'{id}'}"
      heading="Could not load this burn"
      level={1}
    />
  {:else if job}
    {@const drawing = burnDrawing(job)}
    {@const percent = job.progress ? progressPercent(job.progress.fraction) : null}
    <PageHeader
      title={`Burn ${shortId(job.id)}`}
      crumbs={[{ label: 'Burn queue', href: '/burns' }, { label: shortId(job.id) }]}
    />

    <!-- The live region is this section alone. Progress changes every few
         seconds, and announcing the whole page each time would be unusable. -->
    <section aria-live="polite" aria-labelledby="status-heading" class="status">
      <Disc
        mode={drawing.mode}
        {shape}
        capacity={latestDetail?.medium?.capacity_blocks ?? null}
        progress={drawing.progress}
        size={136}
        hole="var(--surface)"
      />
      <div class="state">
        <h2 id="status-heading">
          <StatusMark tone={jobStateTone(job.state)} label={jobStateLabel(job.state)} />
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
          <p aria-hidden="true" class="stage">
            {stageLabel(job.progress.stage)}{percent !== null ? `, ${percent}%` : ''}
          </p>
          {#if percent !== null}
            <Progress value={percent / 100} label={stageLabel(job.progress.stage)} tone="laser" />
          {/if}
        {/if}

        {#if job.has_started_writing && isLive(job.state)}
          <p class="quiet small">
            A disc is being written. The worker finishes locally even if this page or the network
            goes away, and it will report the outcome when it can.
          </p>
        {/if}
      </div>
    </section>

    {#if refusal}
      <ErrorPanel problem={refusal} area="other" refusal />
    {/if}

    <div class="columns">
      <div class="stack">
        <Panel title="Attempts" aside={String(job.attempts.length)} flush>
          {#if job.attempts.length === 0}
            <p class="quiet padded">No worker has taken this burn yet.</p>
          {:else}
            <p class="quiet padded">
              {job.attempts.filter((attempt) => attempt.consumed_media).length} of {job.attempts
                .length}
              attempt{job.attempts.length === 1 ? '' : 's'} consumed a disc.
            </p>
            <Table caption="Attempts made at this burn">
              <thead>
                <tr>
                  <th scope="col" class="num">#</th>
                  <th scope="col">Outcome</th>
                  <th scope="col">Disc consumed</th>
                  <th scope="col">Engine</th>
                  <th scope="col">Started</th>
                </tr>
              </thead>
              <tbody>
                {#each job.attempts as attempt (attempt.id)}
                  <tr>
                    <th scope="row" class="num">{attempt.attempt_number}</th>
                    <td>
                      <div class="outcome">
                        <StatusMark
                          tone={attemptOutcomeTone(attempt.state)}
                          label={attemptOutcomeLabel(attempt.state)}
                          pill
                        />
                        {#if attempt.error_code}
                          <small><code>{attempt.error_code}</code></small>
                        {/if}
                        {#if attempt.error_detail}
                          <details>
                            <summary>What the worker said</summary>
                            <pre>{attempt.error_detail}</pre>
                          </details>
                        {/if}
                      </div>
                    </td>
                    <td>
                      {#if attempt.consumed_media}
                        {#if attempt.physical_copy_id}
                          <a href={`/discs/${attempt.physical_copy_id}`}>Yes: the disc</a>
                        {:else}
                          Yes
                        {/if}
                      {:else}
                        No
                      {/if}
                    </td>
                    <td>{attempt.engine} {attempt.engine_version}</td>
                    <td>
                      <time datetime={attempt.started_at}
                        >{new Date(attempt.started_at).toLocaleString()}</time
                      >
                    </td>
                  </tr>
                {/each}
              </tbody>
            </Table>
          {/if}
        </Panel>

        {#if latestAttempt}
          <Panel title="Latest attempt timeline">
            {#if events.length === 0}
              <p class="quiet">The worker has not reported anything yet.</p>
            {:else}
              <ol class="timeline">
                {#each events as event (event.sequence)}
                  <li>
                    <time datetime={event.observed_at}
                      >{new Date(event.observed_at).toLocaleTimeString()}</time
                    >
                    <span>
                      <strong>{stageLabel(event.stage)}</strong>
                      <code>{event.code}</code>
                      {#if progressPercent(event.fraction) !== null}
                        {progressPercent(event.fraction)}%
                      {/if}
                    </span>
                    {#if eventMessage(event)}
                      <span class="message">{eventMessage(event)}</span>
                    {/if}
                  </li>
                {/each}
              </ol>
            {/if}
          </Panel>

          <BurnReport attempt={latestAttempt} settings={reportSettings} />
        {/if}
      </div>

      <div class="stack">
        <Panel title="Actions">
          {#if canCancel(job)}
            <!-- Offered only while nothing physical has happened. Once a disc
                 is being consumed the control is gone rather than disabled: an
                 option that cannot be honoured should not be presented as one. -->
            <Button onclick={cancel} disabled={working}>
              Cancel this burn: no disc has been written yet
            </Button>
          {:else if isLive(job.state)}
            <p>
              This burn cannot be cancelled: a disc is being consumed. Stopping a write part-way
              ruins the disc, so the attempt finishes and its outcome is recorded.
            </p>
          {/if}

          {#if canRetry(job)}
            <Button onclick={retry} disabled={working}>
              Retry: queues another attempt and spends another disc
            </Button>
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
                Record what you found against it in <a href="/discs">the disc inventory</a>,
                including destroying it if it cannot be trusted.
              </li>
              <li>Then release this burn, which makes it retryable.</li>
            </ol>
            <Button variant="primary" onclick={resolve} disabled={working}>
              I have accounted for the disc
            </Button>
          {:else if !canCancel(job) && !isLive(job.state)}
            <p class="quiet">Nothing to do here.</p>
          {/if}
        </Panel>

        <Panel title="Details">
          <dl class="facts">
            <div>
              <dt>Identifier</dt>
              <dd><code>{job.id}</code></dd>
            </div>
            <div>
              <dt>Artifact</dt>
              <dd><a href={`/library/${job.artifact_id}`}><code>{job.artifact_id}</code></a></dd>
            </div>
            <div>
              <dt>Disc</dt>
              <dd><code>{job.disc_id}</code></dd>
            </div>
            <div>
              <dt>Verification</dt>
              <dd>{job.verification_policy.join(', ').replace(/_/g, ' ')}</dd>
            </div>
            <div>
              <dt>Eject policy</dt>
              <dd>{job.eject_policy.replace(/_/g, ' ')}</dd>
            </div>
            <div>
              <dt>Media requested</dt>
              <dd>{job.requested_media_profile ?? 'Any the drive can write'}</dd>
            </div>
            {#if latestDetail?.medium}
              <div>
                <dt>Disc found</dt>
                <dd>{mediumLabel(latestDetail.medium)}</dd>
              </div>
            {/if}
            <div>
              <dt>Priority</dt>
              <dd class="num">{job.priority}</dd>
            </div>
            <div>
              <dt>Queued</dt>
              <dd>
                <time datetime={job.created_at}>{new Date(job.created_at).toLocaleString()}</time>
              </dd>
            </div>
            {#if job.completed_at}
              <div>
                <dt>Finished</dt>
                <dd>
                  <time datetime={job.completed_at}
                    >{new Date(job.completed_at).toLocaleString()}</time
                  >
                </dd>
              </div>
            {/if}
          </dl>
          <p class="quiet small note">
            <StatusMark tone="idle" label="Target compatibility unknown" /> A verified disc holds the
            same bytes, which is not a promise that any particular player, console or drive will accept
            it.
          </p>
        </Panel>
      </div>
    </div>
  {/if}
</div>

<style>
  .status {
    display: flex;
    gap: var(--space-5);
    align-items: center;
    padding: var(--space-4);
    margin-bottom: var(--space-4);
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--radius-lg);
  }
  .state {
    flex: 1;
    display: grid;
    gap: var(--space-2);
    min-width: 0;
  }
  .state h2 {
    margin: 0;
    font-size: var(--text-lg);
  }
  .state p {
    margin: 0;
  }
  .instruction {
    font-size: var(--text-md);
  }
  .stage {
    color: var(--text-muted);
  }
  .columns {
    display: grid;
    grid-template-columns: minmax(0, 1.5fr) minmax(0, 1fr);
    gap: var(--space-4);
    align-items: start;
  }
  @media (max-width: 1099px) {
    .columns {
      grid-template-columns: minmax(0, 1fr);
    }
  }
  @media (max-width: 599px) {
    .status {
      flex-direction: column;
      align-items: flex-start;
    }
  }
  .stack {
    display: grid;
    gap: var(--space-4);
    min-width: 0;
  }
  .stack :global(.panel .body > button + button) {
    margin-top: var(--space-2);
  }
  .outcome {
    display: grid;
    gap: var(--space-1);
    justify-items: start;
  }
  .facts {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(11rem, 1fr));
    gap: var(--space-3) var(--space-4);
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
  .note {
    margin: var(--space-3) 0 0;
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
  pre {
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    margin: var(--space-1) 0 0;
  }
  .padded {
    padding: var(--space-3) var(--space-4) 0;
  }
  .small {
    font-size: var(--text-sm);
  }
  .quiet {
    color: var(--text-muted);
  }
</style>
