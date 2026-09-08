<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { onMount } from 'svelte';
  import { page } from '$app/state';
  import {
    SETTABLE_CONDITIONS,
    conditionLabel,
    conditionMark,
    isEditable,
    loadDisc,
    markDestroyed,
    methodLabel,
    recordCheck,
    updateDisc,
    type PhysicalCopyDetail,
  } from '$lib/discs';
  import type { LoadState, Problem } from '$lib/library';

  let view = $state<LoadState<PhysicalCopyDetail>>({ kind: 'loading' });
  let problem = $state<Problem | null>(null);
  let busy = $state(false);

  let label = $state('');
  let location = $state('');
  let notes = $state('');
  let condition = $state('');

  let checkMethod = $state('full_sector_readback');
  let checkResult = $state('passed');
  let checkedWith = $state('');
  let checkNotes = $state('');

  /** Destructive and irreversible, so it takes a second, deliberate action. */
  let confirmingDestroy = $state(false);

  const id = $derived(page.params.id ?? '');
  const copy = $derived(view.kind === 'ready' ? view.data : null);

  onMount(async () => {
    view = await loadDisc(id);
    if (view.kind === 'ready') {
      label = view.data.label ?? '';
      location = view.data.storage_location ?? '';
      notes = view.data.notes ?? '';
    }
  });

  async function save(event: SubmitEvent) {
    event.preventDefault();
    if (busy) return;
    busy = true;
    problem = null;

    const result = await updateDisc(id, {
      label: label.trim() || null,
      storage_location: location.trim() || null,
      notes: notes.trim() || null,
      status: condition || null,
    });
    if (result.kind === 'error') {
      problem = result.problem;
    } else {
      view = result;
      condition = '';
    }
    busy = false;
  }

  async function check(event: SubmitEvent) {
    event.preventDefault();
    if (busy) return;
    busy = true;
    problem = null;

    const result = await recordCheck(id, {
      method: checkMethod,
      result: checkResult,
      checked_with: checkedWith.trim() || null,
      notes: checkNotes.trim() || null,
    });
    if (result.kind === 'error') {
      problem = result.problem;
    } else {
      view = result;
      checkedWith = '';
      checkNotes = '';
    }
    busy = false;
  }

  async function destroy() {
    busy = true;
    problem = null;
    const result = await markDestroyed(id);
    if (result.kind === 'error') {
      problem = result.problem;
    } else {
      view = result;
      confirmingDestroy = false;
    }
    busy = false;
  }
</script>

<svelte:head>
  <title>{copy ? `${copy.label ?? 'Disc'} — Tangible` : 'Disc — Tangible'}</title>
</svelte:head>

<p><a href="/discs">← Discs</a></p>

