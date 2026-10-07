// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// Writes THIRD_PARTY_NOTICES.txt beside the built UI: every npm package whose
// code ends up in the bundle, with its licence text.
//
// Every package here is a development dependency, because the UI is compiled,
// so a package manager's "production" listing is empty; yet the Svelte runtime
// and SvelteKit's client are bundled into what ships, and their licences
// require their notices to go with them. The bundler is the only thing that
// knows exactly which modules it included, so the list comes from there.

import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { join, sep } from 'node:path';
import type { Plugin } from 'vite';

const LICENSE_FILE = /^(licen[cs]e|copying|notice|unlicense)(\.|-|$)/i;

interface Bundled {
  name: string;
  version: string;
  license: string;
  texts: string[];
}

/** The package root a bundled module belongs to, if it came from node_modules. */
function packageRoot(id: string): string | null {
  const marker = `${sep}node_modules${sep}`;
  const at = id.lastIndexOf(marker);
  if (at < 0) return null;
  const rest = id.slice(at + marker.length).split(sep);
  const parts = rest[0]?.startsWith('@') ? rest.slice(0, 2) : rest.slice(0, 1);
  return join(id.slice(0, at + marker.length), ...parts);
}

function describe(root: string): Bundled | null {
  const manifest = join(root, 'package.json');
  if (!existsSync(manifest)) return null;
  const pkg = JSON.parse(readFileSync(manifest, 'utf8')) as {
    name?: string;
    version?: string;
    license?: string;
  };
  const texts = readdirSync(root)
    .filter((file) => LICENSE_FILE.test(file))
    .sort()
    .map((file) => readFileSync(join(root, file), 'utf8').trim());
  return {
    name: pkg.name ?? root,
    version: pkg.version ?? 'unknown',
    license: pkg.license ?? 'unknown',
    texts,
  };
}

export function thirdPartyNotices(): Plugin {
  const roots = new Set<string>();
  return {
    name: 'tangible-third-party-notices',
    apply: 'build',
    generateBundle(_options, bundle) {
      for (const output of Object.values(bundle)) {
        if (output.type !== 'chunk') continue;
        for (const id of Object.keys(output.modules)) {
          // Virtual modules carry a NUL prefix and a query; neither is a path.
          const path = id.replace(/^\0/, '').split('?')[0] ?? id;
          const root = packageRoot(path);
          if (root) roots.add(root);
        }
      }
      // Only the browser build ships; the server build SvelteKit also runs
      // is discarded by the static adapter.
      if (this.environment?.name !== 'client' && this.environment !== undefined) return;
      const packages = [...roots]
        .map(describe)
        .filter((p): p is Bundled => p !== null)
        .sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version));
      const seen = new Set<string>();
      const sections = packages
        .filter((p) => {
          const key = `${p.name}@${p.version}`;
          if (seen.has(key)) return false;
          seen.add(key);
          return true;
        })
        .map((p) =>
          [
            `${p.name} ${p.version}`,
            `License: ${p.license}`,
            '',
            p.texts.length > 0 ? p.texts.join('\n\n') : '(no licence file in the package)',
          ].join('\n'),
        );
      const header = [
        "Third-party software in Tangible's web UI",
        '',
        'These packages are compiled into the web UI that the Tangible server',
        'serves. Each is listed with the licence it is distributed under.',
        '',
      ].join('\n');
      this.emitFile({
        type: 'asset',
        fileName: 'THIRD_PARTY_NOTICES.txt',
        source: `${header}\n${sections.join(`\n\n${'='.repeat(72)}\n\n`)}\n`,
      });
    },
  };
}

// Exported for tests.
export const _internal = { packageRoot };
