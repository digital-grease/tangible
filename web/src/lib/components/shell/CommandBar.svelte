<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // The command bar (docs/plans/ui-overhaul.md, decision 8): Ctrl+K (or
  // Cmd+K) from anywhere, or its button, to go to a page, start one of the
  // three global actions, or open an artifact found by name.
  //
  // An extra, never the only way: everything here is also an ordinary link.
  // A modal dialog holding a combobox and its list box, as the WAI-ARIA
  // combobox pattern has them: the arrow keys move the active option, Enter
  // takes it, Escape closes and returns focus to the button.
  import { goto } from '$app/navigation';
  import { tick } from 'svelte';
  import Icon from '$lib/components/ui/Icon.svelte';
  import type { IconName } from '$lib/components/ui/icons';
  import { commandOptions, type Command } from '$lib/commands';
  import { loadLibrary } from '$lib/library';
  import type { NavGroup } from '$lib/nav';

  let { groups }: { groups: readonly NavGroup[] } = $props();

  const uid = $props.id();
  let dialog = $state<HTMLDialogElement | null>(null);
  let trigger = $state<HTMLButtonElement | null>(null);
  let input = $state<HTMLInputElement | null>(null);
  let query = $state('');
  let active = $state(0);
  let found = $state<Command[]>([]);
  let searching = $state(false);
  let searchedFor = '';
  let soon: ReturnType<typeof setTimeout> | undefined;

  const options = $derived(commandOptions(groups, query, found));
  const sections = $derived(
    (['Actions', 'Go to', 'Library'] as const)
      .map((section) => ({
        section,
        items: options.filter((option) => option.section === section),
      }))
      .filter((group) => group.items.length > 0),
  );

  async function show() {
    query = '';
    found = [];
    active = 0;
    dialog?.showModal();
    await tick();
    input?.focus();
  }

  function hide() {
    dialog?.close();
  }

  function closed() {
    clearTimeout(soon);
    trigger?.focus();
  }

  function search(text: string) {
    clearTimeout(soon);
    const name = text.trim();
    if (name.length < 2) {
      found = [];
      searching = false;
      return;
    }
    searching = true;
    soon = setTimeout(async () => {
      searchedFor = name;
      const result = await loadLibrary(fetch, undefined, { name });
      if (searchedFor !== name) return;
      searching = false;
      found =
        result.kind === 'ready'
          ? result.data.items.slice(0, 6).map((item) => ({
              section: 'Library' as const,
              label: item.source_filename ?? item.id,
              detail: item.format,
              href: `/library/${item.id}`,
              icon: 'disc' as IconName,
            }))
          : [];
    }, 200);
  }

  function oninput() {
    active = 0;
    search(query);
  }

  async function choose(option: Command | undefined) {
    if (!option) return;
    hide();
    await goto(option.href);
  }

  function onkeydown(event: KeyboardEvent) {
    const count = options.length;
    if (event.key === 'ArrowDown' && count > 0) {
      event.preventDefault();
      active = (active + 1) % count;
    } else if (event.key === 'ArrowUp' && count > 0) {
      event.preventDefault();
      active = (active - 1 + count) % count;
    } else if (event.key === 'Enter') {
      event.preventDefault();
      void choose(options[active]);
    }
  }

  function onwindowkeydown(event: KeyboardEvent) {
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'k') {
      event.preventDefault();
      if (dialog?.open) hide();
      else void show();
    }
  }

  function optionId(index: number): string {
    return `${uid}-option-${index}`;
  }
</script>

<svelte:window onkeydown={onwindowkeydown} />

<button
  type="button"
  class="trigger"
  bind:this={trigger}
  onclick={show}
  aria-haspopup="dialog"
  aria-label="Search or run a command (Ctrl+K)"
>
  <Icon name="search" />
  <span class="words" aria-hidden="true">Search or run a command</span>
  <kbd aria-hidden="true">Ctrl K</kbd>
</button>

