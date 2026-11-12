<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // The design system (docs/plans/ui-overhaul.md): fonts bundled and served
  // from this origin, then the tokens, then the element styles.
  import '@fontsource-variable/inter';
  import '@fontsource-variable/jetbrains-mono';
  import '$lib/styles/tokens.css';
  import '$lib/styles/base.css';
  import { goto } from '$app/navigation';
  import { page } from '$app/state';
  import {
    loadSession,
    loadSetupStatus,
    rememberSession,
    roleLabel,
    signOut,
    whenSignedOut,
    type SessionView,
  } from '$lib/session';
  import { LICENSE_NAME, SOURCE_URL } from '$lib/source';
  import {
    THEME_CHOICES,
    applyTheme,
    isThemeChoice,
    loadTheme,
    saveTheme,
    themeLabel,
    type ThemeChoice,
  } from '$lib/theme';
  let { children } = $props();

  // Applied as the app starts, on every page, signed in or not.
  const startingTheme = loadTheme();
  applyTheme(startingTheme);
  let theme = $state<ThemeChoice>(startingTheme);

  function chooseTheme(value: string) {
    if (!isThemeChoice(value)) return;
    theme = value;
    saveTheme(value);
    applyTheme(value);
  }

  const links = [
    { href: '/', label: 'System' },
    { href: '/imports', label: 'Imports' },
    { href: '/library', label: 'Library' },
    { href: '/catalog', label: 'Catalog' },
    { href: '/burns', label: 'Burns' },
    { href: '/discs', label: 'Discs' },
    { href: '/rips', label: 'Rips' },
  ];

  /** Pages anyone may see: the way in. */
  const PUBLIC_PATHS = ['/login', '/setup'];

  type Gate =
    | { kind: 'checking' }
    | { kind: 'public' }
    | { kind: 'signed-in'; session: SessionView }
    | { kind: 'unreachable'; detail: string };

  let gate = $state<Gate>({ kind: 'checking' });
  const isPublic = $derived(PUBLIC_PATHS.includes(page.url.pathname));

  function toSignIn() {
    rememberSession(null);
    gate = { kind: 'checking' };
    const here = page.url.pathname + page.url.search;
    void goto(`/login?next=${encodeURIComponent(here)}`, { replaceState: true });
  }

  // Any API call that finds the session gone sends the person to sign in,
  // and back here afterwards. The sign-in pages handle their own refusals.
  whenSignedOut(() => {
    if (!PUBLIC_PATHS.includes(page.url.pathname)) toSignIn();
  });

  /**
   * Find out who is signed in.
   *
   * With plain `fetch`, not the API helper: a 401 here is the expected answer
   * for somebody who has not signed in, not a lapsed session to react to.
   */
  async function check() {
    if (isPublic) {
      gate = { kind: 'public' };
      return;
    }
    const session = await loadSession(fetch);
    if (session.kind === 'ok') {
      rememberSession(session.data);
      gate = { kind: 'signed-in', session: session.data };
      return;
    }
    if (session.problem.code === 'UNAUTHENTICATED') {
      // A server with no accounts yet sends its first visitor to set one up.
      const setup = await loadSetupStatus(fetch);
      if (setup.kind === 'ok' && setup.data.needed) {
        void goto('/setup', { replaceState: true });
        return;
      }
      toSignIn();
      return;
    }
    gate = { kind: 'unreachable', detail: session.problem.detail };
  }

  // Checked on every navigation, so signing in on one page and arriving on
  // another shows the right header, and a role change is picked up.
  $effect(() => {
    void page.url.pathname;
    void check();
  });

  async function leave() {
    await signOut();
    gate = { kind: 'checking' };
    void goto('/login', { replaceState: true });
  }
</script>

<a class="skip" href="#main">Skip to content</a>