<div aria-busy={view.kind === 'loading'}>
  {#if view.kind === 'loading'}
    <p>Loading disc…</p>
  {:else if view.kind === 'error'}
    <h1>Could not load this disc</h1>
    <p>{view.problem.title}.</p>
    <details>
      <summary>Technical detail</summary>
      <p>{view.problem.detail}</p>
      <p>Error code: <code>{view.problem.code}</code></p>
    </details>
  {:else if copy}
    <h1>{copy.label ?? `Disc ${copy.id.slice(0, 8)}`}</h1>

    <section aria-labelledby="condition-heading" class="status">
      <h2 id="condition-heading">
        <span aria-hidden="true">{conditionMark(copy.status)}</span>
        {conditionLabel(copy.status)}
      </h2>
      {#if copy.should_be_destroyed}
        <p>
          This disc does not hold what was intended, or is no longer trusted to. Destroy it rather
          than returning it to a shelf: an untracked bad disc gets found later and believed.
        </p>
      {/if}
      {#if copy.is_settled}
        <p>This disc has been destroyed. Its record remains so the burn history stays true.</p>
      {/if}
    </section>

    {#if problem}
      <section class="refusal" aria-live="assertive" aria-labelledby="refusal-heading">
        <h2 id="refusal-heading">{problem.title}</h2>
        <p>{problem.detail}</p>
        <p>Error code: <code>{problem.code}</code></p>
      </section>
    {/if}

    <section aria-labelledby="details-heading">
      <h2 id="details-heading">How it was made</h2>
      <dl>
        <dt>Artifact</dt>
        <dd><a href={`/library/${copy.artifact_id}`}><code>{copy.artifact_id}</code></a></dd>
        <dt>Burn attempt</dt>
        <dd><code>{copy.burn_attempt_id}</code></dd>
        <dt>Medium</dt>
        <dd>{copy.media_profile}{copy.manufacturer_id ? ` (${copy.manufacturer_id})` : ''}</dd>
        <dt>Verification at burn</dt>
        <dd>
          {copy.verification_level.replace(/_/g, ' ')} — {copy.verification_result.replace(
            /_/g,
            ' ',
          )}
        </dd>
        <dt>Burned</dt>
        <dd>{new Date(copy.created_at).toLocaleString()}</dd>
      </dl>
      <p class="note">
        How this disc was made is history and cannot be edited. What condition it is in, and where
        it lives, can be.
      </p>
    </section>

    {#if isEditable(copy)}
      <section aria-labelledby="record-heading">
        <h2 id="record-heading">Record what you know</h2>
        <form onsubmit={save}>
          <p>
            <label for="label">Label</label><br />
            <input id="label" bind:value={label} placeholder="Example Disc 1 of 2" />
          </p>
          <p>
            <label for="location">Where it is kept</label><br />
            <input id="location" bind:value={location} placeholder="Shelf B, sleeve 14" />
          </p>
          <p>
            <label for="notes">Notes</label><br />
            <textarea id="notes" bind:value={notes} rows="3"></textarea>
          </p>
          <p>
            <label for="condition">Condition</label><br />
            <select id="condition" bind:value={condition}>
              <option value="">Leave as it is</option>
              {#each SETTABLE_CONDITIONS as option (option)}
                <option value={option}>{conditionLabel(option)}</option>
              {/each}
            </select>
            <br />
            <!-- The strongest claim in the system stays the most expensive one
                 to make. -->
            <small>
              Verified is not in this list. It means a disc was read back and matched, so recording
              a check is the only way to write it.
            </small>
          </p>
          <button type="submit" disabled={busy}>Save</button>
        </form>
      </section>

      <section aria-labelledby="check-heading">
        <h2 id="check-heading">Record a check</h2>
        <form onsubmit={check}>
          <p>
            <label for="method">What you did</label><br />
            <select id="method" bind:value={checkMethod}>
              <option value="full_sector_readback">Read the whole disc back</option>
              <option value="filesystem_compare">Compared the filesystem</option>
              <option value="tool_verify">Trusted the engine's own check</option>
              <option value="track_hash_compare">Compared track hashes</option>
            </select>
          </p>
          <p>
            <label for="result">What you found</label><br />
            <select id="result" bind:value={checkResult}>
              <option value="passed">It matched</option>
              <option value="failed">It did not match</option>
              <option value="partial">Partly readable</option>
              <option value="not_performed">Could not check</option>
            </select>
          </p>
          <p>
            <label for="drive">Drive used</label><br />
            <input id="drive" bind:value={checkedWith} placeholder="PIONEER BDR-212" />
          </p>
          <p>
            <label for="check-notes">Notes</label><br />
            <input id="check-notes" bind:value={checkNotes} />
          </p>
          <button type="submit" disabled={busy}>Record this check</button>
        </form>
      </section>

      <section aria-labelledby="destroy-heading">
        <h2 id="destroy-heading">Destroy</h2>
        {#if confirmingDestroy}
          <p>
            <strong>This cannot be undone.</strong> The record stays so the burn history remains true,
            but nothing further can be recorded against this disc.
          </p>
          <button type="button" onclick={destroy} disabled={busy}>
            Yes, this disc has been destroyed
          </button>
          <button type="button" onclick={() => (confirmingDestroy = false)} disabled={busy}>
            Keep it
          </button>
        {:else}
          <button type="button" onclick={() => (confirmingDestroy = true)} disabled={busy}>
            Mark this disc destroyed
          </button>
        {/if}
      </section>
    {/if}

    <section aria-labelledby="history-heading">
      <h2 id="history-heading">Check history</h2>
      {#if copy.checks.length === 0}
        <p>This disc has not been checked since it was burned.</p>
      {:else}
        <table>
          <caption class="visually-hidden">Checks recorded for this disc</caption>
          <thead>
            <tr>
              <th scope="col">When</th>
              <th scope="col">What was done</th>
              <th scope="col">Result</th>
              <th scope="col">Drive</th>
              <th scope="col">Notes</th>
            </tr>
          </thead>
          <tbody>
            {#each copy.checks as entry (entry.id)}
              <tr>
                <th scope="row">{new Date(entry.checked_at).toLocaleString()}</th>
                <td>{methodLabel(entry.method)}</td>
                <td>{entry.result.replace(/_/g, ' ')}</td>
                <td>{entry.checked_with ?? '—'}</td>
                <td>{entry.notes ?? '—'}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
    </section>
  {/if}
</div>

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
  table th,
  table td {
    text-align: left;
    padding: 0.5rem;
    border-bottom: 1px solid currentColor;
  }
  label {
    font-weight: 600;
  }
  input,
  select,
  textarea {
    padding: 0.4rem;
    min-width: min(28rem, 100%);
  }
  button {
    padding: 0.5rem 1rem;
    margin-right: 0.5rem;
  }
  .status {
    border: 1px solid currentColor;
    padding: 0.75rem 1rem;
  }
  .refusal {
    border: 2px solid currentColor;
    padding: 0.75rem 1rem;
  }
  .note {
    font-size: 0.9em;
  }
  small {
    display: inline-block;
    max-width: 40rem;
  }
  .visually-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
  }
</style>
