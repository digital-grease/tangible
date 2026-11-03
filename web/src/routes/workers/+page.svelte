<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import {
    anyOpen,
    cancelErasure,
    changeWorker,
    credentialLabel,
    ENROLLMENT_MINUTES,
    connectionLabel,
    driveModel,
    erasureStateLabel,
    issueEnrollment,
    loadDrives,
    loadErasures,
    loadWorkers,
    requestErasure,
    workerActions,
    workerStatusLabel,
    type DriveView,
    type ErasureMode,
    type ErasureView,
    type IssuedEnrollment,
    type WorkerAction,
    type WorkerView,
  } from '$lib/workers';
  import type { Problem } from '$lib/session';
  import { watch } from '$lib/events';

  type Load<T> =
    { kind: 'loading' } | { kind: 'ready'; data: T } | { kind: 'error'; problem: Problem };

  let workers = $state<Load<{ items: WorkerView[]; more: boolean }>>({ kind: 'loading' });
  let drives = $state<Load<DriveView[]>>({ kind: 'loading' });

  // A change asked of a worker, and the one being confirmed before a revoke.
  let changing = $state<string | null>(null);
  let revoking = $state<WorkerView | null>(null);
  let workerProblem = $state<Problem | null>(null);
  let erasures = $state<Load<ErasureView[]>>({ kind: 'loading' });

  // The erase form, for one drive at a time.
  let erasingDrive = $state<DriveView | null>(null);
  let eraseMode = $state<ErasureMode>('quick');
  let eraseConfirmed = $state(false);
  let eraseProblem = $state<Problem | null>(null);
  let eraseSubmitting = $state(false);

  let timer: ReturnType<typeof setTimeout> | undefined;
  let stopWatching: (() => void) | undefined;

  async function refresh() {
    const [workerResult, driveResult, erasureResult] = await Promise.all([
      loadWorkers(),
      loadDrives(),
      loadErasures(),
    ]);
    workers =
      workerResult.kind === 'ok'
        ? {
            kind: 'ready',
            data: { items: workerResult.data.items, more: workerResult.data.next_cursor != null },
          }
        : { kind: 'error', problem: workerResult.problem };
    drives =
      driveResult.kind === 'ok'
        ? { kind: 'ready', data: driveResult.data.items }
        : { kind: 'error', problem: driveResult.problem };
    erasures =
      erasureResult.kind === 'ok'
        ? { kind: 'ready', data: erasureResult.data.items }
        : { kind: 'error', problem: erasureResult.problem };
    // Where the browser has no event stream, checked again only while an
    // erasure is still going. With one, the page refreshes as workers,
    // drives and erasures change, and reconciles every minute.
    clearTimeout(timer);
    if (!streaming && erasures.kind === 'ready' && anyOpen(erasures.data)) {
      timer = setTimeout(refresh, 3000);
    }
  }

  /** Whether the event stream drives this page. */
  const streaming = typeof EventSource !== 'undefined';

  onMount(() => {
    void refresh();
    if (streaming) stopWatching = watch(['workers', 'erasures'], refresh);
  });
  onDestroy(() => {
    clearTimeout(timer);
    stopWatching?.();
  });

  async function change(worker: WorkerView, action: WorkerAction) {
    if (changing) return;
    changing = worker.id;
    workerProblem = null;
    const result = await changeWorker(worker.id, action);
    changing = null;
    if (result.kind === 'error') {
      workerProblem = result.problem;
      return;
    }
    revoking = null;
    await refresh();
  }

  function startErase(drive: DriveView) {
    erasingDrive = drive;
    eraseMode = 'quick';
    eraseConfirmed = false;
    eraseProblem = null;
  }

  async function erase(event: SubmitEvent) {
    event.preventDefault();
    if (!erasingDrive || eraseSubmitting) return;
    eraseSubmitting = true;
    eraseProblem = null;
    const result = await requestErasure(erasingDrive.id, eraseMode, eraseConfirmed);
    eraseSubmitting = false;
    if (result.kind === 'error') {
      eraseProblem = result.problem;
      return;
    }
    erasingDrive = null;
    await refresh();
  }

  async function withdraw(erasure: ErasureView) {
    const result = await cancelErasure(erasure.id);
    if (result.kind === 'error') {
      eraseProblem = result.problem;
    }
    await refresh();
  }

  function when(value: string | null | undefined): string {
    return value ? new Date(value).toLocaleString() : '';
  }

  let minutes = $state<number>(ENROLLMENT_MINUTES.default);
  let issued = $state<IssuedEnrollment | null>(null);
  let problem = $state<Problem | null>(null);
  let submitting = $state(false);

  async function issue(event: SubmitEvent) {
    event.preventDefault();
    if (submitting) return;
    submitting = true;
    problem = null;
    issued = null;
    const result = await issueEnrollment(minutes);
    submitting = false;
    if (result.kind === 'error') {
      problem = result.problem;
      return;
    }
    issued = result.data;
  }
