<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  /**
   * An artifact's deletion (ADR 0020): what it is now, and asking for it or
   * keeping it. Deleting is never immediate: the artifact is pending for the
   * server's grace period, during which it can be kept, and an original
   * needs saying so besides.
   */
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
  let confirmed = $state(false);
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
    const result = await requestDeletion(artifactId, original && confirmed);
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
  <section aria-labelledby="deleted-heading" class="notice">
    <h2 id="deleted-heading">Deleted</h2>
    <p>
      <strong>Deleted {when(deletion.deleted_at)}.</strong> Its bytes are gone. This record is what
      is left: its files' names and hashes, where it came from, what was made of it, and the discs
      burned from it. Asked for by {deletion.requested_by} on {when(deletion.requested_at)}.
    </p>
  </section>
{:else if deletion?.state === 'pending'}
  <section aria-labelledby="pending-heading" class="notice">
    <h2 id="pending-heading">Pending deletion</h2>
    <p>
      <strong>Its bytes are removed after {when(deletion.delete_after)}</strong>, unless it is kept.
      Asked for by {deletion.requested_by} on {when(deletion.requested_at)}. Until then it can still
      be downloaded and checked, but nothing new is burned or made from it, and it is not sent to
      RomM.
    </p>
    <button type="button" disabled={working} onclick={keep}>
      {working ? 'Keeping…' : `Keep ${name}`}
    </button>
    <div role="alert">
      {#if problem}<p><strong>Not kept.</strong> {explain(problem)}</p>{/if}
    </div>
  </section>
{:else}
  <section aria-labelledby="delete-heading">
    <h2 id="delete-heading">Delete</h2>
    {#if !asking}
      <button
        type="button"
        onclick={() => {
          asking = true;
          confirmed = false;
          problem = null;
        }}>Delete this artifact…</button
      >
    {:else}
      <form class="danger" onsubmit={ask} aria-labelledby="delete-form-heading">
        <h3 id="delete-form-heading">Delete {name}</h3>
        <p>
          <strong>After the grace period its bytes are removed, and cannot be brought back</strong>
          except from a backup. Until then it is pending and can be kept. Its record stays: the files'
          names and hashes, where it came from, what was made of it, and its burns. A file it shares with
          another artifact stays for that one.
        </p>
        {#if original}
          <p>
            <label>
              <input type="checkbox" bind:checked={confirmed} />
              I understand this is an original, which may be the only copy there is
            </label>
          </p>
        {/if}
        <div role="alert">
          {#if problem}<p><strong>Not deleted.</strong> {explain(problem)}</p>{/if}
        </div>
        <button type="submit" disabled={working || (original && !confirmed)}>
          {working ? 'Asking…' : `Delete ${name}`}
        </button>
        <button type="button" onclick={() => (asking = false)}>Keep {name}</button>
      </form>
    {/if}
  </section>
{/if}

<style>
  .danger {
    border: 3px solid currentColor;
    padding: 0 1rem 1rem;
    margin-top: 1rem;
  }
  .notice {
    border: 2px solid currentColor;
    padding: 0 1rem 1rem;
  }
</style>
