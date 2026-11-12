<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // An edition's RomM export: its platform, the switch, how the export
  // stands, and, once RomM's API is connected, what RomM made of it, with a
  // link to the game's page there, and how the last scan of its platform
  // went. A panel of its own under the catalog's columns.
  import { onDestroy } from 'svelte';
  import Button from './ui/Button.svelte';
  import Checkbox from './ui/Checkbox.svelte';
  import Field from './ui/Field.svelte';
  import Panel from './ui/Panel.svelte';
  import StatusMark from './ui/StatusMark.svelte';
  import {
    inRommLabel,
    inRommTone,
    loadEditionRomm,
    loadRommSettings,
    platformOptionLabel,
    rommApiInUse,
    rommStateLabel,
    rommStateTone,
    saveEditionRomm,
    scanLabel,
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
   * Check back after a change: the exporter works in the background and
   * records how it went, usually within seconds. With RomM's API in use,
   * keep checking, less often, until RomM has been asked about the game
   * again, which waits for RomM's scan.
   */
  function follow(id: string, before: Snapshot, tries: number) {
    clearTimeout(timer);
    if (tries <= 0) return;
    timer = setTimeout(async () => {
      const r = await loadEditionRomm(id);
      if (r.kind === 'ok') {
        romm = r.data;
        const exported = r.data.checked_at !== before.exported;
        const looked = r.data.in_romm?.checked_at !== before.looked && !r.data.scan_owed;
        if (exported && (!rommApiInUse(settings) || looked)) return;
      }
      follow(id, before, tries - 1);
    }, 3000);
  }

  type Snapshot = { exported: string | null | undefined; looked: string | null | undefined };

  async function save(event: SubmitEvent) {
    event.preventDefault();
    if (saving) return;
    saving = true;
    problem = null;
    const before = { exported: romm?.checked_at, looked: romm?.in_romm?.checked_at };
    const result = await saveEditionRomm(editionId, platform || null, exportToRomm);
    saving = false;
    if (result.kind === 'error') {
      problem = result.problem;
      return;
    }
    romm = result.data;
    // Two minutes: long enough for a quick scan of a platform or two.
    follow(editionId, before, 40);
  }

  function when(value: string | null | undefined): string {
    return value ? new Date(value).toLocaleString() : '';
  }
</script>

<Panel title="RomM">
  {#if settings && !settings.configured}
    <p class="quiet">Configure an export root before creating game exports.</p>
  {/if}
  <form class="form" onsubmit={save}>
    <Field label="Platform">
      {#snippet children({ id })}
        <select {id} bind:value={platform}>
          <option value="">Not set</option>
          {#each settings?.platforms ?? [] as option (option.slug)}
            <option value={option.slug}>{platformOptionLabel(option)}</option>
          {/each}
        </select>
      {/snippet}
    </Field>
    <Checkbox
      label="Show this game in RomM"
      hint="Its discs are placed in RomM's library as one game, kept in step as discs are added, and taken out again if you switch this off. Only files Tangible wrote are ever changed."
      bind:checked={exportToRomm}
      disabled={!platform || (settings !== null && !settings.configured)}
    />
    <div role="alert">
      {#if problem}<p class="problem"><strong>Not saved.</strong> {problem.detail}</p>{/if}
    </div>
    <div>
      <Button type="submit" disabled={saving}>{saving ? 'Saving…' : 'Save'}</Button>
    </div>
  </form>
  {#if settings?.platforms.some((option) => option.slug === platform && option.known_to_romm === false)}
    <p role="note" class="note">
      The connected RomM does not know this platform, so it would not recognise the folder.
    </p>
  {/if}
  {#if romm}
    <div aria-live="polite" class="status">
      <p>
        <StatusMark tone={rommStateTone(romm)} label={rommStateLabel(romm)} />
        {#if romm.folder && romm.state === 'current'}
          <span
            >as <code>{romm.folder}</code>, {romm.file_count} file{romm.file_count === 1
              ? ''
              : 's'}</span
          >
        {/if}
        {#if romm.detail}<span class="detail">{romm.detail}</span>{/if}
      </p>
      {#if rommApiInUse(settings)}
        {@const seen = inRommLabel(romm)}
        {#if seen && romm.in_romm}
          <p>
            <StatusMark tone={inRommTone(romm)} label={seen} />
            {#if romm.in_romm.url && romm.in_romm.state !== 'unseen'}
              <a href={romm.in_romm.url} target="_blank" rel="noopener noreferrer"
                >Open it in RomM</a
              >
            {/if}
            {#if romm.in_romm.detail}<span class="detail">{romm.in_romm.detail}</span>{/if}
            <span class="detail">Asked {when(romm.in_romm.checked_at)}</span>
          </p>
        {/if}
        {#if romm.scan_owed && romm.export}
          <p>Waiting for RomM to scan its platform's folder.</p>
        {/if}
        {#if romm.last_scan}
          <p class="detail">
            Last scan of {romm.last_scan.platforms.join(', ')}: {scanLabel(romm.last_scan)}, asked {when(
              romm.last_scan.requested_at,
            )}
          </p>
        {/if}
      {/if}
    </div>
  {/if}
</Panel>

<style>
  .form {
    display: grid;
    gap: var(--space-3);
    justify-items: start;
    max-width: 40rem;
  }
  .problem {
    margin: 0;
    color: var(--bad);
  }
  .note {
    margin: var(--space-3) 0 0;
    color: var(--warn);
  }
  .status {
    display: grid;
    gap: var(--space-2);
    margin-top: var(--space-4);
    padding-top: var(--space-3);
    border-top: 1px solid var(--line-2);
  }
  .status p {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-1) var(--space-2);
    align-items: baseline;
    margin: 0;
  }
  .detail {
    flex-basis: 100%;
    font-size: var(--text-sm);
    color: var(--text-muted);
  }
  p.detail {
    display: block;
  }
</style>