</script>

<svelte:head><title>Workers — Tangible</title></svelte:head>

<h1>Workers</h1>

<section aria-labelledby="workers-heading" aria-busy={workers.kind === 'loading'}>
  <h2 id="workers-heading">Workers</h2>
  {#if workers.kind === 'loading'}
    <p>Loading workers…</p>
  {:else if workers.kind === 'error'}
    <div role="alert"><p>{workers.problem.detail}</p></div>
  {:else if workers.data.items.length === 0}
    <p>No workers yet. Enroll one below.</p>
  {:else}
    <table>
      <caption>Every enrolled worker, oldest first</caption>
      <thead>
        <tr>
          <th scope="col">Worker</th>
          <th scope="col">Status</th>
          <th scope="col">Version</th>
          <th scope="col">Last seen</th>
          <th scope="col">Credential</th>
          <th scope="col">Drives</th>
          <th scope="col"><span class="visually-hidden">Actions</span></th>
        </tr>
      </thead>
      <tbody>
        {#each workers.data.items as worker (worker.id)}
          <tr>
            <th scope="row">{worker.name}</th>
            <td>
              {workerStatusLabel(worker.status)}
              {#if worker.revoked_at}<br /><small>since {when(worker.revoked_at)}</small>{/if}
            </td>
            <td>{worker.software_version}</td>
            <td>{when(worker.last_seen_at) || 'Never'}</td>
            <td>{credentialLabel(worker)}</td>
            <td>{worker.drives.map((drive) => drive.name).join(', ') || 'None'}</td>
            <td>
              {#each workerActions(worker.status, worker.credential_rotation_requested_at != null) as action (action)}
                {#if action === 'revoke'}
                  <button type="button" onclick={() => (revoking = worker)}>
                    Revoke {worker.name}…
                  </button>
                {:else}
                  <button
                    type="button"
                    disabled={changing === worker.id}
                    onclick={() => change(worker, action)}
                  >
                    {action === 'drain'
                      ? `Drain ${worker.name}`
                      : action === 'enable'
                        ? `Enable ${worker.name}`
                        : `Rotate ${worker.name}'s credential`}
                  </button>
                {/if}
              {/each}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
    {#if workers.data.more}
      <p><small>Showing the first 200 workers.</small></p>
    {/if}
    <p>
      <small
        >Draining a worker lets it finish what it holds and gives it nothing new, for maintenance.
        Enabling it undoes that. Rotating its credential has the worker swap it for a new one
        between jobs; the old one works until then, so nothing is interrupted. If the credential may
        be known to somebody else, revoke the worker instead.</small
      >
    </p>
  {/if}

  <div role="alert">
    {#if workerProblem}
      <p>
        <strong>Not changed.</strong>
        {workerProblem.code === 'FORBIDDEN'
          ? 'Only an administrator can change workers.'
          : workerProblem.detail}
      </p>
    {/if}
  </div>

  {#if revoking}
    {@const worker = revoking}
    <form
      class="danger"
      aria-labelledby="revoke-heading"
      onsubmit={(event) => {
        event.preventDefault();
        void change(worker, 'revoke');
      }}
    >
      <h3 id="revoke-heading">Revoke {worker.name}</h3>
      <p>
        <strong>Its credential is refused from its next request, for good.</strong> It can no longer take
        work, renew a lease or report. A burn it is writing is not interrupted by the server; its lease
        lapses as a lost worker's does. To use the machine again, issue a new enrollment token and enroll
        it under a new name.
      </p>
      <button type="submit" disabled={changing === worker.id}>
        {changing === worker.id ? 'Revoking…' : `Revoke ${worker.name}`}
      </button>
      <button type="button" onclick={() => (revoking = null)}>Keep {worker.name}</button>
    </form>
  {/if}
</section>

<section aria-labelledby="drives-heading" aria-busy={drives.kind === 'loading'}>
  <h2 id="drives-heading">Drives</h2>
  {#if drives.kind === 'loading'}
    <p>Loading drives…</p>
  {:else if drives.kind === 'error'}
    <div role="alert"><p>{drives.problem.detail}</p></div>
  {:else if drives.data.length === 0}
    <p>No drives yet. A burn worker reports its drive when it first starts.</p>
  {:else}
    <table>
      <caption>Every drive a worker has reported</caption>
      <thead>
        <tr>
          <th scope="col">Drive</th>
          <th scope="col">Worker</th>
          <th scope="col">Model</th>
          <th scope="col">Connection</th>
          <th scope="col">Worker last seen</th>
          <th scope="col"><span class="visually-hidden">Actions</span></th>
        </tr>
      </thead>
      <tbody>
        {#each drives.data as drive (drive.id)}
          <tr>
            <th scope="row">{drive.name} <small>({drive.device})</small></th>
            <td>{drive.worker_name} <small>({drive.worker_status})</small></td>
            <td>{driveModel(drive)}</td>
            <td>{connectionLabel(drive.connection)}</td>
            <td>{when(drive.worker_last_seen_at) || 'Never'}</td>
            <td>
              <button type="button" onclick={() => startErase(drive)}>
                Erase the disc in {drive.name}…
              </button>
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}

  {#if erasingDrive}
    <form class="danger" onsubmit={erase} aria-labelledby="erase-heading">
      <h3 id="erase-heading">Erase the disc in {erasingDrive.name}</h3>
      <p>
        <strong>This destroys everything on the disc in that drive.</strong> It cannot be undone. Only
        a rewritable disc (CD-RW, DVD-RW, DVD+RW, BD-RE) is erased; the worker looks first and leaves
        a blank or write-once disc alone, and says so.
      </p>
      <fieldset>
        <legend>How thoroughly</legend>
        <label>
          <input type="radio" name="mode" value="quick" bind:group={eraseMode} />
          Quick: only what makes the disc writable again. Fast on a CD-RW; some drives take half an hour
          on a DVD-RW whichever you choose
        </label>
        <br />
        <label>
          <input type="radio" name="mode" value="full" bind:group={eraseMode} />
          Full: writes over the whole disc, which can take an hour
        </label>
      </fieldset>
      <p>
        <label>
          <input type="checkbox" bind:checked={eraseConfirmed} />
          I understand everything on the disc in {erasingDrive.name} will be destroyed
        </label>
      </p>
      <div role="alert">
        {#if eraseProblem}
          <p>
            <strong>Not erased.</strong>
            {eraseProblem.code === 'FORBIDDEN'
              ? 'Only an administrator can erase discs.'
              : eraseProblem.detail}
          </p>
        {/if}
      </div>
      <button type="submit" disabled={!eraseConfirmed || eraseSubmitting}>
        {eraseSubmitting ? 'Asking…' : `Erase the disc in ${erasingDrive.name}`}
      </button>
      <button type="button" onclick={() => (erasingDrive = null)}>Keep the disc</button>
    </form>
  {/if}
</section>

<section aria-labelledby="erasures-heading">
  <h2 id="erasures-heading">Recent erasures</h2>
  <div aria-live="polite">
    {#if erasures.kind === 'loading'}
      <p>Loading erasures…</p>
    {:else if erasures.kind === 'error'}
      <p>{erasures.problem.detail}</p>
    {:else if erasures.data.length === 0}
      <p>No disc has been erased.</p>
    {:else}
      <table>
        <caption>Newest first</caption>
        <thead>
          <tr>
            <th scope="col">Asked for</th>
            <th scope="col">Drive</th>
            <th scope="col">How</th>
            <th scope="col">State</th>
            <th scope="col">What was found</th>
            <th scope="col"><span class="visually-hidden">Actions</span></th>
          </tr>
        </thead>
        <tbody>
          {#each erasures.data as erasure (erasure.id)}
            <tr>
              <td>{when(erasure.created_at)} by {erasure.requested_by}</td>
              <td>{erasure.drive_name}</td>
              <td>{erasure.mode === 'full' ? 'Full' : 'Quick'}</td>
              <td>
                {erasureStateLabel(erasure.state)}
                {#if erasure.error_detail}<br /><small>{erasure.error_detail}</small>{/if}
              </td>
              <td>
                {#if erasure.medium_before}
                  {erasure.medium_before.profile}, {erasure.medium_before.blank
                    ? 'blank'
                    : `${erasure.medium_before.sessions} session(s)`}
                {/if}
              </td>
              <td>
                {#if erasure.state === 'queued'}
                  <button type="button" onclick={() => withdraw(erasure)}>Withdraw</button>
                {/if}
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}
  </div>
</section>

<section aria-labelledby="enroll-heading">
  <h2 id="enroll-heading">Enroll a burn worker</h2>
  <p>
    A new worker needs a one-use enrollment token on its first start. It exchanges the token for a
    credential of its own, kept on the worker's volume, and the token is spent.
  </p>

  <form onsubmit={issue}>
    <p>
      <label for="minutes">Valid for (minutes)</label><br />
      <input
        id="minutes"
        name="minutes"
        type="number"
        min={ENROLLMENT_MINUTES.min}
        max={ENROLLMENT_MINUTES.max}
        bind:value={minutes}
        required
      />
      <br />
      <small>Long enough to paste it into the worker's settings and start it.</small>
    </p>

    <div role="alert">
      {#if problem}
        <p>
          <strong>Not issued.</strong>
          {problem.code === 'FORBIDDEN'
            ? 'Only an administrator can enroll workers.'
            : problem.detail}
        </p>
      {/if}
    </div>

    <button type="submit" disabled={submitting}>
      {submitting ? 'Issuing…' : 'Issue an enrollment token'}
    </button>
  </form>

  {#if issued}
    <div aria-live="polite" class="issued">
      <h3>Enrollment token</h3>
      <p>
        Shown this once. The server keeps only its hash, so if it is lost, issue another. It lapses
        at {new Date(issued.expires_at).toLocaleString()}.
      </p>
      <p>
        <label for="token">Token</label><br />
        <input id="token" readonly value={issued.enrollment_token} size="80" />
      </p>
      <p>
        Set it as <code>TANGIBLE_ENROLLMENT_TOKEN</code> in the worker's environment, start the worker,
        then remove the setting again.
      </p>
    </div>
  {/if}
</section>

<style>
  .danger {
    border: 3px solid currentColor;
    padding: 0 1rem 1rem;
    margin-top: 1rem;
  }
  .visually-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }
  .issued {
    border: 2px solid currentColor;
    padding: 0 1rem;
    margin-top: 1rem;
  }
  #token {
    font-family: ui-monospace, monospace;
    max-width: 100%;
  }
</style>
