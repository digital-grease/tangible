<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // An artifact's integrity: what the last check of all its stored files
  // found, and "Verify now", which reads them back against their digests
  // and follows the check to its end. Nothing found is repaired.
  import Button from '$lib/components/ui/Button.svelte';
  import Callout from '$lib/components/ui/Callout.svelte';
  import Progress from '$lib/components/ui/Progress.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import { shortDigest } from '$lib/ids';
  import { integrityTone } from '$lib/library';
  import { onDestroy } from 'svelte';
  import {
    FOLLOW_EVERY_MS,
    checkPercent,
    checkProgress,
    checkStateLabel,
    checkStateTone,
    integrityLabel,
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
  <p class="state">
    <StatusMark
      tone={integrityTone(integrityState)}
      label={`${integrityLabel(integrityState)}${checkedAt ? `, checked ${when(checkedAt)}` : ''}.`}
    />
  </p>
  {#if integrityState === 'damaged'}
    <Callout tone="bad">
      <p>
        <strong>Nothing was repaired.</strong>
        Originals are never rewritten. Restore the damaged files from a backup, then verify again; the
        deploy guide's "Checking stored bytes" says where each file lives.
      </p>
    </Callout>
  {/if}

  {#if mayAsk}
    <div class="action">
      <Button onclick={verify} disabled={asking || (check !== null && isActive(check))}>
        {asking ? 'Asking…' : 'Verify now'}
      </Button>
      <p class="hint">
        Reads every stored file of this artifact back and compares it with the digest recorded when
        it was stored, and its manifest with the catalogue. Changes nothing.
      </p>
    </div>
  {/if}

  <div role="alert">
    {#if refusal}<p class="problem"><strong>Not checked.</strong> {refusal.detail}</p>{/if}
  </div>

  <div aria-live="polite">
    {#if check}
      {@const percent = checkPercent(check)}
      <div class="check">
        <p class="state">
          <StatusMark tone={checkStateTone(check)} label={`${checkStateLabel(check)}.`} />
          {#if isActive(check)}<span>{checkProgress(check)}</span>{/if}
        </p>
        {#if isActive(check) && percent !== null}
          <Progress value={percent / 100} label="Share of this artifact read" />
        {/if}
        {#if check.state === 'failed' && check.error_detail}
          <p>{check.error_detail}</p>
        {/if}
        {#if check.problems.length > 0}
          <ul class="problems">
            {#each check.problems as problem, index (index)}
              <li>
                <strong>{problemKindLabel(problem.kind)}</strong>{#if problem.sha256}: <code
                    title={problem.sha256}>{shortDigest(problem.sha256)}</code
                  >{/if}
                <span class="hint">{problem.detail}</span>
              </li>
            {/each}
          </ul>
        {/if}
      </div>
    {/if}
  </div>
</section>

<style>
  .state {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    align-items: baseline;
  }
  .state :global(.mark) {
    font-weight: var(--weight-medium);
  }
  .action {
    display: grid;
    gap: var(--space-2);
    justify-items: start;
    margin-bottom: var(--space-3);
  }
  .hint {
    max-width: 75ch;
    margin: 0;
    font-size: var(--text-sm);
    color: var(--text-muted);
  }
  .problem {
    color: var(--bad);
  }
  .check {
    display: grid;
    gap: var(--space-2);
    max-width: 40rem;
  }
  .check p {
    margin: 0;
  }
  .problems {
    display: grid;
    gap: var(--space-1);
    margin: 0;
    padding-left: var(--space-5);
  }
  .problems li {
    display: grid;
  }
</style>
