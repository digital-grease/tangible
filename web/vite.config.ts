// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vitest/config';
import { thirdPartyNotices } from './src/vite/third-party-notices';

export default defineConfig({
  plugins: [sveltekit(), thirdPartyNotices()],
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
