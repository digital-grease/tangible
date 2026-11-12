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
  import AppShell from '$lib/components/shell/AppShell.svelte';
  import {
    loadSession,
    loadSetupStatus,
    rememberSession,
    signOut,
    whenSignedOut,
    type SessionView,
  } from '$lib/session';
  import { LICENSE_NAME, SOURCE_URL } from '$lib/source';
  import { applyTheme, loadTheme, saveTheme, type ThemeChoice } from '$lib/theme';
  let { children } = $props();

  // Applied as the app starts, on every page, signed in or not.
  const startingTheme = loadTheme();
  applyTheme(startingTheme);
  let theme = $state<ThemeChoice>(startingTheme);

  function chooseTheme(choice: ThemeChoice) {
    theme = choice;
    saveTheme(choice);
    applyTheme(choice);
  }

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
  <AppShell
    session={gate.session}
    pathname={page.url.pathname}
    {theme}
    onTheme={chooseTheme}
    onSignOut={leave}
  >
    {@render children()}
  </AppShell>
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
     the means to get its source (AGPL section 13). Signed in, the shell's
     sidebar carries it. -->
{#if gate.kind !== 'signed-in'}
  <footer>
    <a href={SOURCE_URL} rel="noreferrer">Source code</a>, free software under {LICENSE_NAME}
  </footer>
{/if}

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
  .skip:focus {
    z-index: 50;
  }
</style>
