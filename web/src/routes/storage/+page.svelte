<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // The store as a whole, for administrators (docs/plans/storage-integrity.md):
  // full scrubs and every integrity check, and the garbage collection plan.
  // Nothing on this page repairs or removes a stored file; the server
  // cannot do either.
  import { onDestroy, onMount } from 'svelte';
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import {
    FOLLOW_EVERY_MS,
    cancelCheck,
    checkPercent,
    checkProgress,
    checkStateLabel,
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

  function when(value: string | null | undefined): string {
    if (!value) return '';
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
  }
</script>

<svelte:head><title>Storage — Tangible</title></svelte:head>

<h1>Storage</h1>

<p>
  Integrity checks read stored files back and compare each with the digest recorded when it was
  stored. The collection plan says which stored files nothing refers to. Nothing here repairs or
  removes a file: originals are never rewritten, and the remedy for damage is your backup.
</p>

<div role="alert">
  {#if refusal}<p><strong>Not done.</strong> {refusal.detail}</p>{/if}
</div>
<p aria-live="polite">{status}</p>

<div aria-busy={view.kind === 'loading'}>
  {#if view.kind === 'loading'}
    <p>Loading checks…</p>
  {:else if view.kind === 'forbidden'}
    <p>Only an administrator can see storage.</p>
  {:else if view.kind === 'error'}
    <ErrorPanel
      problem={view.problem}
      area="library"
      endpoint="GET /api/v1/integrity-checks"
      heading="Could not load integrity checks"
      level={2}
    />
  {:else}
    <section aria-labelledby="scrub-heading">
      <h2 id="scrub-heading">Full scrub</h2>
      <div aria-live="polite">
        {#if activeScrub}
          {@const percent = checkPercent(activeScrub)}
          <p>
            <strong>{checkStateLabel(activeScrub)}.</strong>
            {checkProgress(activeScrub)}, {activeScrub.problem_count} problem{activeScrub.problem_count ===
            1
              ? ''
              : 's'} so far.
          </p>
          {#if percent !== null}
            <progress max="100" value={percent} aria-label="Share of the library read"
              >{percent}%</progress
            >
          {/if}
          <p>
            <button type="button" onclick={() => withdraw(activeScrub)}>Withdraw the scrub</button>
            <small>It stops after the file it is reading. Nothing is changed.</small>
          </p>
        {:else}
          <p>
            {#if lastScrub}
              The last ended {when(lastScrub.completed_at)}: {checkStateLabel(lastScrub)}.
            {:else}
              No full scrub has run.
            {/if}
          </p>
          <p>
            <button type="button" onclick={scrub} disabled={asking}>
              {asking ? 'Asking…' : 'Check the whole library'}
            </button>
            <br />
            <small
              >Reads every stored byte, so the library's disks are busy until it ends. It can be
              withdrawn at any time. The server can queue one itself every few days (<code
                >TANGIBLE_SCRUB_INTERVAL_DAYS</code
              >).</small
            >
          </p>
        {/if}
      </div>
    </section>

    <section aria-labelledby="checks-heading">
      <h2 id="checks-heading">Checks</h2>
      {#if view.checks.length === 0}
        <p>No integrity check has been asked for.</p>
      {:else}
        <table>
          <caption class="visually-hidden">Integrity checks, newest first</caption>
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
                <th scope="row">{when(check.created_at)}</th>
                <td>
                  {#if check.artifact_id}
                    <a href={`/library/${check.artifact_id}`}>{scopeLabel(check)}</a>
                  {:else}
                    {scopeLabel(check)}
                  {/if}
                </td>
                <td>{check.created_by}</td>
                <td>
                  {checkStateLabel(check)}
                  {#if check.state === 'failed' && check.error_detail}
                    <br /><small>{check.error_detail}</small>
                  {/if}
                  {#if isActive(check) && check.scope !== 'all'}
                    <br /><button type="button" onclick={() => withdraw(check)}>Withdraw</button>
                  {/if}
                </td>
                <td>
                  {checkProgress(check)}
                  {#if check.problems.length > 0}
                    <details>
                      <summary
                        >{check.problem_count} problem{check.problem_count === 1
                          ? ''
                          : 's'}</summary
                      >
                      <ul>
                        {#each check.problems as problem, index (index)}
                          <li>
                            {problemKindLabel(problem.kind)}{#if problem.sha256}:
                              <code>{problem.sha256}</code>{/if}
                            <br /><small>{problem.detail}</small>
                          </li>
                        {/each}
                      </ul>
                      {#if check.problem_count > check.problems.length}
                        <p>
                          And {check.problem_count - check.problems.length} more, counted but not kept.
                        </p>
                      {/if}
                    </details>
                  {/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
    </section>
  {/if}
</div>

<section aria-labelledby="gc-heading">
  <h2 id="gc-heading">Collection plan</h2>
  <p>
    What garbage collection would remove: stored files that no artifact, manifest or import refers
    to, older than a grace period. A plan only. Nothing is removed, and this server cannot remove
    stored files.
  </p>
  <form
    onsubmit={(event) => {
      event.preventDefault();
      void planCollection();
    }}
  >
    <label>
      Grace period, in days
      <input type="number" min="0" max="3650" step="1" required bind:value={graceDays} />
    </label>
    <button type="submit" disabled={planning}>{planning ? 'Planning…' : 'Plan a collection'}</button
    >
    <br />
    <small>Reads every directory in the store, so it takes a while on a large library.</small>
  </form>
  <div role="alert">
    {#if planProblem}<p><strong>No plan.</strong> {planProblem.detail}</p>{/if}
  </div>
  <div aria-live="polite">
    {#if plan}
      {#if !plan.certain}
        <p class="badge">
          <strong>Uncertain.</strong>
          {plan.manifests_unreadable} manifest(s) and {plan.checkpoints_unreadable} import checkpoint(s)
          could not be read, and any of them may name a file listed here. The server log says which.
        </p>
      {/if}
      <dl>
        <dt>Stored</dt>
        <dd>{tallyLabel(plan.stored)}</dd>
        <dt>In use</dt>
        <dd>{tallyLabel(plan.live)}</dd>
        <dt>Unused, recent</dt>
        <dd>{tallyLabel(plan.recent)}, kept for {graceLabel(plan)}</dd>
        <dt>Would be removed</dt>
        <dd>{tallyLabel(plan.collectable)}</dd>
        <dt>Partial files</dt>
        <dd>
          {tallyLabel(plan.partial_files)} left by interrupted imports, {plan.stale_partial_files} older
          than {graceLabel(plan)}
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
        <table>
          <caption>
            {plan.candidates.length < plan.collectable.count
              ? `The ${plan.candidates.length} largest of ${plan.collectable.count} files collection would remove`
              : 'Files collection would remove'}
          </caption>
          <thead>
            <tr>
              <th scope="col">SHA-256</th>
              <th scope="col">Size</th>
              <th scope="col">Written</th>
              <th scope="col">Catalogued</th>
            </tr>
          </thead>
          <tbody>
            {#each plan.candidates as candidate (candidate.sha256)}
              <tr>
                <th scope="row"><code>{candidate.sha256}</code></th>
                <td>{formatBytes(candidate.size_bytes)}</td>
                <td>{when(candidate.modified_at)}</td>
                <td>{candidate.recorded ? 'Recorded, unreferenced' : 'Never catalogued'}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
      <p><small>Planned {when(plan.planned_at)}.</small></p>
    {/if}
  </div>
</section>

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
  th,
  td {
    text-align: left;
    padding: 0.5rem;
    border-bottom: 1px solid currentColor;
    vertical-align: top;
  }
  code {
    word-break: break-all;
  }
  .badge {
    border: 1px solid currentColor;
    padding: 0.75rem;
  }
  .visually-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
  }
</style>
