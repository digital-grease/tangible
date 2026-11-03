<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import {
    changedFields,
    editDisc,
    editDiscSet,
    editEdition,
    editTitle,
    isStale,
    type DiscSetView,
    type DiscView,
    type EditionView,
    type TitleView,
  } from '$lib/catalog';
  import type { Problem } from '$lib/library';

  type Kind = 'title' | 'edition' | 'set' | 'disc';
  type Item = TitleView | EditionView | DiscSetView | DiscView;

  let {
    kind,
    item,
    onsaved,
    oncancel,
  }: { kind: Kind; item: Item; onsaved: () => void; oncancel: () => void } = $props();

  /** The form, from the item as it was read. */
  function initial(): Record<string, string | number | null> {
    switch (kind) {
      case 'title': {
        const title = item as TitleView;
        return {
          display_title: title.display_title,
          sort_title: title.sort_title,
          kind: title.kind,
          release_year: title.release_year ?? null,
        };
      }
      case 'edition': {
        const edition = item as EditionView;
        return {
          display_name: edition.display_name,
          publisher: edition.publisher ?? '',
          region: edition.region ?? '',
        };
      }
      case 'set': {
        const set = item as DiscSetView;
        return {
          name: set.name,
          set_kind: set.set_kind,
          disc_count_expected: set.disc_count_expected ?? null,
        };
      }
      case 'disc': {
        const disc = item as DiscView;
        return {
          sequence_number: disc.sequence_number,
          display_name: disc.display_name ?? '',
          media_family: disc.media_family,
          region: disc.region ?? '',
        };
      }
    }
  }

  // Taken from the item once, when the form opens: what it was read as is
  // what the edit is made against.
  let form = $state(initial());
  let saving = $state(false);
  let problem = $state<Problem | null>(null);

  const what = $derived(
    { title: 'title', edition: 'edition', set: 'disc set', disc: 'disc' }[kind],
  );

  async function save(event: SubmitEvent) {
    event.preventDefault();
    if (saving) return;
    const body = changedFields(item as unknown as Record<string, unknown>, form);
    if (Object.keys(body).length === 0) {
      oncancel();
      return;
    }
    saving = true;
    problem = null;
    const result =
      kind === 'title'
        ? await editTitle(item.id, item.version, body)
        : kind === 'edition'
          ? await editEdition(item.id, item.version, body)
          : kind === 'set'
            ? await editDiscSet(item.id, item.version, body)
            : await editDisc(item.id, item.version, body);
    saving = false;
    if (result.kind === 'error') {
      problem = result.problem;
      return;
    }
    onsaved();
  }
</script>

<form class="editor" onsubmit={save} aria-label={`Change this ${what}`}>
  {#if kind === 'title'}
    <label>Name <input bind:value={form.display_title} required maxlength="500" /></label>
    <label>Sorts as <input bind:value={form.sort_title} required maxlength="500" /></label>
    <label>
      Kind
      <select bind:value={form.kind}>
        <option value="unknown">Unspecified</option>
        <option value="movie">Film</option>
        <option value="television">Television</option>
        <option value="game">Game</option>
        <option value="software">Software</option>
        <option value="operating_system">Operating system</option>
        <option value="music">Music</option>
        <option value="data_archive">Data archive</option>
        <option value="training">Training</option>
        <option value="custom">Other</option>
      </select>
    </label>
    <label>
      Year
      <input type="number" bind:value={form.release_year} min="1870" max="2200" />
    </label>
  {:else if kind === 'edition'}
    <label>Name <input bind:value={form.display_name} required maxlength="500" /></label>
    <label>Publisher <input bind:value={form.publisher} maxlength="300" /></label>
    <label>Region <input bind:value={form.region} maxlength="100" /></label>
  {:else if kind === 'set'}
    <label>Name <input bind:value={form.name} required maxlength="500" /></label>
    <label>
      Shape
      <select bind:value={form.set_kind}>
        <option value="unknown">Unspecified</option>
        <option value="single_disc">One disc</option>
        <option value="multi_disc">Several discs</option>
        <option value="season_box">A season's box</option>
        <option value="installation_set">Installation discs</option>
        <option value="feature_and_supplements">Feature and supplements</option>
        <option value="compilation">Compilation</option>
      </select>
    </label>
    <label>
      Discs it shipped with
      <input type="number" bind:value={form.disc_count_expected} min="1" />
    </label>
  {:else}
    <label>
      Disc number <input type="number" bind:value={form.sequence_number} required min="1" />
    </label>
    <label>Name <input bind:value={form.display_name} maxlength="500" /></label>
    <label>
      Medium
      <select bind:value={form.media_family}>
        <option value="unknown">Unspecified</option>
        <option value="cd">CD</option>
        <option value="dvd">DVD</option>
        <option value="bluray">Blu-ray</option>
        <option value="uhd_bluray">UHD Blu-ray</option>
        <option value="gd_rom">GD-ROM</option>
        <option value="proprietary_optical">Another optical format</option>
      </select>
    </label>
    <label>Region <input bind:value={form.region} maxlength="100" /></label>
  {/if}

  <div role="alert">
    {#if problem}
      <p>
        <strong>Not saved.</strong>
        {#if isStale(problem)}
          Somebody changed this {what} since it was shown here, so nothing was saved over their change.
          Close this and open it again to see it.
        {:else if problem.code === 'FORBIDDEN'}
          Only an operator or an administrator can change the catalog.
        {:else}
          {problem.detail}
        {/if}
      </p>
    {/if}
  </div>

  <button type="submit" disabled={saving}>{saving ? 'Saving…' : `Save this ${what}`}</button>
  <button type="button" onclick={oncancel}>Close</button>
</form>

<style>
  .editor {
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem 1rem;
    align-items: end;
    border: 2px solid currentColor;
    padding: 0.75rem;
    margin: 0.5rem 0 1rem;
  }
  label {
    display: flex;
    flex-direction: column;
    font-weight: 600;
  }
  [role='alert'] {
    flex-basis: 100%;
  }
</style>
