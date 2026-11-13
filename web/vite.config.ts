// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { svelteTesting } from '@testing-library/svelte/vite';
import { defineConfig } from 'vitest/config';
import { thirdPartyNotices } from './src/vite/third-party-notices.ts';

export default defineConfig({
  plugins: [
    // SvelteKit's configuration, which svelte.config.js held before
    // SvelteKit 3.
    sveltekit({
      preprocess: vitePreprocess(),
      // SvelteKit 3 dropped the $lib alias for package.json's #lib import,
      // which TypeScript resolves only with a file extension on every
      // import; kept as SvelteKit offers, it becomes tsconfig paths too.
      alias: { $lib: 'src/lib' },
      // A static single-page build, served by the Tangible server from the
      // same origin as the API. Every page loads its data in the browser
      // through the public API, so there is nothing for a server-side
      // renderer to do, and no Node process to run, patch or expose in
      // production. Paths the build has no file for get the fallback page,
      // which routes in the browser.
      adapter: adapter({ fallback: 'index.html', strict: true }),
      csp: {
        // The web UI is always served under a Content Security Policy. Kit
        // writes it into the page with hashes for its own inline bootstrap;
        // the server adds frame-ancestors, which only a header can carry.
        mode: 'hash',
        directives: {
          'default-src': ['self'],
          'object-src': ['none'],
          'base-uri': ['self'],
          'form-action': ['self'],
          // Style attributes only, not stylesheets or scripts. SvelteKit's
          // own route announcer, which tells screen readers the page
          // changed, sets one, as does app.html's wrapper; blocked, the
          // announcer shows as stray text. An attribute style cannot run
          // script or select other elements, and default-src still stops it
          // loading anything from elsewhere.
          'style-src-attr': ['unsafe-inline'],
        },
      },
    }),
    // svelteTesting acts only under vitest: it compiles components for the
    // browser and cleans the DOM up after each component test.
    svelteTesting(),
    thirdPartyNotices(),
  ],
  build: {
    // Never inline an asset as a data: URI. The Content Security Policy
    // allows only this origin (default-src 'self'), so an inlined font or
    // image is refused: Vite inlines files under 4 KiB, which took the
    // smallest font subsets and left their characters in a fallback font.
    assetsInlineLimit: 0,
  },
  server: {
    proxy: {
      // Development only: the UI consumes the same public API as any other
      // client, so proxy rather than embedding a base URL.
      '/api': 'http://localhost:8080',
      '/livez': 'http://localhost:8080',
      '/readyz': 'http://localhost:8080',
    },
  },
  test: {
    include: ['src/**/*.{test,spec}.{js,ts}'],
  },
});
