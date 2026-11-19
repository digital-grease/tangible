<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Copies of the database (ADR 0026, docs/plans/backups-ui.md): the copies
  // in the backups volume, taking one now, downloading, adding and removing
  // one. Removing asks first, under the list, as the DATs page does.
  import Confirm from '$lib/components/ui/Confirm.svelte';
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import Callout from '$lib/components/ui/Callout.svelte';
  import Checkbox from '$lib/components/ui/Checkbox.svelte';
  import EmptyState from '$lib/components/ui/EmptyState.svelte';
  import Field from '$lib/components/ui/Field.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import Panel from '$lib/components/ui/Panel.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import Table from '$lib/components/ui/Table.svelte';
  import { when } from '$lib/overview';
  import {
    FOLLOW_EVERY_MS,
    deleteCopy,
    downloadHref,
    isPruned,
    kindLabel,
    loadBackups,
    operationLabel,
    readiness,
    scheduleLabel,
    setSchedule,
    takeCopy,
    totalSize,
    uploadCopy,
    type Backups,
    type Copy,
  } from '$lib/backups';
  import { formatBytes } from '$lib/library';
  import type { Problem } from '$lib/session';
  import { onDestroy, onMount } from 'svelte';

  type View =
    { kind: 'loading' } | { kind: 'ready'; backups: Backups } | { kind: 'error'; problem: Problem };

  let view = $state<View>({ kind: 'loading' });
  let actionProblem = $state<Problem | null>(null);
  let done = $state<string | null>(null);
  let chosen = $state<File | null>(null);
  let uploading = $state(false);
  let confirmingDelete = $state<string | null>(null);
  let deleteProblem = $state<Problem | null>(null);
  let timer: ReturnType<typeof setTimeout> | null = null;
  /** The schedule being edited, once the page has it. */
  let scheduleForm = $state<{ enabled: boolean; time: string } | null>(null);
  let scheduleSaving = $state(false);
  let scheduleProblem = $state<Problem | null>(null);
  let scheduleSaved = $state(false);

  const deleting = $derived(
    view.kind === 'ready'
      ? (view.backups.copies.find((copy) => copy.name === confirmingDelete) ?? null)
      : null,
  );
  const busy = $derived(view.kind === 'ready' && view.backups.operation != null);

  async function refresh() {
    const result = await loadBackups();
    view = result.kind === 'ok' ? { kind: 'ready', backups: result.data } : result;
    if (result.kind === 'ok' && scheduleForm === null) {
      scheduleForm = { enabled: result.data.schedule.enabled, time: result.data.schedule.time };
    }
    follow();
  }

  async function saveSchedule(event: SubmitEvent) {
    event.preventDefault();
    if (!scheduleForm || scheduleSaving || view.kind !== 'ready') return;
    scheduleSaving = true;
    scheduleProblem = null;
    scheduleSaved = false;
    const result = await setSchedule(scheduleForm.enabled, scheduleForm.time);
    scheduleSaving = false;
    if (result.kind === 'error') {
      scheduleProblem = result.problem;
      return;
    }
    view = { kind: 'ready', backups: { ...view.backups, schedule: result.data } };
    scheduleSaved = true;
  }

  /** Look again while something is under way, and stop once nothing is. */
  function follow() {
    if (timer) clearTimeout(timer);
    timer = null;
    if (view.kind === 'ready' && view.backups.operation) {
      timer = setTimeout(refresh, FOLLOW_EVERY_MS);
    }
  }

  onMount(refresh);
  onDestroy(() => {
    if (timer) clearTimeout(timer);
  });

  async function take() {
    actionProblem = null;
    done = null;
    const result = await takeCopy();
    if (result.kind === 'error') {
      actionProblem = result.problem;
      return;
    }
    view = { kind: 'ready', backups: result.data };
    follow();
  }

  function choose(event: Event) {
    const input = event.currentTarget as HTMLInputElement;
    chosen = input.files?.[0] ?? null;
    actionProblem = null;
    done = null;
  }

  async function upload(event: SubmitEvent) {
    event.preventDefault();
    if (!chosen || uploading || view.kind !== 'ready') return;
    uploading = true;
    actionProblem = null;
    done = null;
    const result = await uploadCopy(chosen, view.backups.max_upload_bytes);
    uploading = false;
    if (result.kind === 'error') {
      actionProblem = result.problem;
      return;
    }
    done = `Kept ${result.data.name}, ${formatBytes(result.data.size_bytes)}.`;
    chosen = null;
    await refresh();
  }

  async function remove(copy: Copy) {
    deleteProblem = null;
    const result = await deleteCopy(copy.name);
    if (result.kind === 'error') {
      deleteProblem = result.problem;
      return;
    }
    confirmingDelete = null;
    done = `Removed ${copy.name}.`;
    await refresh();
  }

  /** The full date and time, for a time's tooltip. */
  function full(value: string): string {
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
  }