<dialog bind:this={dialog} class="palette" aria-label="Command bar" onclose={closed}>
  <div class="query">
    <Icon name="search" />
    <input
      bind:this={input}
      bind:value={query}
      {oninput}
      {onkeydown}
      type="text"
      role="combobox"
      aria-label="Search or run a command"
      aria-expanded="true"
      aria-controls="{uid}-options"
      aria-autocomplete="list"
      aria-activedescendant={options.length > 0 ? optionId(active) : undefined}
      placeholder="Type a page, an action, or part of an artifact's name"
      autocomplete="off"
      spellcheck="false"
    />
    <button type="button" class="close" onclick={hide} aria-label="Close">
      <Icon name="close" />
    </button>
  </div>
  <div id="{uid}-options" role="listbox" aria-label="Results" class="options">
    {#each sections as group (group.section)}
      <div role="group" aria-labelledby="{uid}-{group.section}">
        <div class="section" id="{uid}-{group.section}" role="presentation">{group.section}</div>
        {#each group.items as option (option.href + option.label)}
          {@const index = options.indexOf(option)}
          <!-- Chosen with the keyboard through the combobox; a click works
               too. Options are not tab stops, as the pattern has them. -->
          <!-- svelte-ignore a11y_click_events_have_key_events -->
          <div
            id={optionId(index)}
            role="option"
            tabindex="-1"
            aria-selected={index === active}
            class="option"
            class:active={index === active}
            onclick={() => choose(option)}
            onmousemove={() => (active = index)}
          >
            <Icon name={option.icon} />
            <span class="label">{option.label}</span>
            {#if option.detail}<span class="detail">{option.detail}</span>{/if}
          </div>
        {/each}
      </div>
    {/each}
    {#if options.length === 0}
      <p class="nothing">{searching ? 'Searching the library…' : 'Nothing matches.'}</p>
    {/if}
  </div>
  <p class="hint" aria-hidden="true">↑ ↓ to move · Enter to choose · Esc to close</p>
  <p class="visually-hidden" role="status">
    {searching ? 'Searching the library' : `${options.length} results`}
  </p>
</dialog>

<style>
  .trigger {
    flex: 1;
    max-width: 26rem;
    justify-content: flex-start;
    gap: var(--space-2);
    color: var(--text-muted);
    font-weight: var(--weight-regular);
    background: var(--bg);
  }
  .trigger kbd {
    margin-left: auto;
    font-size: var(--text-xs);
    color: var(--text-muted);
  }
  .palette {
    width: min(38rem, calc(100vw - 2rem));
    margin: 12vh auto auto;
    padding: 0;
    border: 1px solid var(--line);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-2);
    background: var(--surface);
    color: var(--text);
  }
  .query {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-3) var(--space-4);
    border-bottom: 1px solid var(--line);
  }
  .query input {
    flex: 1;
    border: 0;
    background: none;
    font-size: var(--text-md);
    min-height: 2rem;
    padding: 0;
  }
  .query input:focus-visible {
    outline: none;
  }
  .palette:has(input:focus-visible) .query {
    box-shadow: inset 0 -2px 0 var(--accent);
  }
  .close {
    border: 0;
    background: none;
    min-height: 0;
    padding: var(--space-1);
    color: var(--text-muted);
  }
  .options {
    max-height: min(24rem, 60vh);
    overflow-y: auto;
    padding: var(--space-2);
  }
  .section {
    padding: var(--space-2) var(--space-2) var(--space-1);
    font-size: var(--text-xs);
    font-weight: var(--weight-strong);
    color: var(--text-muted);
    letter-spacing: 0.06em;
    text-transform: uppercase;
  }
  .option {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-2);
    border-radius: var(--radius);
    cursor: pointer;
  }
  .option.active {
    background: color-mix(in srgb, var(--accent) 14%, transparent);
    box-shadow: inset 3px 0 0 var(--accent);
  }
  .label {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .detail {
    margin-left: auto;
    color: var(--text-muted);
    font-size: var(--text-sm);
    white-space: nowrap;
  }
  .nothing,
  .hint {
    margin: 0;
    padding: var(--space-2) var(--space-4);
    color: var(--text-muted);
    font-size: var(--text-sm);
  }
  .hint {
    border-top: 1px solid var(--line);
    background: var(--surface-2);
  }
  @media (max-width: 1199px) {
    .trigger .words {
      display: none;
    }
    .trigger {
      flex: 0 0 auto;
    }
  }
</style>
