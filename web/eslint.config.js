// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import js from '@eslint/js';
import svelte from 'eslint-plugin-svelte';
import globals from 'globals';
import ts from 'typescript-eslint';

export default ts.config(
  js.configs.recommended,
  ...ts.configs.recommended,
  ...svelte.configs['flat/recommended'],
  {
    languageOptions: {
      globals: { ...globals.browser, ...globals.node },
    },
  },
  {
    rules: {
      // Guards links in an app served under a base path. Tangible's UI is
      // always served at the root by its own server, beside /api/v1, and its
      // API calls are absolute, so there is no base path to resolve against.
      'svelte/no-navigation-without-resolve': 'off',
    },
  },
  {
    files: ['**/*.svelte'],
    languageOptions: {
      parserOptions: { parser: ts.parser },
    },
  },
  {
    // Prettier owns formatting; eslint owns correctness. Overlapping them
    // produces rules that fight each other.
    ignores: ['.svelte-kit/', 'build/', 'node_modules/', 'pnpm-lock.yaml'],
  },
);
