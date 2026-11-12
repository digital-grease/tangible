<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // The shell around every signed-in page (docs/plans/ui-overhaul.md, "The
  // shell"): the sidebar with the grouped navigation and the account, the
  // top bar with where you are, the command bar and the New menu, the drive
  // bay, and the page.
  //
  // Below 1200 pixels the sidebar keeps its icons only, each still named for
  // a screen reader, unless expanded; above, it can be collapsed to icons
  // (lib/sidebar.ts, remembered in the browser). Below 900 it becomes a
  // drawer behind a menu button.
  import { onMount, type Snippet } from 'svelte';
  import Icon from '$lib/components/ui/Icon.svelte';
  import Menu from '$lib/components/ui/Menu.svelte';
  import { loadDashboard } from '$lib/dashboard';
  import { watch } from '$lib/events';
  import { countLabel, counts, isCurrent, navFor, whereAmI, type CountKey } from '$lib/nav';
  import { roleLabel, type SessionView } from '$lib/session';
  import {
    loadSidebar,
    saveSidebar,
    sidebarCompact,
    toggledSidebar,
    type SidebarChoice,
  } from '$lib/sidebar';
  import { LICENSE_NAME, SOURCE_URL } from '$lib/source';
  import { THEME_CHOICES, isThemeChoice, themeLabel, type ThemeChoice } from '$lib/theme';
  import CommandBar from './CommandBar.svelte';
  import DriveBay from './DriveBay.svelte';
  import Wordmark from './Wordmark.svelte';

  let {
    session,
    pathname,
    theme,
    onTheme,
    onSignOut,
    children,
  }: {
    session: SessionView;
    pathname: string;
    theme: ThemeChoice;
    onTheme: (choice: ThemeChoice) => void;
    onSignOut: () => void;
    children: Snippet;
  } = $props();

  const groups = $derived(navFor(session.role));
  const here = $derived(whereAmI(pathname, groups));
  const administrator = $derived(session.role === 'administrator');

  let figures = $state<Record<CountKey, number> | null>(null);
  let drawerOpen = $state(false);
  let sidebarChoice = $state<SidebarChoice>(loadSidebar());
  let width = $state(0);
  const compact = $derived(sidebarCompact(sidebarChoice, width));

  function toggleSidebar() {
    sidebarChoice = toggledSidebar(compact);
    saveSidebar(sidebarChoice);
  }
  let menuButton = $state<HTMLButtonElement | null>(null);

  const newItems = [
    { label: 'Import a file or a URL', href: '/imports', icon: 'import' as const },
    { label: 'Rip a disc', href: '/rips', icon: 'rip' as const },
    { label: 'Create a burn job', href: '/burns/new', icon: 'burn' as const },
  ];

  async function refreshCounts() {
    const result = await loadDashboard();
    if (result.kind === 'ok') figures = counts(result.data);
  }

  onMount(() => {
    void refreshCounts();
    return watch(['imports', 'burns'], refreshCounts);
  });

  // Going somewhere closes the drawer.
  $effect(() => {
    void pathname;
    drawerOpen = false;
  });

  function nextTheme(): ThemeChoice {
    const at = THEME_CHOICES.indexOf(theme);
    return THEME_CHOICES[(at + 1) % THEME_CHOICES.length] ?? 'system';
  }

  function onkeydown(event: KeyboardEvent) {
    if (event.key === 'Escape' && drawerOpen) {
      drawerOpen = false;
      menuButton?.focus();
    }
  }
</script>

<svelte:window {onkeydown} bind:innerWidth={width} />

