// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
import adapter from '@sveltejs/adapter-static';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

/** @type {import('@sveltejs/kit').Config} */
export default {
  preprocess: vitePreprocess(),
  kit: {
    // A static single-page build, served by the Tangible server from the same
    // origin as the API. Every page loads its data in the browser through the
    // public API, so there is nothing for a server-side renderer to do, and
    // no Node process to run, patch or expose in production. Paths the build
    // has no file for get the fallback page, which routes in the browser.
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
        // Style attributes only, not stylesheets or scripts. SvelteKit's own
        // route announcer, which tells screen readers the page changed, sets
        // one, as does app.html's wrapper; blocked, the announcer shows as
        // stray text. An attribute style cannot run script or select other
        // elements, and default-src still stops it loading anything from
        // elsewhere.
        'style-src-attr': ['unsafe-inline'],
      },
    },
  },
};
