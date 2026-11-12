<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts" module>
  import type { IconName } from './icons';

  export interface MenuItem {
    label: string;
    /** A page to go to; otherwise `onselect` runs. */
    href?: string;
    icon?: IconName;
    onselect?: () => void;
  }
</script>

<script lang="ts">
  // A button that shows a short list of links or actions, such as "New".
  //
  // A disclosure, not an ARIA menu: the items are ordinary links and buttons,
  // which is what the WAI-ARIA practices recommend for navigation. Opening it
  // moves focus to the first item; the arrow keys move between items; Escape
  // closes it and returns focus to the button; clicking elsewhere or tabbing
  // away closes it.
  import { tick } from 'svelte';
  import Button from './Button.svelte';
  import Icon from './Icon.svelte';

  let {
    label,
    icon,
    items,
    variant = 'secondary',
    align = 'start',
  }: {
    label: string;
    icon?: IconName;
    items: readonly MenuItem[];
    variant?: 'primary' | 'secondary' | 'quiet';
    /** Which edge of the button the list lines up with. */
    align?: 'start' | 'end';
  } = $props();

  const uid = $props.id();
  let open = $state(false);
  let root = $state<HTMLElement | null>(null);
  let trigger = $state<HTMLElement | null>(null);
  let list = $state<HTMLElement | null>(null);

  function entries(): HTMLElement[] {
    return list ? [...list.querySelectorAll<HTMLElement>('a, button')] : [];
  }

  async function toggle() {
    open = !open;
    if (open) {
      await tick();
      entries()[0]?.focus();
    }
  }

  function close(returnFocus: boolean) {
    open = false;
    if (returnFocus) trigger?.focus();
  }

  function onkeydown(event: KeyboardEvent) {
    if (!open) return;
    if (event.key === 'Escape') {
      event.preventDefault();
      close(true);
      return;
    }
    if (event.key !== 'ArrowDown' && event.key !== 'ArrowUp') return;
    const all = entries();
    if (all.length === 0) return;
    event.preventDefault();
    const at = all.indexOf(document.activeElement as HTMLElement);
    const step = event.key === 'ArrowDown' ? 1 : -1;
    all[(at + step + all.length) % all.length]?.focus();
  }

  function onfocusout(event: FocusEvent) {
    const next = event.relatedTarget as Node | null;
    if (open && next && root && !root.contains(next)) close(false);
  }

  function choose(item: MenuItem) {
    close(!item.href);
    item.onselect?.();
  }

  $effect(() => {
    if (!open) return;
    const away = (event: PointerEvent) => {
      if (root && !root.contains(event.target as Node)) close(false);
    };
    document.addEventListener('pointerdown', away);
    return () => document.removeEventListener('pointerdown', away);
  });
</script>

<svelte:window {onkeydown} />

<div class="menu" bind:this={root} {onfocusout}>
  <Button
    {variant}
    {icon}
    bind:element={trigger}
    aria-expanded={open}
    aria-controls="{uid}-list"
    onclick={toggle}
  >
    {label}<Icon name="chevron-down" size={14} />
  </Button>
  <ul id="{uid}-list" class="list {align}" bind:this={list} hidden={!open}>
    {#each items as item (item.label)}
      <li>
        {#if item.href}
          <a href={item.href} onclick={() => choose(item)}>
            {#if item.icon}<Icon name={item.icon} />{/if}{item.label}
          </a>
        {:else}
          <button type="button" onclick={() => choose(item)}>
            {#if item.icon}<Icon name={item.icon} />{/if}{item.label}
          </button>
        {/if}
      </li>
    {/each}
  </ul>
</div>

<style>
  .menu {
    position: relative;
    display: inline-block;
  }
  .list {
    position: absolute;
    top: calc(100% + var(--space-1));
    z-index: 20;
    min-width: 13rem;
    margin: 0;
    padding: var(--space-1);
    list-style: none;
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-2);
  }
  .list[hidden] {
    display: none;
  }
  .start {
    left: 0;
  }
  .end {
    right: 0;
  }
  .list a,
  .list button {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    width: 100%;
    min-height: 2rem;
    padding: var(--space-1) var(--space-2);
    border: 0;
    border-radius: var(--radius);
    background: none;
    color: var(--text);
    font: inherit;
    font-weight: var(--weight-medium);
    text-align: left;
    text-decoration: none;
    justify-content: flex-start;
  }
  .list a:hover,
  .list button:hover,
  .list a:focus-visible,
  .list button:focus-visible {
    background: var(--surface-2);
  }
</style>
