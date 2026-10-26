<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // An artifact's integrity: what the last check of all its stored files
  // found, and "Verify now", which reads them back against their digests
  // and follows the check to its end. Nothing found is repaired.
  import { onDestroy } from 'svelte';
  import {
    FOLLOW_EVERY_MS,
    checkPercent,
    checkProgress,
    checkStateLabel,
    integrityLabel,
    integrityMark,
    isActive,
    loadCheck,
    problemKindLabel,
    validateArtifact,
    type IntegrityCheck,
  } from '$lib/integrity';
  import { sessionRole, type Problem } from '$lib/session';

  let {
    artifactId,
    integrityState,
    checkedAt = null,
    onchecked = () => {},
  }: {
    artifactId: string;
    integrityState: string;
    checkedAt?: string | null;
    /** Called when a check this started has ended, to load the artifact again. */
    onchecked?: () => void;
  } = $props();

  let check = $state<IntegrityCheck | null>(null);
  let asking = $state(false);
  let refusal = $state<Problem | null>(null);
  let timer: ReturnType<typeof setTimeout> | undefined;

  onDestroy(() => clearTimeout(timer));

  // A viewer may not ask; the server refuses anyone else it should.
  const mayAsk = sessionRole() !== 'viewer';

  function when(value: string): string {
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
  }

  function follow(id: string) {
    clearTimeout(timer);
    timer = setTimeout(async () => {
      const result = await loadCheck(id);
      if (result.kind === 'error') {
        refusal = result.problem;
        return;
      }
      check = result.data;
      if (isActive(result.data)) follow(id);
      else onchecked();
    }, FOLLOW_EVERY_MS);
  }

  async function verify() {
    if (asking) return;
    asking = true;
    refusal = null;
    const result = await validateArtifact(artifactId);
    asking = false;
    if (result.kind === 'error') {
      refusal = result.problem;
      return;
    }
    check = result.data;
    if (isActive(result.data)) follow(result.data.id);
    else onchecked();
  }
</script>

<section aria-labelledby="integrity-heading">
  <h2 id="integrity-heading">Integrity</h2>
  <p>
    <span aria-hidden="true">{integrityMark(integrityState)}</span>
    {integrityLabel(integrityState)}{#if checkedAt}, checked {when(checkedAt)}{/if}.
  </p>
  {#if integrityState === 'damaged'}
    <p class="badge">
      <strong>Nothing was repaired.</strong>
      Originals are never rewritten. Restore the damaged files from a backup, then verify again; the deploy
      guide's "Checking stored bytes" says where each file lives.
    </p>
  {/if}

  {#if mayAsk}
    <p>
      <button
        type="button"
        onclick={verify}
        disabled={asking || (check !== null && isActive(check))}
      >
        {asking ? 'Asking…' : 'Verify now'}
      </button>
      <br />
      <small
        >Reads every stored file of this artifact back and compares it with the digest recorded when
        it was stored, and its manifest with the catalogue. Changes nothing.</small
      >
    </p>
  {/if}

  <div role="alert">
    {#if refusal}<p><strong>Not checked.</strong> {refusal.detail}</p>{/if}
  </div>

  <div aria-live="polite">
    {#if check}
      {@const percent = checkPercent(check)}
      <p>
        <strong>{checkStateLabel(check)}.</strong>
        {#if isActive(check)}{checkProgress(check)}{/if}
      </p>
      {#if isActive(check) && percent !== null}
        <progress max="100" value={percent} aria-label="Share of this artifact read"
          >{percent}%</progress
        >
      {/if}
      {#if check.state === 'failed' && check.error_detail}
        <p>{check.error_detail}</p>
      {/if}
      {#if check.problems.length > 0}
        <ul>
          {#each check.problems as problem, index (index)}
            <li>
              {problemKindLabel(problem.kind)}{#if problem.sha256}:
                <code title={problem.sha256}>{problem.sha256.slice(0, 12)}…</code>{/if}
              <br /><small>{problem.detail}</small>
            </li>
          {/each}
        </ul>
      {/if}
    {/if}
  </div>
</section>

<style>
  .badge {
    border: 1px solid currentColor;
    padding: 0.75rem;
  }
</style>
