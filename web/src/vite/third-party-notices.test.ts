// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import { join, sep } from 'node:path';
import { describe, expect, it } from 'vitest';
import { _internal } from './third-party-notices';

const { packageRoot } = _internal;
const nm = (...parts: string[]) => join(sep, 'repo', 'web', 'node_modules', ...parts);

describe('third-party notices', () => {
  it('finds the package a bundled module came from', () => {
    expect(packageRoot(nm('svelte', 'src', 'internal', 'client', 'index.js'))).toBe(nm('svelte'));
    expect(packageRoot(nm('@sveltejs', 'kit', 'src', 'runtime', 'client.js'))).toBe(
      nm('@sveltejs', 'kit'),
    );
  });

  it('finds the package a bundled stylesheet came from, so fonts are listed', () => {
    // The fonts arrive as a CSS module that names its font files; the
    // stylesheet is what places them in the bundle.
    expect(packageRoot(nm('@fontsource-variable', 'inter', 'index.css'))).toBe(
      nm('@fontsource-variable', 'inter'),
    );
  });

  it('takes the innermost package of a nested dependency', () => {
    expect(packageRoot(nm('.pnpm', 'svelte@5', 'node_modules', 'esm-env', 'index.js'))).toBe(
      nm('.pnpm', 'svelte@5', 'node_modules', 'esm-env'),
    );
  });

  it("ignores the project's own modules", () => {
    expect(packageRoot(join(sep, 'repo', 'web', 'src', 'lib', 'library.ts'))).toBeNull();
  });
});
