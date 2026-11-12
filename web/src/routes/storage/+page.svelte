<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // The store as a whole, for administrators (docs/plans/storage-integrity.md;
  // UX.md 21): full scrubs and every integrity check, and the garbage
  // collection plan. Nothing on this page repairs or removes a stored file;
  // the server cannot do either. In the new design (docs/plans/ui-overhaul.md,
  // U8): a panel for the scrub, one for the checks, one for the plan.
  import Callout from '$lib/components/ui/Callout.svelte';
  import { onDestroy, onMount } from 'svelte';
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import EmptyState from '$lib/components/ui/EmptyState.svelte';
  import Field from '$lib/components/ui/Field.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import Panel from '$lib/components/ui/Panel.svelte';
  import Progress from '$lib/components/ui/Progress.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import Table from '$lib/components/ui/Table.svelte';
  import { when } from '$lib/overview';
  import {
    FOLLOW_EVERY_MS,
    cancelCheck,
    checkPercent,
    checkProgress,
    checkStateLabel,
    checkStateTone,
    graceLabel,
    isActive,
    loadChecks,
    loadGcPlan,
    problemKindLabel,
    requestScrub,
    scopeLabel,
    tallyLabel,
    type GcPlan,
    type IntegrityCheck,
  } from '$lib/integrity';
  import { formatBytes } from '$lib/library';
  import type { Problem } from '$lib/session';

  type View =
    | { kind: 'loading' }
    | { kind: 'forbidden' }
    | { kind: 'error'; problem: Problem }
    | { kind: 'ready'; checks: IntegrityCheck[] };

  let view = $state<View>({ kind: 'loading' });
  let refusal = $state<Problem | null>(null);
  let asking = $state(false);
  let status = $state('');
  let timer: ReturnType<typeof setTimeout> | undefined;

  let graceDays = $state(7);
  let planning = $state(false);
  let plan = $state<GcPlan | null>(null);
  let planProblem = $state<Problem | null>(null);

  const activeScrub = $derived(
    view.kind === 'ready'
      ? (view.checks.find((check) => check.scope === 'all' && isActive(check)) ?? null)
      : null,
  );
  const lastScrub = $derived(
    view.kind === 'ready'
      ? (view.checks.find((check) => check.scope === 'all' && !isActive(check)) ?? null)
      : null,
  );

  async function refresh() {
    clearTimeout(timer);
    const result = await loadChecks();
    if (result.kind === 'ok') {
      view = { kind: 'ready', checks: result.data.items };
      // Look again while anything is under way.
      if (result.data.items.some(isActive)) timer = setTimeout(refresh, FOLLOW_EVERY_MS);
    } else if (result.problem.code === 'FORBIDDEN') {
      view = { kind: 'forbidden' };
    } else if (view.kind === 'ready') {
      refusal = result.problem;
    } else {
      view = { kind: 'error', problem: result.problem };
    }
  }

  onMount(() => void refresh());
  onDestroy(() => clearTimeout(timer));

  async function scrub() {
    if (asking) return;
    asking = true;
    refusal = null;
    const result = await requestScrub();
    asking = false;
    if (result.kind === 'error') {
      refusal = result.problem;
      return;
    }
    status = isActive(result.data)
      ? 'A full scrub is queued. This page follows it.'
      : 'The full scrub has ended.';
    await refresh();
  }

  async function withdraw(check: IntegrityCheck) {
    refusal = null;
    const result = await cancelCheck(check.id);
    if (result.kind === 'error') {
      refusal = result.problem;
      return;
    }
    status = 'Withdrawn. What it had read is kept; nothing was changed.';
    await refresh();
  }

  async function planCollection() {
    if (planning) return;
    planning = true;
    planProblem = null;
    const result = await loadGcPlan(graceDays);
    planning = false;
    if (result.kind === 'error') {
      planProblem = result.problem;
      return;
    }
    plan = result.data;
  }

  /** The full date and time, for a tooltip or a sentence. */
  function full(value: string | null | undefined): string {
    if (!value) return '';
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
  }
</script>

<svelte:head><title>Storage — Tangible</title></svelte:head>

<PageHeader
  title="Storage"
  description="Integrity checks read stored files back and compare each with the digest recorded when it was stored. The collection plan says which stored files nothing refers to. Nothing here repairs or removes a file: originals are never rewritten, and the remedy for damage is your backup."
/>

