<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // The library (UX.md 4; docs/plans/ui-overhaul.md, "Pages"): every
  // artifact, newest identifiers last, filtered by the catalogue's filters
  // and part of a file's name, each row with a drawing of the disc it
  // describes (from its disc shape; a plain disc where that is not known).
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import Disc from '$lib/components/ui/Disc.svelte';
  import EmptyState from '$lib/components/ui/EmptyState.svelte';
  import Icon from '$lib/components/ui/Icon.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import Panel from '$lib/components/ui/Panel.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import Table from '$lib/components/ui/Table.svelte';
  import { bestMatchLabel } from '$lib/identification';
  import { apiFetch } from '$lib/session';
  import { onMount } from 'svelte';
  import {
    deletionLabel,
    filtersNothing,
    formatBytes,
    formatLabel,
    integrityShort,
    integrityTone,
    isOriginal,
    loadLibrary,
    originLabel,
    validationLabel,
    validationTone,
    type ArtifactSummaryPage,
    type LibraryFilter,
    type LoadState,
  } from '$lib/library';

  let view = $state<LoadState<ArtifactSummaryPage>>({ kind: 'loading' });
  let loadingMore = $state(false);

  // What the form says, and what the list shown was loaded with: "Load
  // more" keeps the latter, whatever has been changed in the form since.
  let name = $state('');
  let format = $state('');
  let origin = $state('');
  let validation = $state('');
  let integrity = $state('');
  let linked = $state('');
  let deleted = $state(false);
  let shown = $state<LibraryFilter>({});

  function chosen(): LibraryFilter {
    return {
      name,
      format,
      origin,
      validation_state: validation,
      integrity_state: integrity,
      linked: linked === '' ? null : linked === 'true',
      deleted,
    };
  }

  async function show(filter: LibraryFilter) {
    shown = filter;
    view = { kind: 'loading' };
    view = await loadLibrary(apiFetch, undefined, filter);
  }

  onMount(() => show({}));

  function filter(event: SubmitEvent) {
    event.preventDefault();
    void show(chosen());
  }

  function clear() {
    name = format = origin = validation = integrity = linked = '';
    deleted = false;
    void show({});
  }

  async function loadMore() {
    if (view.kind !== 'ready' || !view.data.next_cursor || loadingMore) return;
    loadingMore = true;
    const next = await loadLibrary(apiFetch, view.data.next_cursor, shown);
    if (next.kind === 'ready' && view.kind === 'ready') {
      // Append rather than replace: paging through a library should not lose
      // what the operator has already scrolled past.
      view = {
        kind: 'ready',
        data: {
          items: [...view.data.items, ...next.data.items],
          next_cursor: next.data.next_cursor,
        },
      };
    } else if (next.kind === 'error') {
      view = next;
    }
    loadingMore = false;
  }
</script>

<svelte:head><title>Library — Tangible</title></svelte:head>