<div class="app" class:compact class:drawer-open={drawerOpen}>
  <aside class="sidebar" id="sidebar">
    <a class="mark" href="/" aria-label="Tangible, overview">
      <Wordmark hole="var(--surface)" />
    </a>

    <nav aria-label="Primary">
      {#each groups as group (group.label ?? 'top')}
        <div class="group">
          {#if group.label}<div class="group-label label">{group.label}</div>{/if}
          <ul>
            {#each group.items as item (item.href)}
              {@const count = item.count && figures ? figures[item.count] : 0}
              <li>
                <a
                  href={item.href}
                  class="item"
                  aria-current={isCurrent(item, pathname) ? 'page' : undefined}
                  title={item.label}
                >
                  <Icon name={item.icon} size={17} />
                  <span class="label">{item.label}</span>
                  {#if item.count && count > 0}
                    <span class="count num" aria-hidden="true">{count}</span>
                    <span class="visually-hidden">, {countLabel(item.count, count)}</span>
                  {/if}
                </a>
              </li>
            {/each}
          </ul>
        </div>
      {/each}
    </nav>

    <div class="account">
      <p class="who label">
        <span class="name">{session.username}</span>
        <span class="role">{roleLabel(session.role)}</span>
      </p>
      <label class="theme label">
        Theme
        <select
          value={theme}
          onchange={(event) => {
            const value = event.currentTarget.value;
            if (isThemeChoice(value)) onTheme(value);
          }}
        >
          {#each THEME_CHOICES as choice (choice)}
            <option value={choice}>{themeLabel(choice)}</option>
          {/each}
        </select>
      </label>
      <!-- Where the sidebar is icons only, the theme is one button that
           steps through the choices, saying which it is on. -->
      <button
        type="button"
        class="icon-button theme-step"
        onclick={() => onTheme(nextTheme())}
        aria-label="Theme: {themeLabel(theme)}. Change to {themeLabel(nextTheme())}"
        title="Theme: {themeLabel(theme)}"
      >
        <Icon name={theme === 'dark' ? 'moon' : theme === 'light' ? 'sun' : 'monitor'} />
      </button>
      <button type="button" class="icon-button sign-out" onclick={onSignOut} title="Sign out">
        <Icon name="sign-out" />
        <span class="label">Sign out</span>
      </button>
      <!-- On every page: users of a network service are owed the means to
           get its source (AGPL section 13). -->
      <p class="source label">
        <a href={SOURCE_URL} rel="noreferrer">Source code</a>, {LICENSE_NAME}
      </p>
      <a class="icon-button source-icon" href={SOURCE_URL} rel="noreferrer" title="Source code">
        <Icon name="source" label="Source code, {LICENSE_NAME}" />
      </a>
      <!-- Labels or icons only, at any width that is not a drawer. Named
           for what pressing it does. -->
      <button
        type="button"
        class="icon-button sidebar-toggle"
        onclick={toggleSidebar}
        title={compact ? 'Expand the sidebar' : 'Collapse the sidebar'}
      >
        <Icon name={compact ? 'sidebar-expand' : 'sidebar-collapse'} />
        <span class="label">{compact ? 'Expand the sidebar' : 'Collapse the sidebar'}</span>
      </button>
    </div>
  </aside>

  <button
    type="button"
    class="scrim"
    tabindex="-1"
    aria-hidden="true"
    onclick={() => (drawerOpen = false)}
  ></button>

  <div class="column">
    <header class="topbar">
      <button
        type="button"
        class="icon-button menu-button"
        bind:this={menuButton}
        aria-controls="sidebar"
        aria-expanded={drawerOpen}
        aria-label="Menu"
        onclick={() => (drawerOpen = !drawerOpen)}
      >
        <Icon name="menu" />
      </button>
      <p class="where">
        {#if here?.group}<span class="group-name">{here.group}</span><span
            class="slash"
            aria-hidden="true">/</span
          >{/if}<span class="page-name">{here?.item.label ?? ''}</span>
      </p>
      <CommandBar {groups} />
      <Menu label="New" icon="plus" variant="primary" align="end" items={newItems} />
    </header>
    <DriveBay {administrator} />
    <main id="main">
      {@render children()}
    </main>
  </div>
</div>

<style>
  .app {
    display: grid;
    grid-template-columns: 13.5rem minmax(0, 1fr);
    min-height: 100vh;
  }

  /* --- sidebar ------------------------------------------------------------ */
  .sidebar {
    position: sticky;
    top: 0;
    height: 100vh;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    padding: var(--space-3) var(--space-2);
    background: var(--surface);
    border-right: 1px solid var(--line);
  }
  .mark {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-1) var(--space-2) var(--space-2);
    color: var(--text);
    text-decoration: none;
  }
  nav {
    display: grid;
    gap: var(--space-3);
  }
  .group-label {
    padding: 0 var(--space-2) var(--space-1);
    font-size: 0.6875rem;
    font-weight: var(--weight-strong);
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--text-muted);
  }
  nav ul {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: 1px;
  }
  .item {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: 0.375rem var(--space-2);
    border-radius: var(--radius);
    color: var(--text);
    text-decoration: none;
    font-weight: var(--weight-medium);
  }
  .item :global(.icon) {
    color: var(--text-muted);
  }
  .item:hover {
    background: var(--surface-2);
  }
  /* aria-current is the truth; the background, bar and icon colour are its
     echo, and the bold weight a second one, so it is not colour alone. */
  .item[aria-current='page'] {
    background: var(--surface-2);
    box-shadow: inset 3px 0 0 var(--accent);
    font-weight: var(--weight-strong);
  }
  .item[aria-current='page'] :global(.icon) {
    color: var(--accent);
  }
  .count {
    margin-left: auto;
    min-width: 1.375rem;
    padding: 0.0625rem 0.375rem;
    border-radius: var(--radius-full);
    background: var(--line-2);
    color: var(--text-muted);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    font-weight: var(--weight-strong);
    text-align: center;
  }
  .account {
    margin-top: auto;
    display: grid;
    gap: var(--space-2);
    padding: var(--space-3) var(--space-2) var(--space-1);
    border-top: 1px solid var(--line);
    font-size: var(--text-sm);
  }
  .who {
    margin: 0;
    display: grid;
  }
  .name {
    font-weight: var(--weight-strong);
    overflow-wrap: anywhere;
  }
  .role,
  .source {
    color: var(--text-muted);
  }
  .source {
    margin: 0;
    font-size: var(--text-xs);
  }
  .theme {
    display: grid;
    gap: var(--space-1);
    font-weight: var(--weight-medium);
    color: var(--text-muted);
  }
  .theme select {
    width: 100%;
    font-size: var(--text-sm);
  }
  .icon-button {
    justify-content: flex-start;
    border-color: transparent;
    background: none;
    color: var(--text-muted);
    min-height: 2rem;
    padding: var(--space-1) var(--space-2);
  }
  .icon-button:hover {
    color: var(--text);
    background: var(--surface-2);
  }
  .theme-step,
  .source-icon,
  .menu-button,
  .scrim {
    display: none;
  }

  /* --- main column -------------------------------------------------------- */
  .column {
    min-width: 0;
    display: flex;
    flex-direction: column;
  }
  .topbar {
    position: sticky;
    top: 0;
    z-index: 10;
    display: flex;
    align-items: center;
    gap: var(--space-3);
    min-height: 3.25rem;
    padding: var(--space-2) var(--space-6);
    background: color-mix(in srgb, var(--bg) 92%, transparent);
    backdrop-filter: blur(6px);
    border-bottom: 1px solid var(--line);
  }
  .where {
    margin: 0 auto 0 0;
    font-weight: var(--weight-strong);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .group-name {
    color: var(--text-muted);
    font-weight: var(--weight-medium);
  }
  .slash {
    margin: 0 var(--space-2);
    color: var(--text-muted);
  }
  main {
    padding: var(--space-6);
    max-width: 90rem;
    width: 100%;
  }

  /* --- icons only (lib/sidebar.ts decides when) --------------------------- */
  .app.compact {
    grid-template-columns: 4rem minmax(0, 1fr);
  }
  .compact .sidebar {
    align-items: center;
    padding-inline: var(--space-1);
  }
  .compact .mark {
    --wordmark-name: none;
  }
  .compact .sidebar .label {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip: rect(0 0 0 0);
    white-space: nowrap;
  }
  .compact .group-label {
    display: none;
  }
  .compact nav {
    gap: var(--space-2);
    width: 100%;
  }
  .compact .item {
    justify-content: center;
    position: relative;
  }
  .compact .count {
    position: absolute;
    top: 0;
    right: 0.125rem;
    min-width: 1rem;
    padding: 0 0.25rem;
    font-size: 0.625rem;
  }
  .compact .account {
    justify-items: center;
    padding-inline: 0;
    width: 100%;
  }
  .compact .theme {
    display: none;
  }
  .compact .theme-step,
  .compact .source-icon {
    display: inline-flex;
  }

  /* --- drawer ------------------------------------------------------------- */
  @media (max-width: 899px) {
    .app {
      grid-template-columns: minmax(0, 1fr);
    }
    .sidebar {
      position: fixed;
      inset: 0 auto 0 0;
      z-index: 30;
      width: min(17rem, 85vw);
      align-items: stretch;
      padding-inline: var(--space-2);
      transform: translateX(-100%);
      visibility: hidden;
      transition:
        transform 0.2s ease,
        visibility 0.2s;
      box-shadow: var(--shadow-2);
    }
    .drawer-open .sidebar {
      transform: none;
      visibility: visible;
    }
    .scrim {
      position: fixed;
      inset: 0;
      z-index: 25;
      border: 0;
      border-radius: 0;
      background: rgb(5 8 12 / 0.45);
    }
    .drawer-open .scrim {
      display: block;
    }
    .menu-button {
      display: inline-flex;
    }
    .sidebar-toggle {
      display: none;
    }
    .topbar,
    main {
      padding-inline: var(--space-4);
    }
  }
</style>