{#if gate.kind === 'public'}
  <main id="main">
    {@render children()}
  </main>
{:else if gate.kind === 'signed-in'}
  <header>
    <!-- A nav landmark with an accessible name, so a screen reader can jump
         straight to it. -->
    <nav aria-label="Primary">
      <strong>Tangible</strong>
      <ul>
        {#each links as link (link.href)}
          <li>
            <a href={link.href} aria-current={page.url.pathname === link.href ? 'page' : undefined}
              >{link.label}</a
            >
          </li>
        {/each}
        {#if gate.session.role === 'administrator'}
          <!-- Shown to administrators only for tidiness; the server is what
               refuses everyone else. -->
          <li>
            <a href="/workers" aria-current={page.url.pathname === '/workers' ? 'page' : undefined}
              >Workers</a
            >
          </li>
          <li>
            <a href="/users" aria-current={page.url.pathname === '/users' ? 'page' : undefined}
              >Accounts</a
            >
          </li>
          <li>
            <a
              href="/integrations"
              aria-current={page.url.pathname === '/integrations' ? 'page' : undefined}
              >Integrations</a
            >
          </li>
          <li>
            <a href="/storage" aria-current={page.url.pathname === '/storage' ? 'page' : undefined}
              >Storage</a
            >
          </li>
          <li>
            <a href="/dats" aria-current={page.url.pathname === '/dats' ? 'page' : undefined}
              >DATs</a
            >
          </li>
          <li>
            <a href="/audit" aria-current={page.url.pathname === '/audit' ? 'page' : undefined}
              >Audit log</a
            >
          </li>
          <li>
            <a
              href="/diagnostics"
              aria-current={page.url.pathname === '/diagnostics' ? 'page' : undefined}
              >Diagnostics</a
            >
          </li>
        {/if}
      </ul>
      <div class="who">
        <label class="theme">
          Theme
          <select value={theme} onchange={(event) => chooseTheme(event.currentTarget.value)}>
            {#each THEME_CHOICES as choice (choice)}
              <option value={choice}>{themeLabel(choice)}</option>
            {/each}
          </select>
        </label>
        <span>{gate.session.username} ({roleLabel(gate.session.role)})</span>
        <button type="button" onclick={leave}>Sign out</button>
      </div>
    </nav>
  </header>

  <main id="main">
    {@render children()}
  </main>
{:else if gate.kind === 'unreachable'}
  <main id="main">
    <h1>Tangible</h1>
    <div role="alert">
      <h2>Could not check your session</h2>
      <p>{gate.detail}</p>
      <button type="button" onclick={check}>Try again</button>
    </div>
  </main>
{:else}
  <main id="main" aria-busy="true">
    <p aria-live="polite">Checking your session…</p>
  </main>
{/if}

<!-- On every page, signed in or not: users of a network service are owed
     the means to get its source (AGPL section 13). -->
<footer>
  <a href={SOURCE_URL} rel="noreferrer">Source code</a>, free software under {LICENSE_NAME}
</footer>

<style>
  /* Visible only when focused: keyboard users get it, everyone else does not. */
  .skip {
    position: absolute;
    left: -9999px;
  }
  .skip:focus {
    position: static;
    display: inline-block;
    padding: 0.5rem;
  }
  header {
    background: var(--surface);
    border-bottom: 1px solid var(--line);
  }
  nav {
    display: flex;
    gap: 1.5rem;
    align-items: center;
    max-width: 60rem;
    margin: 0 auto;
    padding: 0.75rem 1rem;
  }
  .who {
    margin-left: auto;
    display: flex;
    gap: 0.75rem;
    align-items: center;
  }
  nav ul {
    display: flex;
    flex-wrap: wrap;
    gap: 0.25rem 1rem;
    list-style: none;
    margin: 0;
    padding: 0;
  }
  /* aria-current is the source of truth; the underline is the visual echo, so
     the state is never communicated by styling alone. */
  nav a[aria-current='page'] {
    text-decoration: underline;
    font-weight: 600;
  }
  main {
    max-width: 60rem;
    margin: 0 auto;
    padding: 1.5rem 1rem;
  }
  footer {
    max-width: 60rem;
    margin: 0 auto;
    padding: 1rem;
    font-size: var(--text-sm);
    color: var(--text-muted);
  }
  .theme {
    display: flex;
    gap: 0.5rem;
    align-items: center;
    font-weight: var(--weight-regular);
    color: var(--text-muted);
  }
</style>
