<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
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
  let { children } = $props();

  const links = [
    { href: '/', label: 'System' },
    { href: '/imports', label: 'Imports' },
    { href: '/library', label: 'Library' },
    { href: '/catalog', label: 'Catalog' },
    { href: '/burns', label: 'Burns' },
    { href: '/discs', label: 'Discs' },
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
        {/if}
      </ul>
      <div class="who">
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
  :global(body) {
    margin: 0;
    font-family: system-ui, sans-serif;
    line-height: 1.5;
  }
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
    border-bottom: 1px solid currentColor;
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
    gap: 1rem;
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
    font-size: 0.875rem;
  }
  :global(a:focus-visible),
  :global(button:focus-visible) {
    outline: 3px solid currentColor;
    outline-offset: 2px;
  }
</style>
