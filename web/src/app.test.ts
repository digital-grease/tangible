// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Every file the page links from the build is in the build. The server
// answers a path the build has no file for with the page itself, so a
// missing icon is not an error anywhere: the browser is handed HTML for it
// and shows none. Every release up to 0.5.0 linked a favicon.png that no
// build had; this reads the page and the static directory instead of
// trusting either.
import { readFileSync, existsSync } from 'node:fs';
import { describe, expect, it } from 'vitest';

const PAGE = readFileSync(new URL('./app.html', import.meta.url), 'utf8');
const STATIC = new URL('../static/', import.meta.url);

/** The files app.html links from the build's root, in order. */
function linked(page: string): string[] {
  return [...page.matchAll(/(?:href|src)="%sveltekit\.assets%\/([^"]+)"/g)].map(
    ([, file]) => file ?? '',
  );
}

describe('app.html', () => {
  it('links only files the build has', () => {
    const files = linked(PAGE);
    expect(files).toEqual(['favicon.ico', 'favicon.svg']);
    for (const file of files) {
      expect(existsSync(new URL(file, STATIC)), file).toBe(true);
    }
  });

  it('finds a link to a file the build lacks', () => {
    const page = '<link rel="icon" href="%sveltekit.assets%/favicon.png" />';
    expect(linked(page)).toEqual(['favicon.png']);
    expect(existsSync(new URL('favicon.png', STATIC))).toBe(false);
  });
});

describe('the icon', () => {
  it('is an SVG with nothing the page policy would refuse', () => {
    const svg = readFileSync(new URL('favicon.svg', STATIC), 'utf8');
    expect(svg).toMatch(/<svg xmlns="http:\/\/www\.w3\.org\/2000\/svg" viewBox="0 0 24 24">/);
    expect(svg).not.toMatch(/<style|<script|href=/);
  });

  it('is in favicon.ico at 16, 32 and 48 pixels', () => {
    const ico = readFileSync(new URL('favicon.ico', STATIC));
    // An icon directory: reserved 0, type 1 (icon), then the image count,
    // and a 16-byte entry for each that starts with its width and height.
    expect(ico.readUInt16LE(0)).toBe(0);
    expect(ico.readUInt16LE(2)).toBe(1);
    const count = ico.readUInt16LE(4);
    const sizes = Array.from({ length: count }, (_, i) => {
      const entry = 6 + i * 16;
      return [ico.readUInt8(entry), ico.readUInt8(entry + 1)];
    });
    expect(sizes).toEqual([
      [16, 16],
      [32, 32],
      [48, 48],
    ]);
  });
});
