// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
import { sveltekit } from '@sveltejs/kit/vite';
import { svelteTesting } from '@testing-library/svelte/vite';
import { defineConfig } from 'vitest/config';
import { thirdPartyNotices } from './src/vite/third-party-notices';

export default defineConfig({
  // svelteTesting acts only under vitest: it compiles components for the
  // browser and cleans the DOM up after each component test.
  plugins: [sveltekit(), svelteTesting(), thirdPartyNotices()],
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
