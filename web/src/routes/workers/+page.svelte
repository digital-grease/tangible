<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Workers and drives (UX.md 11; docs/plans/ui-overhaul.md, "Pages"): every
  // enrolled burn worker with what can be done to it, every drive a worker
  // reports with what it is doing now and a drawing of the disc in it, the
  // recent erasures, and a form that issues an enrollment token. Revoking a
  // worker and erasing a disc each ask first, in words that say what happens.
  import Confirm from '$lib/components/ui/Confirm.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import Disc from '$lib/components/ui/Disc.svelte';
  import EmptyState from '$lib/components/ui/EmptyState.svelte';
  import Field from '$lib/components/ui/Field.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import Panel from '$lib/components/ui/Panel.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import Table from '$lib/components/ui/Table.svelte';
  import { bayEntry } from '$lib/drivebay';
  import { watch } from '$lib/events';
  import { when } from '$lib/overview';
  import type { Problem } from '$lib/session';
  import {
    anyOpen,
    cancelErasure,
    changeWorker,
    connectionLabel,
    credentialLabel,
    credentialTone,
    driveModel,
    ENROLLMENT_MINUTES,
    erasureStateLabel,
    erasureStateTone,
    issueEnrollment,
    loadDrives,
    loadErasures,
    loadWorkers,
    requestErasure,
    workerActions,
    workerStatusLabel,
    workerStatusTone,
    type DriveView,
    type ErasureMode,
    type ErasureView,
    type IssuedEnrollment,
    type WorkerAction,
    type WorkerView,
  } from '$lib/workers';
  import { onDestroy, onMount } from 'svelte';

  type Load<T> =
    { kind: 'loading' } | { kind: 'ready'; data: T } | { kind: 'error'; problem: Problem };

  let workers = $state<Load<{ items: WorkerView[]; more: boolean }>>({ kind: 'loading' });
  let drives = $state<Load<DriveView[]>>({ kind: 'loading' });
  let erasures = $state<Load<ErasureView[]>>({ kind: 'loading' });

  // A change asked of a worker, and the one being confirmed before a revoke.
  let changing = $state<string | null>(null);
  let revoking = $state<WorkerView | null>(null);
  let workerProblem = $state<Problem | null>(null);

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

  // A confirmation opens below its table and takes the keyboard to its
  // question (Confirm.svelte).
  function startRevoke(worker: WorkerView) {
    revoking = worker;
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

  /** The full date and time, for a time's tooltip. */
  function full(value: string): string {
    return new Date(value).toLocaleString();
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

<svelte:head><title>Workers and drives — Tangible</title></svelte:head>

<PageHeader
  title="Workers and drives"
  description="The burn workers enrolled here, the drives they report, and the discs they have erased."
>
  {#snippet actions()}
    <Button icon="plus" href="#enroll">Enroll a worker</Button>
  {/snippet}
</PageHeader>

<div class="stack">
  <div aria-busy={workers.kind === 'loading'}>
    <Panel
      title="Workers"
      aside={workers.kind === 'ready' ? `${workers.data.items.length}` : undefined}
      flush
    >
      {#if workers.kind === 'loading'}
        <p class="padded quiet">Loading workers…</p>
      {:else if workers.kind === 'error'}
        <div class="padded" role="alert">
          <StatusMark tone="bad" label={workers.problem.detail} />
        </div>
      {:else if workers.data.items.length === 0}
        <EmptyState inset level={3} title="No workers yet" icon="worker">
          <p>A burn worker needs an enrollment token on its first start.</p>
          {#snippet action()}
            <Button variant="primary" href="#enroll" icon="plus">Enroll one</Button>
          {/snippet}
        </EmptyState>
      {:else}
        <Table caption="Every enrolled worker, oldest first">
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
              {@const actions = workerActions(
                worker.status,
                worker.credential_rotation_requested_at != null,
              )}
              {@const credentialMark = credentialTone(worker)}
              <tr>
                <th scope="row">{worker.name}</th>
                <td>
                  <div class="cell">
                    <StatusMark
                      tone={workerStatusTone(worker.status)}
                      label={workerStatusLabel(worker.status)}
                    />
                    {#if worker.revoked_at}
                      <span class="small quiet">
                        since <time datetime={worker.revoked_at}>{full(worker.revoked_at)}</time>
                      </span>
                    {/if}
                  </div>
                </td>
                <td><code>{worker.software_version}</code></td>
                <td>
                  {#if worker.last_seen_at}
                    <time datetime={worker.last_seen_at} title={full(worker.last_seen_at)}
                      >{when(worker.last_seen_at)}</time
                    >
                  {:else}
                    <span class="quiet">Never</span>
                  {/if}
                </td>
                <td>
                  <div class="inline">
                    {#if credentialMark}
                      <StatusMark tone={credentialMark} label={credentialLabel(worker)} />
                    {:else}
                      <span>{credentialLabel(worker)}</span>
                    {/if}
                    {#if actions.includes('rotate-credential')}
                      <Button
                        size="sm"
                        variant="quiet"
                        icon="refresh"
                        label={`Rotate ${worker.name}'s credential`}
                        disabled={changing === worker.id}
                        onclick={() => change(worker, 'rotate-credential')}>Rotate</Button
                      >
                    {/if}
                  </div>
                </td>
                <td>
                  {#if worker.drives.length === 0}
                    <span class="quiet">None</span>
                  {:else}
                    <ul class="chips">
                      {#each worker.drives as drive (drive.id)}
                        <li>{drive.name}</li>
                      {/each}
                    </ul>
                  {/if}
                </td>
                <td>
                  <div class="row-actions">
                    {#if actions.includes('drain')}
                      <Button
                        size="sm"
                        label={`Drain ${worker.name}`}
                        disabled={changing === worker.id}
                        onclick={() => change(worker, 'drain')}>Drain</Button
                      >
                    {/if}
                    {#if actions.includes('enable')}
                      <Button
                        size="sm"
                        label={`Enable ${worker.name}`}
                        disabled={changing === worker.id}
                        onclick={() => change(worker, 'enable')}>Enable</Button
                      >
                    {/if}
                    {#if actions.includes('revoke')}
                      <Button
                        size="sm"
                        variant="danger"
                        label={`Revoke ${worker.name}…`}
                        onclick={() => startRevoke(worker)}>Revoke…</Button
                      >
                    {/if}
                  </div>
                </td>
              </tr>
            {/each}
          </tbody>
        </Table>
        <div class="padded footnote">
          {#if workers.data.more}
            <p>Showing the first 200 workers.</p>
          {/if}
          <p>
            Draining a worker lets it finish what it holds and gives it nothing new, for
            maintenance. Enabling it undoes that. Rotating its credential has the worker swap it for
            a new one between jobs; the old one works until then, so nothing is interrupted. If the
            credential may be known to somebody else, revoke the worker instead.
          </p>
        </div>
      {/if}

      <div role="alert">
        {#if workerProblem}
          <p class="padded problem">
            <strong>Not changed.</strong>
            {workerProblem.code === 'FORBIDDEN'
              ? 'Only an administrator can change workers.'
              : workerProblem.detail}
          </p>
        {/if}
      </div>

      {#if revoking}
        {@const worker = revoking}
        {#key worker.id}
          <Confirm
            title={`Revoke ${worker.name}`}
            onsubmit={(event) => {
              event.preventDefault();
              void change(worker, 'revoke');
            }}
          >
            <p>
              <strong>Its credential is refused from its next request, for good.</strong> It can no longer
              take work, renew a lease or report. A burn it is writing is not interrupted by the server;
              its lease lapses as a lost worker's does. To use the machine again, issue a new enrollment
              token and enroll it under a new name.
            </p>
            {#snippet actions()}
              <Button type="submit" variant="danger" disabled={changing === worker.id}>
                {changing === worker.id ? 'Revoking…' : `Revoke ${worker.name}`}
              </Button>
              <Button onclick={() => (revoking = null)}>Keep {worker.name}</Button>
            {/snippet}
          </Confirm>
        {/key}
      {/if}
    </Panel>
  </div>

  <div aria-busy={drives.kind === 'loading'}>
    <Panel
      title="Drives"
      aside={drives.kind === 'ready' ? `${drives.data.length}` : undefined}
      flush
    >
      {#if drives.kind === 'loading'}
        <p class="padded quiet">Loading drives…</p>
      {:else if drives.kind === 'error'}
        <div class="padded" role="alert">
          <StatusMark tone="bad" label={drives.problem.detail} />
        </div>
      {:else if drives.data.length === 0}
        <EmptyState inset level={3} title="No drives yet" icon="disc">
          <p>A burn worker reports its drive when it first starts.</p>
        </EmptyState>
      {:else}
        <Table caption="Every drive a worker has reported">
          <thead>
            <tr>
              <th scope="col">Drive</th>
              <th scope="col">Now</th>
              <th scope="col">Worker</th>
              <th scope="col">Model</th>
              <th scope="col">Connection</th>
              <th scope="col">Worker last seen</th>
              <th scope="col"><span class="visually-hidden">Actions</span></th>
            </tr>
          </thead>
          <tbody>
            {#each drives.data as drive (drive.id)}
              {@const entry = bayEntry(drive)}
              <tr>
                <th scope="row">
                  <div class="drive">
                    <Disc
                      mode={entry.disc.mode}
                      shape={entry.disc.shape}
                      family={entry.disc.family}
                      capacity={entry.disc.capacity}
                      progress={entry.disc.progress}
                      size={28}
                      hole="var(--surface)"
                    />
                    <div class="names">
                      <span>{drive.name}</span>
                      <code class="device">{drive.device}</code>
                    </div>
                  </div>
                </th>
                <td>
                  <div class="cell">
                    <StatusMark tone={entry.tone} label={entry.state} />
                    {#if entry.title}
                      {#if entry.href}
                        <a class="small" href={entry.href}>{entry.title}</a>
                      {:else}
                        <span class="small">{entry.title}</span>
                      {/if}
                    {/if}
                  </div>
                </td>
                <td>
                  <div class="cell">
                    <span>{drive.worker_name}</span>
                    <span class="small quiet">{workerStatusLabel(drive.worker_status)}</span>
                  </div>
                </td>
                <td>{driveModel(drive)}</td>
                <td>{connectionLabel(drive.connection)}</td>
                <td>
                  {#if drive.worker_last_seen_at}
                    <time
                      datetime={drive.worker_last_seen_at}
                      title={full(drive.worker_last_seen_at)}
                      >{when(drive.worker_last_seen_at)}</time
                    >
                  {:else}
                    <span class="quiet">Never</span>
                  {/if}
                </td>
                <td>
                  <div class="row-actions">
                    <Button
                      size="sm"
                      variant="danger"
                      label={`Erase the disc in ${drive.name}…`}
                      onclick={() => startErase(drive)}>Erase…</Button
                    >
                  </div>
                </td>
              </tr>
            {/each}
          </tbody>
        </Table>
      {/if}

      {#if erasingDrive}
        {@const drive = erasingDrive}
        {#key drive.id}
          <Confirm title={`Erase the disc in ${drive.name}`} onsubmit={erase}>
            <p>
              <strong>This destroys everything on the disc in that drive.</strong> It cannot be undone.
              Only a rewritable disc (CD-RW, DVD-RW, DVD+RW, BD-RE) is erased; the worker looks first
              and leaves a blank or write-once disc alone, and says so.
            </p>
            <fieldset>
              <legend>How thoroughly</legend>
              <label class="choice">
                <input type="radio" name="mode" value="quick" bind:group={eraseMode} />
                <span>
                  <strong>Quick:</strong> only what makes the disc writable again. Fast on a CD-RW; some
                  drives take half an hour on a DVD-RW whichever you choose
                </span>
              </label>
              <label class="choice">
                <input type="radio" name="mode" value="full" bind:group={eraseMode} />
                <span
                  ><strong>Full:</strong> writes over the whole disc, which can take an hour</span
                >
              </label>
            </fieldset>
            <label class="choice">
              <input type="checkbox" bind:checked={eraseConfirmed} />
              <span>I understand everything on the disc in {drive.name} will be destroyed</span>
            </label>
            <div role="alert">
              {#if eraseProblem}
                <p class="problem">
                  <strong>Not erased.</strong>
                  {eraseProblem.code === 'FORBIDDEN'
                    ? 'Only an administrator can erase discs.'
                    : eraseProblem.detail}
                </p>
              {/if}
            </div>
            {#snippet actions()}
              <Button type="submit" variant="danger" disabled={!eraseConfirmed || eraseSubmitting}>
                {eraseSubmitting ? 'Asking…' : `Erase the disc in ${drive.name}`}
              </Button>
              <Button onclick={() => (erasingDrive = null)}>Keep the disc</Button>
            {/snippet}
          </Confirm>
        {/key}
      {/if}
    </Panel>
  </div>

  <Panel title="Recent erasures" flush>
    <div aria-live="polite">
      {#if erasures.kind === 'loading'}
        <p class="padded quiet">Loading erasures…</p>
      {:else if erasures.kind === 'error'}
        <div class="padded"><StatusMark tone="bad" label={erasures.problem.detail} /></div>
      {:else if erasures.data.length === 0}
        <p class="padded quiet">No disc has been erased.</p>
      {:else}
        <Table caption="Recent erasures, newest first">
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
                <td>
                  <time datetime={erasure.created_at} title={full(erasure.created_at)}
                    >{when(erasure.created_at)}</time
                  >
                  <span class="quiet">by {erasure.requested_by}</span>
                </td>
                <td>{erasure.drive_name}</td>
                <td>{erasure.mode === 'full' ? 'Full' : 'Quick'}</td>
                <td>
                  <div class="cell">
                    <StatusMark
                      tone={erasureStateTone(erasure.state)}
                      label={erasureStateLabel(erasure.state)}
                    />
                    {#if erasure.error_detail}
                      <span class="small quiet">{erasure.error_detail}</span>
                    {/if}
                  </div>
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
                    <div class="row-actions">
                      <Button
                        size="sm"
                        variant="quiet"
                        label={`Withdraw the erasure in ${erasure.drive_name}`}
                        onclick={() => withdraw(erasure)}>Withdraw</Button
                      >
                    </div>
                  {/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </Table>
      {/if}
    </div>
  </Panel>

  <div id="enroll">
    <Panel title="Enroll a burn worker">
      <p class="lede">
        A new worker needs a one-use enrollment token on its first start. It exchanges the token for
        a credential of its own, kept on the worker's volume, and the token is spent.
      </p>

      <form class="enroll" onsubmit={issue}>
        <Field
          label="Valid for (minutes)"
          hint="Long enough to paste it into the worker's settings and start it."
          required
        >
          {#snippet children({ id, describedBy })}
            <input
              {id}
              name="minutes"
              type="number"
              min={ENROLLMENT_MINUTES.min}
              max={ENROLLMENT_MINUTES.max}
              bind:value={minutes}
              aria-describedby={describedBy}
              required
            />
          {/snippet}
        </Field>
        <div>
          <Button type="submit" variant="primary" disabled={submitting}>
            {submitting ? 'Issuing…' : 'Issue an enrollment token'}
          </Button>
        </div>
      </form>

      <div role="alert">
        {#if problem}
          <p class="problem">
            <strong>Not issued.</strong>
            {problem.code === 'FORBIDDEN'
              ? 'Only an administrator can enroll workers.'
              : problem.detail}
          </p>
        {/if}
      </div>

      {#if issued}
        <div aria-live="polite" class="issued">
          <h3>Enrollment token</h3>
          <p>
            Shown this once. The server keeps only its hash, so if it is lost, issue another. It
            lapses at <time datetime={issued.expires_at}>{full(issued.expires_at)}</time>.
          </p>
          <Field label="Token">
            {#snippet children({ id })}
              <input
                {id}
                class="token"
                readonly
                value={issued?.enrollment_token ?? ''}
                spellcheck="false"
                onfocus={(event) => event.currentTarget.select()}
              />
            {/snippet}
          </Field>
          <p>
            Set it as <code>TANGIBLE_ENROLLMENT_TOKEN</code> in the worker's environment, start the worker,
            then remove the setting again.
          </p>
        </div>
      {/if}
    </Panel>
  </div>
</div>

<style>
  .stack {
    display: grid;
    gap: var(--space-5);
  }
  .padded {
    padding: var(--space-3) var(--space-4);
  }
  .small {
    font-size: var(--text-xs);
  }
  .cell {
    display: grid;
    gap: 0.125rem;
    justify-items: start;
  }
  .inline {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
  }
  .row-actions {
    display: flex;
    gap: var(--space-2);
    justify-content: flex-end;
    white-space: nowrap;
  }
  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-1);
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .chips li {
    padding: 0 var(--space-2);
    border: 1px solid var(--line);
    border-radius: var(--radius);
    background: var(--surface-2);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
  }
  .drive {
    display: flex;
    gap: var(--space-3);
    align-items: center;
  }
  .names {
    display: grid;
  }
  .device {
    color: var(--text-muted);
    font-size: var(--text-xs);
    font-weight: var(--weight-regular);
  }
  .footnote {
    border-top: 1px solid var(--line-2);
    color: var(--text-muted);
    font-size: var(--text-sm);
  }
  .footnote p {
    margin: 0;
    max-width: 90ch;
  }
  .footnote p + p {
    margin-top: var(--space-2);
  }
  .problem {
    margin: 0;
    color: var(--bad);
  }
  .choice {
    display: flex;
    gap: var(--space-2);
    align-items: baseline;
    cursor: pointer;
  }
  .lede {
    margin: 0 0 var(--space-4);
    max-width: 75ch;
  }
  .enroll {
    display: grid;
    gap: var(--space-3);
    justify-items: start;
  }
  .enroll input {
    width: 8rem;
  }
  .issued {
    margin-top: var(--space-4);
    padding: var(--space-4);
    border: 1px solid var(--line);
    border-left: 4px solid var(--accent);
    border-radius: var(--radius);
    background: var(--surface-2);
  }
  .issued h3 {
    margin: 0 0 var(--space-2);
    font-size: var(--text-md);
  }
  .issued p {
    max-width: 75ch;
  }
  .token {
    width: 100%;
    font-family: var(--font-mono);
    font-size: var(--text-sm);
  }
</style>
