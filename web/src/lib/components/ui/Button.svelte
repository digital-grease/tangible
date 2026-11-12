<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // A button, or a link that looks like one.
  //
  // Variants: primary for the one action a view is for; secondary for the
  // rest; quiet for actions that should not draw the eye; danger only for
  // destructive actions, whose words must still say what will happen
  // (AGENTS.md 9). With `href` it is a link, for actions that go somewhere.
  // A button with an icon and no words must have a label.
  import type { Snippet } from 'svelte';
  import type { HTMLAnchorAttributes, HTMLButtonAttributes } from 'svelte/elements';
  import Icon from './Icon.svelte';
  import type { IconName } from './icons';

  type Variant = 'primary' | 'secondary' | 'quiet' | 'danger';

  let {
    variant = 'secondary',
    size = 'md',
    icon,
    label,
    href,
    type = 'button',
    element = $bindable(null),
    children,
    ...rest
  }: {
    variant?: Variant;
    size?: 'sm' | 'md';
    icon?: IconName;
    /** The accessible name, for a button that shows only an icon. */
    label?: string;
    /** Makes it a link to this address. */
    href?: string;
    type?: 'button' | 'submit' | 'reset';
    /** The rendered element, for a caller that must move focus to it. */
    element?: HTMLElement | null;
    children?: Snippet;
  } & Omit<HTMLButtonAttributes, 'type'> &
    Omit<HTMLAnchorAttributes, 'href' | 'type'> = $props();

  const iconOnly = $derived(!children);
</script>

{#snippet content()}
  {#if icon}<Icon name={icon} size={size === 'sm' ? 14 : 16} />{/if}
  {@render children?.()}
{/snippet}

{#if href}
  <a
    bind:this={element}
    {href}
    class="btn {variant} {size}"
    class:icon-only={iconOnly}
    aria-label={label}
    title={iconOnly ? label : undefined}
    {...rest as HTMLAnchorAttributes}
  >
    {@render content()}
  </a>
{:else}
  <button
    bind:this={element}
    {type}
    class="btn {variant} {size}"
    class:icon-only={iconOnly}
    aria-label={label}
    title={iconOnly ? label : undefined}
    {...rest as HTMLButtonAttributes}
  >
    {@render content()}
  </button>
{/if}

<style>
  .btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: var(--space-2);
    min-height: 2rem;
    padding: var(--space-1) var(--space-3);
    border: 1px solid var(--line-strong);
    border-radius: var(--radius);
    background: var(--surface);
    color: var(--text);
    font: inherit;
    font-weight: var(--weight-strong);
    line-height: 1.2;
    text-decoration: none;
    white-space: nowrap;
    cursor: pointer;
  }
  .btn.sm {
    min-height: 1.625rem;
    padding: 0 var(--space-2);
    font-size: var(--text-sm);
  }
  .btn.icon-only {
    padding: 0;
    width: 2rem;
  }
  .btn.sm.icon-only {
    width: 1.625rem;
  }
  .btn:hover:not(:disabled) {
    background: var(--surface-2);
  }
  .primary {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--accent-ink);
  }
  .primary:hover:not(:disabled) {
    background: color-mix(in srgb, var(--accent) 86%, var(--text));
  }
  .quiet {
    background: transparent;
    border-color: transparent;
    color: var(--text-muted);
  }
  .quiet:hover:not(:disabled) {
    color: var(--text);
  }
  .danger {
    background: var(--surface);
    border-color: var(--bad);
    color: var(--bad);
  }
  .danger:hover:not(:disabled) {
    background: color-mix(in srgb, var(--bad) 10%, var(--surface));
  }
  .btn:disabled {
    cursor: not-allowed;
    opacity: 0.55;
  }
</style>
