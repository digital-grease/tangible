<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { onDestroy } from 'svelte';
  import {
    loadEditionRomm,
    loadRommSettings,
    rommStateLabel,
    saveEditionRomm,
    type EditionRomm,
    type RommSettings,
  } from '$lib/romm';
  import type { Problem } from '$lib/session';

  let { editionId }: { editionId: string } = $props();

  let settings = $state<RommSettings | null>(null);
  let romm = $state<EditionRomm | null>(null);
  let platform = $state('');
  let exportToRomm = $state(false);
  let problem = $state<Problem | null>(null);
  let saving = $state(false);
  let timer: ReturnType<typeof setTimeout> | undefined;

  async function load(id: string) {
    problem = null;
    const [s, r] = await Promise.all([loadRommSettings(), loadEditionRomm(id)]);
    if (s.kind === 'ok') settings = s.data;
    if (r.kind === 'ok') {
      romm = r.data;
      platform = r.data.platform ?? '';
      exportToRomm = r.data.export;
    } else {
      problem = r.problem;
    }
  }

  $effect(() => {
    void load(editionId);
  });
  onDestroy(() => clearTimeout(timer));

  /**
   * Check back a few times after a change: the exporter works in the
   * background and records how it went, usually within seconds.
   */
  function follow(id: string, before: string | null | undefined, tries: number) {
    clearTimeout(timer);
    if (tries <= 0) return;
    timer = setTimeout(async () => {
      const r = await loadEditionRomm(id);
      if (r.kind === 'ok') {
        romm = r.data;
        if (r.data.checked_at !== before) return;
      }
      follow(id, before, tries - 1);
    }, 2000);
  }

  async function save(event: SubmitEvent) {
    event.preventDefault();
    if (saving) return;
    saving = true;
    problem = null;
    const before = romm?.checked_at;
    const result = await saveEditionRomm(editionId, platform || null, exportToRomm);
    saving = false;
    if (result.kind === 'error') {
      problem = result.problem;
      return;
    }
    romm = result.data;
    follow(editionId, before, 10);
  }
</script>

<section aria-labelledby="romm-heading">
  <h3 id="romm-heading">RomM</h3>
  {#if settings && !settings.configured}
    <p>Configure an export root before creating game exports.</p>
  {/if}
  <form onsubmit={save}>
    <p>
      <label for="romm-platform">Platform</label><br />
      <select id="romm-platform" bind:value={platform}>
        <option value="">Not set</option>
        {#each settings?.platforms ?? [] as option (option.slug)}
          <option value={option.slug}>{option.name}</option>
        {/each}
      </select>
    </p>
    <p>
      <label>
        <input
          type="checkbox"
          bind:checked={exportToRomm}
          disabled={!platform || (settings !== null && !settings.configured)}
        />
        Show this game in RomM
      </label>
      <br />
      <small
        >Its discs are placed in RomM's library as one game, kept in step as discs are added, and
        taken out again if you switch this off. Only files Tangible wrote are ever changed.</small
      >
    </p>
    <div role="alert">
      {#if problem}<p><strong>Not saved.</strong> {problem.detail}</p>{/if}
    </div>
    <button type="submit" disabled={saving}>{saving ? 'Saving…' : 'Save'}</button>
  </form>
  {#if romm}
    <p aria-live="polite">
      <strong>{rommStateLabel(romm)}</strong>
      {#if romm.folder && romm.state === 'current'}
        as <code>{romm.folder}</code>, {romm.file_count} file{romm.file_count === 1 ? '' : 's'}
      {/if}
      {#if romm.detail}<br />{romm.detail}{/if}
    </p>
  {/if}
</section>
