<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // The drive bay (docs/plans/ui-overhaul.md, "The shell"): each drive's
  // state on every page, from GET /api/v1/drives, kept current by the tab's
  // event stream and a slow reconcile.
  //
  // Folded to one line while every drive is idle, unless opened; anyone can
  // fold or open it, and the choice is remembered in this browser. A polite
  // live region says when a drive's state changes, never each percent.
  import { onMount } from 'svelte';
  import Disc from '$lib/components/ui/Disc.svelte';
  import Icon from '$lib/components/ui/Icon.svelte';
  import Progress from '$lib/components/ui/Progress.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import {
    announcement,
    anyBusy,
    bayEntry,
    baySummary,
    statesOf,
    type BayEntry,
  } from '$lib/drivebay';
  import { watch } from '$lib/events';
  import { loadDrives, type DriveView } from '$lib/workers';

  let { administrator = false }: { administrator?: boolean } = $props();

  const KEY = 'tangible.drive-bay';
  type Fold = 'auto' | 'open' | 'folded';

  const uid = $props.id();
  let drives = $state<DriveView[] | null>(null);
  let problem = $state<string | null>(null);
  let fold = $state<Fold>('auto');
  let said = $state('');
  let seen: Map<string, string> | null = null;

  // Host device paths are for administrators (UX.md 11).
  const entry = (drive: DriveView) => bayEntry(drive, { device: administrator });
  const entries = $derived<BayEntry[]>((drives ?? []).map(entry));
  const open = $derived(fold === 'open' || (fold === 'auto' && anyBusy(drives ?? [])));

  function remembered(): Fold {
    try {
      const value = localStorage.getItem(KEY);
      return value === 'open' || value === 'folded' ? value : 'auto';
    } catch {
      return 'auto';
    }
  }

  function toggle() {
    fold = open ? 'folded' : 'open';
    try {
      localStorage.setItem(KEY, fold);
    } catch {
      // Not remembered; it still applies to this page.
    }
  }

  async function refresh() {
    const result = await loadDrives();
    if (result.kind === 'error') {
      problem = result.problem.detail;
      return;
    }
    problem = null;
    drives = result.data.items;
    const now = drives.map(entry);
    said = announcement(seen, now);
    seen = statesOf(now);
  }

  onMount(() => {
    fold = remembered();
    void refresh();
    return watch(['burns', 'rips', 'erasures', 'workers'], refresh);
  });

  function percent(value: number | null): string {
    return value === null ? 'not reported yet' : `${Math.round(value * 100)}%`;
  }
</script>

<section class="bay" aria-label="Drives">
  <!-- Polite, and only on a change of state (lib/drivebay.ts). -->
  <p class="visually-hidden" role="status">{said}</p>

  {#if problem}
    <p class="line"><StatusMark tone="warn" label="The drives could not be read" /> {problem}</p>
  {:else if drives && drives.length === 0}
    <p class="line">
      <span class="muted">No drives yet.</span>
      {#if administrator}<a href="/workers">Add a burn worker</a>{/if}
    </p>
  {:else if drives}
    <div class="line">
      <button
        type="button"
        class="fold"
        aria-expanded={open}
        aria-controls="{uid}-drives"
        onclick={toggle}
      >
        <Icon name={open ? 'chevron-down' : 'chevron-right'} size={14} />
        Drives
      </button>
      {#if !open}<span class="muted">{baySummary(drives)}</span>{/if}
    </div>
    <ul id="{uid}-drives" class="drives" hidden={!open}>
      {#each entries as entry (entry.id)}
        <li class="drive" class:at-work={entry.atWork}>
          <Disc
            mode={entry.disc.mode}
            shape={entry.disc.shape}
            family={entry.disc.family}
            capacity={entry.disc.capacity}
            progress={entry.disc.progress}
            size={44}
            hole="var(--bg)"
          />
          <div class="text">
            <div class="top">
              <span class="name">{entry.name}</span>
              <span class="where">{entry.where}</span>
            </div>
            <StatusMark tone={entry.tone} label={entry.state} />
            {#if entry.title}
              <div class="title">
                {#if entry.href}<a href={entry.href}>{entry.title}</a>{:else}{entry.title}{/if}
              </div>
            {/if}
            {#if entry.showsProgress}
              <div class="progress">
                <Progress value={entry.progress} label="{entry.name}: {entry.state}" tone="laser" />
                <span class="num">{percent(entry.progress)}</span>
              </div>
            {/if}
          </div>
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  .bay {
    border-bottom: 1px solid var(--line);
    background: var(--surface);
    padding: var(--space-2) var(--space-6);
  }
  .line {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    margin: 0;
    min-height: 1.75rem;
    font-size: var(--text-sm);
  }
  .muted {
    color: var(--text-muted);
  }
  .fold {
    min-height: 0;
    padding: var(--space-1) var(--space-2);
    margin-left: calc(-1 * var(--space-2));
    border: 0;
    background: none;
    color: var(--text-muted);
    font-size: var(--text-xs);
    font-weight: var(--weight-strong);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    gap: var(--space-1);
  }
  .fold:hover {
    background: var(--surface-2);
    color: var(--text);
  }
  .drives {
    list-style: none;
    margin: var(--space-1) 0 var(--space-2);
    padding: 0;
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(17rem, 1fr));
    gap: var(--space-2) var(--space-4);
  }
  .drives[hidden] {
    display: none;
  }
  .drive {
    display: flex;
    gap: var(--space-3);
    align-items: flex-start;
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--line);
    border-radius: var(--radius-lg);
    background: var(--bg);
    min-width: 0;
  }
  .drive.at-work {
    border-color: color-mix(in srgb, var(--laser) 45%, var(--line));
  }
  .text {
    display: grid;
    gap: 0.125rem;
    min-width: 0;
    flex: 1;
  }
  .top {
    display: grid;
    min-width: 0;
  }
  .name {
    font-weight: var(--weight-strong);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .where {
    color: var(--text-muted);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .text :global(.mark) {
    font-size: var(--text-sm);
  }
  .title {
    font-size: var(--text-sm);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .progress {
    display: grid;
    grid-template-columns: 1fr auto;
    gap: var(--space-2);
    align-items: center;
    font-size: var(--text-xs);
    color: var(--text-muted);
    margin-top: var(--space-1);
  }
  .drive :global(svg.disc) {
    margin-top: 0.125rem;
  }
</style>
