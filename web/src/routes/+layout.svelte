<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { page } from '$app/state';
  let { children } = $props();

  const links = [
    { href: '/', label: 'System' },
    { href: '/imports', label: 'Imports' },
    { href: '/library', label: 'Library' },
    { href: '/catalog', label: 'Catalog' },
    { href: '/burns', label: 'Burns' },
    { href: '/discs', label: 'Discs' },
  ];
</script>

<a class="skip" href="#main">Skip to content</a>

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
    </ul>
  </nav>
</header>

<main id="main">
  {@render children()}
</main>

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
  :global(a:focus-visible),
  :global(button:focus-visible) {
    outline: 3px solid currentColor;
    outline-offset: 2px;
  }
</style>