<PageHeader title="Library" description="Every disc image Tangible keeps, as it arrived.">
  {#snippet actions()}
    <Button variant="primary" icon="import" href="/imports">Import</Button>
  {/snippet}
</PageHeader>

<form class="filters" onsubmit={filter} aria-label="Filter the library">
  <label class="name">
    Name
    <span class="search">
      <Icon name="search" />
      <input
        type="search"
        bind:value={name}
        placeholder="Part of a file's name"
        autocomplete="off"
        spellcheck="false"
      />
    </span>
  </label>
  <label>
    Format
    <select bind:value={format}>
      <option value="">Any</option>
      <option value="iso">ISO</option>
      <option value="cue_bin">CUE/BIN</option>
      <option value="toc_bin">TOC/BIN</option>
      <option value="ccd_img_sub">CloneCD (CCD/IMG/SUB)</option>
      <option value="mds_mdf">Alcohol 120% (MDS/MDF)</option>
      <option value="chd">CHD</option>
      <option value="video_ts_directory">VIDEO_TS</option>
      <option value="unknown">Unrecognised</option>
    </select>
  </label>
  <label>
    Where it came from
    <select bind:value={origin}>
      <option value="">Anywhere</option>
      <option value="imported_original">Imported</option>
      <option value="derived">Made here from another</option>
      <option value="physical_dump">Ripped from a disc</option>
    </select>
  </label>
  <label>
    Validation
    <select bind:value={validation}>
      <option value="">Any</option>
      <option value="valid">Valid</option>
      <option value="valid_with_warnings">Valid, with warnings</option>
      <option value="invalid">Invalid</option>
      <option value="pending">Not validated yet</option>
      <option value="unsupported">Not validated here</option>
    </select>
  </label>
  <label>
    Stored bytes
    <select bind:value={integrity}>
      <option value="">Any</option>
      <option value="intact">Checked and intact</option>
      <option value="damaged">Damaged</option>
      <option value="unchecked">Not checked yet</option>
    </select>
  </label>
  <label>
    Disc
    <select bind:value={linked}>
      <option value="">Linked or not</option>
      <option value="true">Linked to a disc</option>
      <option value="false">Not linked to any disc</option>
    </select>
  </label>
  <label class="check">
    <input type="checkbox" bind:checked={deleted} />
    Only deleted artifacts
  </label>
  <div class="buttons">
    <Button type="submit" variant="primary">Show</Button>
    {#if !filtersNothing(shown)}
      <Button variant="quiet" onclick={clear}>Show everything</Button>
    {/if}
  </div>
</form>

<!-- One live region for the whole view, so view changes are announced once
     rather than per row. -->
<div aria-live="polite" aria-busy={view.kind === 'loading'}>
  {#if view.kind === 'loading'}
    <p class="quiet">Loading artifacts…</p>
  {:else if view.kind === 'empty' && !filtersNothing(shown)}
    <p class="quiet">Nothing in the library matches these filters.</p>
  {:else if view.kind === 'empty'}
    <EmptyState title="No artifacts yet" icon="library">
      <p>Import an authorized disc image to create your first artifact.</p>
      {#snippet action()}
        <Button variant="primary" href="/imports" icon="import">Import one</Button>
      {/snippet}
    </EmptyState>
  {:else if view.kind === 'error'}
    <ErrorPanel
      problem={view.problem}
      area="library"
      endpoint="GET /api/v1/artifacts"
      heading="Could not load the library"
    />
  {:else}
    <Panel
      title="Artifacts"
      aside={`${view.data.items.length}${view.data.next_cursor ? '+' : ''} shown`}
      flush
    >
      <Table caption="Artifacts in the library">
        <thead>
          <tr>
            <th scope="col">Artifact</th>
            <th scope="col">Format</th>
            <th scope="col" class="num">Size</th>
            <th scope="col" class="num">Files</th>
            <th scope="col">Validation</th>
            <th scope="col">Stored bytes</th>
            <th scope="col">Identified as</th>
          </tr>
        </thead>
        <tbody>
          {#each view.data.items as artifact (artifact.id)}
            <tr>
              <th scope="row">
                <div class="artifact">
                  <Disc shape={artifact.disc_shape} size={28} hole="var(--surface)" />
                  <div class="names">
                    <a href={`/library/${artifact.id}`}>{artifact.source_filename ?? artifact.id}</a
                    >
                    <span class="origin">
                      {#if isOriginal(artifact.origin)}<Icon name="lock" size={12} />{/if}
                      {originLabel(artifact.origin)}
                    </span>
                  </div>
                </div>
              </th>
              <td>{formatLabel(artifact.format)}</td>
              <td class="num">{formatBytes(artifact.total_bytes)}</td>
              <td class="num">{artifact.component_count}</td>
              <td>
                <StatusMark
                  tone={validationTone(artifact.validation_state)}
                  label={validationLabel(artifact.validation_state)}
                  pill
                />
                {#if deletionLabel(artifact.deletion)}
                  <div class="also">
                    <StatusMark tone="warn" label={deletionLabel(artifact.deletion) ?? ''} />
                  </div>
                {/if}
              </td>
              <td>
                <!-- Damage found by an integrity check outranks how the bytes
                     looked when they arrived, so it has its own column. -->
                <StatusMark
                  tone={integrityTone(artifact.integrity_state)}
                  label={integrityShort(artifact.integrity_state)}
                  pill
                />
              </td>
              <td>
                {#if artifact.best_match}
                  <!-- The DAT is named in the title as well as the cell, so the
                       column stays short. -->
                  <span title={`In ${artifact.best_match.dat_name}`}
                    >{bestMatchLabel(artifact.best_match)}</span
                  >
                {:else}
                  <span class="quiet">Not identified</span>
                {/if}
              </td>
            </tr>
          {/each}
        </tbody>
      </Table>
    </Panel>

    {#if view.data.next_cursor}
      <div class="more">
        <Button onclick={loadMore} disabled={loadingMore}>
          {loadingMore ? 'Loading…' : 'Load more'}
        </Button>
      </div>
    {/if}
  {/if}
</div>

<style>
  .filters {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-3) var(--space-4);
    align-items: end;
    margin-bottom: var(--space-5);
    padding: var(--space-3) var(--space-4);
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--radius-lg);
  }
  .filters label {
    display: grid;
    gap: var(--space-1);
    font-size: var(--text-sm);
    font-weight: var(--weight-medium);
    color: var(--text-muted);
  }
  .filters select {
    color: var(--text);
  }
  .name {
    flex: 1 1 16rem;
  }
  .search {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: 0 var(--space-2);
    background: var(--surface);
    border: 1px solid var(--line-strong);
    border-radius: var(--radius);
    color: var(--text-muted);
  }
  .search:focus-within {
    outline: var(--focus-width) solid var(--accent);
    outline-offset: var(--focus-offset);
  }
  .search input {
    flex: 1;
    min-width: 0;
    border: 0;
    padding-inline: 0;
    background: none;
    color: var(--text);
  }
  .search input:focus-visible {
    outline: none;
  }
  .filters .check {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-height: 2rem;
    color: var(--text);
  }
  .buttons {
    display: flex;
    gap: var(--space-2);
  }
  .artifact {
    display: flex;
    gap: var(--space-3);
    align-items: center;
    min-width: 16rem;
  }
  .names {
    display: grid;
    min-width: 0;
  }
  .names a {
    font-weight: var(--weight-strong);
    overflow-wrap: anywhere;
  }
  .origin {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    color: var(--text-muted);
    font-size: var(--text-xs);
    font-weight: var(--weight-regular);
  }
  .also {
    margin-top: var(--space-1);
    font-size: var(--text-xs);
  }
  .more {
    margin-top: var(--space-4);
  }
</style>
