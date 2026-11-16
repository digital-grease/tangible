<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  /**
   * An artifact's deletion (ADR 0020): what it is now, and asking for it or
   * keeping it. Deleting is never immediate: the artifact is pending for the
   * server's grace period, during which it can be kept. One question asks
   * for it; answering it is the confirmation an original needs
   * (`confirm_original`), so an original takes no more than a derivative.
   */
  import Button from '$lib/components/ui/Button.svelte';
  import Callout from '$lib/components/ui/Callout.svelte';
  import Confirm from '$lib/components/ui/Confirm.svelte';
  import {
    isOriginal,
    keepArtifact,
    requestDeletion,
    type ArtifactDeletionView,
    type Problem,
  } from '$lib/library';

  let {
    artifactId,
    name,
    origin,
    deletion,
    onchanged,
  }: {
    artifactId: string;
    name: string;
    origin: string;
    deletion: ArtifactDeletionView | null | undefined;
    onchanged: () => void | Promise<void>;
  } = $props();

  let asking = $state(false);
  let working = $state(false);
  let problem = $state<Problem | null>(null);

  const original = $derived(isOriginal(origin));

  function when(value: string | null | undefined): string {
    return value ? new Date(value).toLocaleString() : '';
  }

  /** A refusal in words: the server's own, which says what holds it. */
  function explain(refusal: Problem): string {
    return refusal.code === 'FORBIDDEN'
      ? 'Only an administrator can delete artifacts.'
      : refusal.detail;
  }

  async function ask(event: SubmitEvent) {
    event.preventDefault();
    if (working) return;
    working = true;
    problem = null;
    // Answering the question is the confirmation an original needs.
    const result = await requestDeletion(artifactId, original);
    working = false;
    if (result.kind === 'error') {
      problem = result.problem;
      return;
    }
    asking = false;
    await onchanged();
  }

  async function keep() {
    if (working) return;
    working = true;
    problem = null;
    const result = await keepArtifact(artifactId);
    working = false;
    if (result.kind === 'error') {
      problem = result.problem;
      return;
    }
    await onchanged();
  }
</script>

{#if deletion?.state === 'deleted'}
  <Callout tone="bad" title="Deleted" marked>
    <p>
      <strong>Deleted {when(deletion.deleted_at)}.</strong> Its bytes are gone. This record is what
      is left: its files' names and hashes, where it came from, what was made of it, and the discs
      burned from it. Asked for by {deletion.requested_by} on {when(deletion.requested_at)}.
    </p>
  </Callout>
{:else if deletion?.state === 'pending'}
  <Callout tone="warn" title="Pending deletion" marked>
    <p>
      <strong>Its bytes are removed after {when(deletion.delete_after)}</strong>, unless it is kept.
      Asked for by {deletion.requested_by} on {when(deletion.requested_at)}. Until then it can still
      be downloaded and checked, but nothing new is burned or made from it, and it is not sent to
      RomM.
    </p>
    <div>
      <Button variant="primary" disabled={working} onclick={keep}>
        {working ? 'Keeping…' : `Keep ${name}`}
      </Button>
    </div>
    <div role="alert">
      {#if problem}<p class="problem"><strong>Not kept.</strong> {explain(problem)}</p>{/if}
    </div>
  </Callout>
{:else}
  <section aria-labelledby="delete-heading">
    <h2 id="delete-heading">Delete</h2>
    {#if !asking}
      <Button
        variant="danger"
        onclick={() => {
          asking = true;
          problem = null;
        }}>Delete this artifact…</Button
      >
    {:else}
      <Confirm title={`Delete ${name}`} onsubmit={ask}>
        <p>
          Are you sure you want to delete this artifact? Once its grace period ends, its files are
          removed and can't be recovered.
        </p>
        <div role="alert">
          {#if problem}<p class="problem"><strong>Not deleted.</strong> {explain(problem)}</p>{/if}
        </div>
        {#snippet actions()}
          <Button type="submit" variant="danger" disabled={working}>
            {working ? 'Asking…' : `Delete ${name}`}
          </Button>
          <Button onclick={() => (asking = false)}>Keep {name}</Button>
        {/snippet}
      </Confirm>
    {/if}
  </section>
{/if}

<style>
  .problem {
    margin: 0;
    color: var(--bad);
  }
</style>
