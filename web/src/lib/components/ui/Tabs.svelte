<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts" module>
  export interface Tab {
    id: string;
    label: string;
    /** How many things the tab holds, said after its label. */
    count?: number;
  }
</script>

<script lang="ts">
  // Tabs, as the WAI-ARIA tabs pattern has them: one tab stop for the list,
  // the arrow keys, Home and End to move between tabs (each selected as it is
  // reached), and the panel after it. `panel` renders the selected tab's
  // content, given its id.
  import type { Snippet } from 'svelte';

  let {
    tabs,
    label,
    selected = $bindable(''),
    panel,
  }: {
    tabs: readonly Tab[];
    /** What the tabs choose between, as the tab list's name. */
    label: string;
    selected?: string;
    panel: Snippet<[string]>;
  } = $props();

  const uid = $props.id();
  const current = $derived(
    tabs.some((tab) => tab.id === selected) ? selected : (tabs[0]?.id ?? ''),
  );
  const buttons: HTMLButtonElement[] = [];

  function choose(index: number) {
    const tab = tabs[index];
    if (!tab) return;
    selected = tab.id;
    buttons[index]?.focus();
  }

  function onkeydown(event: KeyboardEvent, index: number) {
    const last = tabs.length - 1;
    const next =
      event.key === 'ArrowRight'
        ? index === last
          ? 0
          : index + 1
        : event.key === 'ArrowLeft'
          ? index === 0
            ? last
            : index - 1
          : event.key === 'Home'
            ? 0
            : event.key === 'End'
              ? last
              : null;
    if (next === null) return;
    event.preventDefault();
    choose(next);
  }
</script>

<div class="tabs">
  <div class="list" role="tablist" aria-label={label}>
    {#each tabs as tab, index (tab.id)}
      <button
        bind:this={buttons[index]}
        type="button"
        role="tab"
        id="{uid}-tab-{tab.id}"
        aria-selected={current === tab.id}
        aria-controls="{uid}-panel"
        tabindex={current === tab.id ? 0 : -1}
        onclick={() => (selected = tab.id)}
        onkeydown={(event) => onkeydown(event, index)}
      >
        {tab.label}{#if tab.count !== undefined}<span class="count num">{tab.count}</span>{/if}
      </button>
    {/each}
  </div>
  <div class="panel" role="tabpanel" id="{uid}-panel" aria-labelledby="{uid}-tab-{current}">
    {@render panel(current)}
  </div>
</div>

<style>
  .list {
    display: flex;
    gap: var(--space-5);
    border-bottom: 1px solid var(--line);
    overflow-x: auto;
  }
  [role='tab'] {
    min-height: 0;
    padding: var(--space-2) 0;
    margin-bottom: -1px;
    border: 0;
    border-bottom: 2px solid transparent;
    border-radius: 0;
    background: none;
    color: var(--text-muted);
    font-weight: var(--weight-strong);
    white-space: nowrap;
  }
  [role='tab']:hover {
    background: none;
    color: var(--text);
  }
  [role='tab'][aria-selected='true'] {
    color: var(--text);
    border-bottom-color: var(--accent);
  }
  .count {
    margin-left: var(--space-1);
    color: var(--text-muted);
    font-weight: var(--weight-medium);
  }
  .panel {
    padding-top: var(--space-4);
  }
</style>