</script>

<svelte:head><title>Backups — Tangible</title></svelte:head>

<PageHeader
  title="Backups"
  description="Copies of the database: the catalogue, accounts, jobs, burn history and audit trail. One is taken before every start; take one now, download one to keep elsewhere, or add one from another install. The library's files are not in a copy: back them up with your own tool."
/>

<div class="stack">
  <div aria-live="polite" aria-busy={view.kind === 'loading'}>
    {#if view.kind === 'loading'}
      <p class="quiet">Loading the copies…</p>
    {:else if view.kind === 'error'}
      <ErrorPanel
        problem={view.problem}
        area="deployment"
        endpoint="GET /api/v1/backups"
        heading="The copies could not be loaded"
        level={2}
      />
    {:else}
      {@const backups = view.backups}
      {@const ready = readiness(backups)}
      <Panel title="Taking copies">
        <div class="status">
          <StatusMark tone={ready.tone} label={ready.label} />
          {#if ready.detail}<p class="quiet small">{ready.detail}</p>{/if}
        </div>
        {#if backups.operation}
          <p class="under-way">
            <StatusMark tone="busy" label={operationLabel(backups.operation)} />
          </p>
        {:else if backups.last_outcome}
          {@const last = backups.last_outcome}
          <p class="under-way">
            {#if last.error}
              <StatusMark tone="bad" label={`The last copy failed: ${last.error}`} />
            {:else}
              <StatusMark
                tone="ok"
                label={`${last.name ?? 'A copy'} was kept ${when(last.finished_at)}`}
              />
            {/if}
          </p>
        {/if}
        <div class="actions">
          <Button
            variant="primary"
            onclick={take}
            disabled={busy || !backups.tools.ready || !backups.writable || backups.kept === 0}
          >
            Take a copy now
          </Button>
        </div>
        {#if done}<p class="outcome"><StatusMark tone="ok" label={done} /></p>{/if}
        {#if actionProblem}
          <div class="refusal">
            <ErrorPanel problem={actionProblem} area="deployment" refusal />
          </div>
        {/if}
      </Panel>

      <Panel title="Every day">
        <p class="schedule-now">
          <StatusMark
            tone={backups.schedule.enabled && backups.kept > 0 ? 'ok' : 'idle'}
            label={scheduleLabel(backups.schedule, backups.kept)}
          />
          {#if backups.schedule.next_at}
            <span class="quiet small"
              >The next is due <time
                datetime={backups.schedule.next_at}
                title={full(backups.schedule.next_at)}>{when(backups.schedule.next_at)}</time
              >.</span
            >
          {/if}
        </p>
        {#if scheduleForm}
          <form class="schedule" onsubmit={saveSchedule}>
            <Checkbox
              label="Take a copy every day"
              hint="One is taken at every start as well. A time missed while the server is down is not made up."
              bind:checked={scheduleForm.enabled}
            />
            <Field label="At, in UTC" hint="Copies taken this way count towards those kept.">
              {#snippet children({ id, describedBy })}
                <input
                  {id}
                  type="time"
                  step="60"
                  required
                  aria-describedby={describedBy}
                  disabled={!scheduleForm?.enabled}
                  bind:value={scheduleForm!.time}
                />
              {/snippet}
            </Field>
            <div>
              <Button type="submit" disabled={scheduleSaving}>
                {scheduleSaving ? 'Saving…' : 'Save the schedule'}
              </Button>
            </div>
          </form>
          <div aria-live="polite">
            {#if scheduleSaved}<p class="outcome"><StatusMark tone="ok" label="Saved." /></p>{/if}
          </div>
          {#if scheduleProblem}
            <div class="refusal">
              <ErrorPanel problem={scheduleProblem} area="deployment" refusal />
            </div>
          {/if}
        {/if}
      </Panel>

      <Panel title="Add a copy">
        <form class="upload" onsubmit={upload}>
          <Field
            label="A copy of a Tangible database"
            hint={`A .dump file Tangible took, here or on another install, up to ${formatBytes(backups.max_upload_bytes)}. It is kept beside the others and never removed to make room; nothing in it is run.`}
          >
            {#snippet children({ id, describedBy })}
              <input
                {id}
                type="file"
                accept=".dump,application/octet-stream"
                aria-describedby={describedBy}
                onchange={choose}
              />
            {/snippet}
          </Field>
          <div>
            <Button type="submit" disabled={!chosen || uploading || busy}>
              {uploading ? 'Uploading…' : 'Upload'}
            </Button>
          </div>
        </form>
      </Panel>

      <Panel
        title="Copies"
        aside={backups.copies.length > 0
          ? `${backups.copies.length}, ${totalSize(backups.copies)}`
          : undefined}
        flush
      >
        {#if backups.copies.length === 0}
          <EmptyState inset level={3} title="No copies yet" icon="backup">
            <p>One is taken at every start, or take one now above.</p>
          </EmptyState>
        {:else}
          <Table caption="Copies of the database">
            <thead>
              <tr>
                <th scope="col">Copy</th>
                <th scope="col">Taken</th>
                <th scope="col">Size</th>
                <th scope="col"><span class="visually-hidden">Actions</span></th>
              </tr>
            </thead>
            <tbody>
              {#each backups.copies as copy (copy.name)}
                <tr>
                  <th scope="row">
                    <div class="cell">
                      <code>{copy.name}</code>
                      <span class="small quiet"
                        >{kindLabel(copy.kind)}{isPruned(copy.kind)
                          ? ''
                          : '; never removed to make room'}</span
                      >
                    </div>
                  </th>
                  <td>
                    <time datetime={copy.taken_at} title={full(copy.taken_at)}
                      >{when(copy.taken_at)}</time
                    >
                  </td>
                  <td>{formatBytes(copy.size_bytes)}</td>
                  <td>
                    <div class="row-actions">
                      <a class="download" href={downloadHref(copy.name)} download={copy.name}
                        >Download<span class="visually-hidden"> {copy.name}</span></a
                      >
                      <Button
                        size="sm"
                        variant="danger"
                        disabled={busy}
                        onclick={() => {
                          confirmingDelete = copy.name;
                          deleteProblem = null;
                        }}
                        label={`Remove ${copy.name}`}>Remove</Button
                      >
                    </div>
                  </td>
                </tr>
              {/each}
            </tbody>
          </Table>
        {/if}

        {#if deleting}
          {@const copy = deleting}
          {#key copy.name}
            <Confirm title={`Remove ${copy.name}?`}>
              <p>
                The copy is deleted from the backups volume, and cannot be restored from again.
                Nothing else changes.
              </p>
              {#if deleteProblem}
                <ErrorPanel problem={deleteProblem} area="deployment" refusal />
              {/if}

              {#snippet actions()}
                <Button variant="danger" onclick={() => remove(copy)}>Yes, remove it</Button>
                <Button onclick={() => (confirmingDelete = null)}>Keep it</Button>
              {/snippet}
            </Confirm>
          {/key}
        {/if}
      </Panel>

      <Callout tone="info" title="What a copy holds">
        <p>
          Every account's password hash, the integration passwords sealed with this server's key,
          and the audit trail: keep a downloaded copy where only you can read it. The key itself is
          not in it, so a copy's integration passwords open only on a server holding the same key;
          copy the key out at the machine (deploy/README.md, "Integration secrets"). The library's
          files are not in it either.
        </p>
      </Callout>
    {/if}
  </div>
</div>

<style>
  .stack {
    display: grid;
    gap: var(--space-5);
  }
  .stack > div {
    display: grid;
    gap: var(--space-5);
  }
  .small {
    font-size: var(--text-xs);
    font-weight: var(--weight-regular);
  }
  .status {
    display: grid;
    gap: var(--space-1);
  }
  .status p,
  .under-way {
    margin: 0;
  }
  .under-way {
    margin-top: var(--space-3);
  }
  .actions {
    margin-top: var(--space-4);
  }
  .cell {
    display: grid;
    gap: 0.125rem;
  }
  .row-actions {
    display: flex;
    gap: var(--space-3);
    align-items: center;
    justify-content: flex-end;
  }
  .download {
    font-size: var(--text-sm);
  }
  .schedule-now {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2) var(--space-3);
    align-items: baseline;
    margin: 0 0 var(--space-4);
  }
  .schedule,
  .upload {
    display: grid;
    gap: var(--space-3);
    justify-items: start;
    max-width: 40rem;
  }
  .outcome {
    margin: var(--space-4) 0 0;
  }
  .refusal {
    margin-top: var(--space-4);
  }
</style>
