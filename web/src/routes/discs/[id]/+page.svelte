<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import Callout from '$lib/components/ui/Callout.svelte';
  import { shortId } from '$lib/ids';
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import Panel from '$lib/components/ui/Panel.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import Table from '$lib/components/ui/Table.svelte';
  import { onMount } from 'svelte';
  import { page } from '$app/state';
  import {
    SETTABLE_CONDITIONS,
    conditionLabel,
    conditionTone,
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

<div aria-busy={view.kind === 'loading'}>
  {#if view.kind === 'loading'}
    <p class="quiet">Loading disc…</p>
  {:else if view.kind === 'error'}
    <p><a href="/discs">Physical copies</a></p>
    <ErrorPanel
      problem={view.problem}
      area="other"
      endpoint="GET /api/v1/physical-copies/{'{id}'}"
      heading="Could not load this disc"
      level={1}
    />
  {:else if copy}
    {@const name = copy.label ?? `Disc ${shortId(copy.id)}`}
    <PageHeader
      title={name}
      crumbs={[{ label: 'Physical copies', href: '/discs' }, { label: name }]}
    >
      {#snippet badges()}
        <span id="condition-heading">
          <StatusMark tone={conditionTone(copy.status)} label={conditionLabel(copy.status)} />
        </span>
      {/snippet}
    </PageHeader>

    {#if copy.should_be_destroyed}
      <Callout tone="bad"
        ><p>
          This disc does not hold what was intended, or is no longer trusted to. Destroy it rather
          than returning it to a shelf: an untracked bad disc gets found later and believed.
        </p></Callout
      >
    {/if}
    {#if copy.is_settled}
      <Callout
        ><p>
          This disc has been destroyed. Its record remains so the burn history stays true.
        </p></Callout
      >
    {/if}

    {#if problem}
      <ErrorPanel {problem} area="other" refusal />
    {/if}

    <div class="columns">
      <div class="stack">
        <Panel title="How it was made">
          <dl class="facts">
            <div>
              <dt>Artifact</dt>
              <dd><a href={`/library/${copy.artifact_id}`}><code>{copy.artifact_id}</code></a></dd>
            </div>
            <div>
              <dt>Burn attempt</dt>
              <dd><code>{copy.burn_attempt_id}</code></dd>
            </div>
            <div>
              <dt>Medium</dt>
              <dd>
                {copy.media_profile}{copy.manufacturer_id ? ` (${copy.manufacturer_id})` : ''}
              </dd>
            </div>
            <div>
              <dt>Verification at burn</dt>
              <dd>
                {copy.verification_level.replace(/_/g, ' ')}: {copy.verification_result.replace(
                  /_/g,
                  ' ',
                )}
              </dd>
            </div>
            <div>
              <dt>Burned</dt>
              <dd>
                <time datetime={copy.created_at}>{new Date(copy.created_at).toLocaleString()}</time>
              </dd>
            </div>
          </dl>
          <p class="quiet note">
            How this disc was made is history and cannot be edited. What condition it is in, and
            where it lives, can be.
          </p>
        </Panel>

        <Panel title="Check history" aside={String(copy.checks.length)} flush>
          {#if copy.checks.length === 0}
            <p class="quiet padded">This disc has not been checked since it was burned.</p>
          {:else}
            <Table caption="Checks recorded for this disc">
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
                    <th scope="row">
                      <time datetime={entry.checked_at}
                        >{new Date(entry.checked_at).toLocaleString()}</time
                      >
                    </th>
                    <td>{methodLabel(entry.method)}</td>
                    <td>{entry.result.replace(/_/g, ' ')}</td>
                    <td>{entry.checked_with ?? '·'}</td>
                    <td>{entry.notes ?? '·'}</td>
                  </tr>
                {/each}
              </tbody>
            </Table>
          {/if}
        </Panel>
      </div>

      {#if isEditable(copy)}
        <div class="stack">
          <Panel title="Record what you know">
            <form class="form" onsubmit={save}>
              <label for="label">Label</label>
              <input id="label" bind:value={label} placeholder="Example Disc 1 of 2" />
              <label for="location">Where it is kept</label>
              <input id="location" bind:value={location} placeholder="Shelf B, sleeve 14" />
              <label for="notes">Notes</label>
              <textarea id="notes" bind:value={notes} rows="3"></textarea>
              <label for="condition">Condition</label>
              <select id="condition" bind:value={condition} aria-describedby="condition-hint">
                <option value="">Leave as it is</option>
                {#each SETTABLE_CONDITIONS as option (option)}
                  <option value={option}>{conditionLabel(option)}</option>
                {/each}
              </select>
              <!-- The strongest claim in the system stays the most expensive
                   one to make. -->
              <p class="hint" id="condition-hint">
                Verified is not in this list. It means a disc was read back and matched, so
                recording a check is the only way to write it.
              </p>
              <div><Button type="submit" variant="primary" disabled={busy}>Save</Button></div>
            </form>
          </Panel>

          <Panel title="Record a check">
            <form class="form" onsubmit={check}>
              <label for="method">What you did</label>
              <select id="method" bind:value={checkMethod}>
                <option value="full_sector_readback">Read the whole disc back</option>
                <option value="filesystem_compare">Compared the filesystem</option>
                <option value="tool_verify">Trusted the engine's own check</option>
                <option value="track_hash_compare">Compared track hashes</option>
              </select>
              <label for="result">What you found</label>
              <select id="result" bind:value={checkResult}>
                <option value="passed">It matched</option>
                <option value="failed">It did not match</option>
                <option value="partial">Partly readable</option>
                <option value="not_performed">Could not check</option>
              </select>
              <label for="drive">Drive used</label>
              <input id="drive" bind:value={checkedWith} placeholder="PIONEER BDR-212" />
              <label for="check-notes">Notes</label>
              <input id="check-notes" bind:value={checkNotes} />
              <div><Button type="submit" disabled={busy}>Record this check</Button></div>
            </form>
          </Panel>

          <Panel title="Destroy">
            {#if confirmingDestroy}
              <p>
                <strong>This cannot be undone.</strong> The record stays so the burn history remains true,
                but nothing further can be recorded against this disc.
              </p>
              <div class="buttons">
                <Button variant="danger" onclick={destroy} disabled={busy}>
                  Yes, this disc has been destroyed
                </Button>
                <Button onclick={() => (confirmingDestroy = false)} disabled={busy}>Keep it</Button>
              </div>
            {:else}
              <Button variant="danger" onclick={() => (confirmingDestroy = true)} disabled={busy}>
                Mark this disc destroyed
              </Button>
            {/if}
          </Panel>
        </div>
      {/if}
    </div>
  {/if}
</div>

<style>
  .columns {
    display: grid;
    grid-template-columns: minmax(0, 1.4fr) minmax(0, 1fr);
    gap: var(--space-4);
    align-items: start;
  }
  @media (max-width: 1099px) {
    .columns {
      grid-template-columns: minmax(0, 1fr);
    }
  }
  .stack {
    display: grid;
    gap: var(--space-4);
  }
  .facts {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(13rem, 1fr));
    gap: var(--space-3) var(--space-5);
    margin: 0;
  }
  dt {
    color: var(--text-muted);
    font-size: var(--text-sm);
  }
  dd {
    margin: 0;
    font-weight: var(--weight-medium);
    overflow-wrap: anywhere;
  }
  .form {
    display: grid;
    gap: var(--space-1);
  }
  .form label {
    margin-top: var(--space-2);
    font-size: var(--text-sm);
  }
  .form label:first-child {
    margin-top: 0;
  }
  .form :global(button) {
    margin-top: var(--space-3);
  }
  .hint {
    margin: 0;
    font-size: var(--text-sm);
    color: var(--text-muted);
  }
  .buttons {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
  }
  .note {
    margin: var(--space-3) 0 0;
    font-size: var(--text-sm);
  }
  .padded {
    padding: var(--space-3) var(--space-4);
  }
</style>