<div class="messages">
  <div role="alert">
    {#if refusal}<p class="problem"><strong>Not done.</strong> {refusal.detail}</p>{/if}
  </div>
  <p aria-live="polite" class="status" class:shown={status !== ''}>{status}</p>
</div>

<div class="stack">
  <div aria-busy={view.kind === 'loading'}>
    {#if view.kind === 'loading'}
      <p class="quiet">Loading checks…</p>
    {:else if view.kind === 'forbidden'}
      <EmptyState title="Administrators only" icon="lock">
        <p>Only an administrator can see storage.</p>
      </EmptyState>
    {:else if view.kind === 'error'}
      <ErrorPanel
        problem={view.problem}
        area="library"
        endpoint="GET /api/v1/integrity-checks"
        heading="Could not load integrity checks"
        level={2}
      />
    {:else}
      <div class="stack">
        <Panel title="Full scrub">
          <div aria-live="polite" class="scrub">
            {#if activeScrub}
              {@const percent = checkPercent(activeScrub)}
              <p class="state">
                <StatusMark tone="busy" label={checkStateLabel(activeScrub)} />
                <span
                  >{checkProgress(activeScrub)}, {activeScrub.problem_count} problem{activeScrub.problem_count ===
                  1
                    ? ''
                    : 's'} so far.</span
                >
              </p>
              <Progress
                value={percent === null ? null : percent / 100}
                label="Share of the library read"
              />
              <div class="action">
                <Button onclick={() => withdraw(activeScrub)}>Withdraw the scrub</Button>
                <p class="hint">It stops after the file it is reading. Nothing is changed.</p>
              </div>
            {:else}
              <p class="state">
                {#if lastScrub}
                  <StatusMark
                    tone={checkStateTone(lastScrub)}
                    label={`The last ended ${lastScrub.completed_at ? when(lastScrub.completed_at) : ''}: ${checkStateLabel(lastScrub)}.`}
                  />
                {:else}
                  <StatusMark tone="idle" label="No full scrub has run." />
                {/if}
              </p>
              <div class="action">
                <Button variant="primary" onclick={scrub} disabled={asking}>
                  {asking ? 'Asking…' : 'Check the whole library'}
                </Button>
                <p class="hint">
                  Reads every stored byte, so the library's disks are busy until it ends. It can be
                  withdrawn at any time. The server can queue one itself every few days (<code
                    >TANGIBLE_SCRUB_INTERVAL_DAYS</code
                  >).
                </p>
              </div>
            {/if}
          </div>
        </Panel>

        <Panel title="Checks" aside={`${view.checks.length}`} flush>
          {#if view.checks.length === 0}
            <p class="padded quiet">No integrity check has been asked for.</p>
          {:else}
            <Table caption="Integrity checks, newest first">
              <thead>
                <tr>
                  <th scope="col">Asked</th>
                  <th scope="col">What</th>
                  <th scope="col">By</th>
                  <th scope="col">State</th>
                  <th scope="col">Read</th>
                </tr>
              </thead>
              <tbody>
                {#each view.checks as check (check.id)}
                  <tr>
                    <th scope="row">
                      <time datetime={check.created_at} title={full(check.created_at)}
                        >{when(check.created_at)}</time
                      >
                    </th>
                    <td>
                      {#if check.artifact_id}
                        <a href={`/library/${check.artifact_id}`}>{scopeLabel(check)}</a>
                      {:else}
                        {scopeLabel(check)}
                      {/if}
                    </td>
                    <td>{check.created_by}</td>
                    <td>
                      <div class="cell">
                        <StatusMark tone={checkStateTone(check)} label={checkStateLabel(check)} />
                        {#if check.state === 'failed' && check.error_detail}
                          <span class="small quiet">{check.error_detail}</span>
                        {/if}
                        {#if isActive(check) && check.scope !== 'all'}
                          <Button
                            size="sm"
                            variant="quiet"
                            label={`Withdraw the check of ${scopeLabel(check)}`}
                            onclick={() => withdraw(check)}>Withdraw</Button
                          >
                        {/if}
                      </div>
                    </td>
                    <td>
                      <div class="cell">
                        <span>{checkProgress(check)}</span>
                        {#if check.problems.length > 0}
                          <details>
                            <summary
                              >{check.problem_count} problem{check.problem_count === 1
                                ? ''
                                : 's'}</summary
                            >
                            <ul class="problems">
                              {#each check.problems as problem, index (index)}
                                <li>
                                  <strong>{problemKindLabel(problem.kind)}</strong
                                  >{#if problem.sha256}: <code>{problem.sha256}</code>{/if}
                                  <span class="small quiet">{problem.detail}</span>
                                </li>
                              {/each}
                            </ul>
                            {#if check.problem_count > check.problems.length}
                              <p class="small">
                                And {check.problem_count - check.problems.length} more, counted but not
                                kept.
                              </p>
                            {/if}
                          </details>
                        {/if}
                      </div>
                    </td>
                  </tr>
                {/each}
              </tbody>
            </Table>
          {/if}
        </Panel>
      </div>
    {/if}
  </div>

  <Panel title="Collection plan">
    <p class="lede">
      What garbage collection would remove: stored files that no artifact, manifest or import refers
      to, older than a grace period. A plan only. Nothing is removed, and this server cannot remove
      stored files.
    </p>
    <form
      class="plan-form"
      onsubmit={(event) => {
        event.preventDefault();
        void planCollection();
      }}
    >
      <Field
        label="Grace period, in days"
        hint="Reads every directory in the store, so it takes a while on a large library."
      >
        {#snippet children({ id, describedBy })}
          <input
            {id}
            type="number"
            min="0"
            max="3650"
            step="1"
            required
            bind:value={graceDays}
            aria-describedby={describedBy}
          />
        {/snippet}
      </Field>
      <div>
        <Button type="submit" disabled={planning}>
          {planning ? 'Planning…' : 'Plan a collection'}
        </Button>
      </div>
    </form>
    <div role="alert">
      {#if planProblem}<p class="problem"><strong>No plan.</strong> {planProblem.detail}</p>{/if}
    </div>
    <div aria-live="polite">
      {#if plan}
        <div class="plan">
          {#if !plan.certain}
            <Callout tone="warn">
              <StatusMark tone="warn" label="Uncertain." />
              <p>
                {plan.manifests_unreadable} manifest(s) and {plan.checkpoints_unreadable} import checkpoint(s)
                could not be read, and any of them may name a file listed here. The server log says which.
              </p>
            </Callout>
          {/if}
          <dl class="facts">
            <dt>Stored</dt>
            <dd>{tallyLabel(plan.stored)}</dd>
            <dt>In use</dt>
            <dd>{tallyLabel(plan.live)}</dd>
            <dt>Unused, recent</dt>
            <dd>{tallyLabel(plan.recent)}, kept for {graceLabel(plan)}</dd>
            <dt>Would be removed</dt>
            <dd><strong>{tallyLabel(plan.collectable)}</strong></dd>
            <dt>Partial files</dt>
            <dd>
              {tallyLabel(plan.partial_files)} left by interrupted imports, {plan.stale_partial_files}
              older than {graceLabel(plan)}
            </dd>
            <dt>Not stored files</dt>
            <dd>{tallyLabel(plan.unrecognized)} in the object tree; never removed</dd>
            <dt>Missing</dt>
            <dd>
              {tallyLabel(plan.missing)} the catalogue records have no file{#if plan.missing.count > 0};
                a full scrub says which artifacts they belong to{/if}
            </dd>
          </dl>
          {#if plan.candidates.length > 0}
            <div class="candidates">
              <Table
                caption={plan.candidates.length < plan.collectable.count
                  ? `The ${plan.candidates.length} largest of ${plan.collectable.count} files collection would remove`
                  : 'Files collection would remove'}
                showCaption
                maxHeight="24rem"
              >
                <thead>
                  <tr>
                    <th scope="col">SHA-256</th>
                    <th scope="col" class="num">Size</th>
                    <th scope="col">Written</th>
                    <th scope="col">Catalogued</th>
                  </tr>
                </thead>
                <tbody>
                  {#each plan.candidates as candidate (candidate.sha256)}
                    <tr>
                      <th scope="row"><code class="digest">{candidate.sha256}</code></th>
                      <td class="num">{formatBytes(candidate.size_bytes)}</td>
                      <td>
                        <time datetime={candidate.modified_at} title={full(candidate.modified_at)}
                          >{when(candidate.modified_at)}</time
                        >
                      </td>
                      <td>{candidate.recorded ? 'Recorded, unreferenced' : 'Never catalogued'}</td>
                    </tr>
                  {/each}
                </tbody>
              </Table>
            </div>
          {/if}
          <p class="small quiet">Planned {full(plan.planned_at)}.</p>
        </div>
      {/if}
    </div>
  </Panel>
</div>

<style>
  .stack {
    display: grid;
    gap: var(--space-5);
  }
  .messages .problem {
    margin: 0 0 var(--space-3);
  }
  .status {
    margin: 0;
  }
  .status.shown {
    margin-bottom: var(--space-4);
  }
  .small {
    font-size: var(--text-xs);
  }
  .padded {
    margin: 0;
    padding: var(--space-3) var(--space-4);
  }
  .problem {
    margin: 0;
    color: var(--bad);
  }
  .scrub {
    display: grid;
    gap: var(--space-3);
    max-width: 75ch;
  }
  .state {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    align-items: baseline;
    margin: 0;
  }
  .action {
    display: grid;
    gap: var(--space-2);
    justify-items: start;
  }
  .hint {
    margin: 0;
    font-size: var(--text-sm);
    color: var(--text-muted);
  }
  .cell {
    display: grid;
    gap: 0.125rem;
    justify-items: start;
  }
  details summary {
    cursor: pointer;
    color: var(--bad);
    font-weight: var(--weight-medium);
  }
  .problems {
    display: grid;
    gap: var(--space-1);
    margin: var(--space-2) 0 0;
    padding-left: var(--space-4);
  }
  .problems li {
    display: grid;
  }
  .problems code,
  .digest {
    overflow-wrap: anywhere;
    font-size: var(--text-xs);
  }
  .lede {
    margin: 0 0 var(--space-4);
    max-width: 75ch;
  }
  .plan-form {
    display: grid;
    gap: var(--space-3);
    justify-items: start;
  }
  .plan-form input {
    width: 8rem;
  }
  .plan {
    display: grid;
    gap: var(--space-4);
    margin-top: var(--space-4);
  }
  .facts {
    display: grid;
    grid-template-columns: max-content minmax(0, 1fr);
    gap: var(--space-1) var(--space-5);
    margin: 0;
  }
  .facts dt {
    color: var(--text-muted);
  }
  .facts dd {
    margin: 0;
  }
  .candidates {
    border: 1px solid var(--line);
    border-radius: var(--radius);
    overflow: hidden;
  }
  .plan > p {
    margin: 0;
  }
</style>
