// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// The colour tokens meet WCAG 2.2 AA contrast in both themes. jsdom cannot
// judge colour, so this reads tokens.css itself and computes each pair the
// UI puts together (docs/plans/ui-overhaul.md, decision 11).
import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';

const CSS = readFileSync(new URL('./tokens.css', import.meta.url), 'utf8');

/** The body of the rule whose selector is exactly `selector`. */
function block(css: string, selector: string): string {
  const at = css.indexOf(`${selector} {`);
  if (at < 0) throw new Error(`no rule for ${selector}`);
  const open = css.indexOf('{', at);
  const close = css.indexOf('}', open);
  return css.slice(open + 1, close);
}

/** The colour tokens a rule sets, by name. */
function colours(body: string): Map<string, string> {
  const found = new Map<string, string>();
  for (const match of body.matchAll(/--([a-z0-9-]+):\s*(#[0-9a-fA-F]{6})\s*;/g)) {
    const [, name, value] = match;
    if (name && value) found.set(name, value.toLowerCase());
  }
  return found;
}

function luminance(hex: string): number {
  const channel = (offset: number) => {
    const value = parseInt(hex.slice(offset, offset + 2), 16) / 255;
    return value <= 0.03928 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * channel(1) + 0.7152 * channel(3) + 0.0722 * channel(5);
}

function contrast(a: string, b: string): number {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x) as [number, number];
  return (hi + 0.05) / (lo + 0.05);
}

const light = colours(block(CSS, ':root'));
const chosenDark = colours(block(CSS, ":root[data-theme='dark']"));
const systemDark = colours(block(CSS, ":root:not([data-theme='light'])"));

const THEMES = [
  ['light', light],
  ['dark', chosenDark],
] as const;

/** Backgrounds text sits on. */
const GROUNDS = ['bg', 'surface', 'surface-2'];

/** Colours used for words: body text, secondary text, links, status words. */
const TEXT = ['text', 'text-muted', 'accent', 'ok', 'warn', 'bad', 'laser'];

describe('colour tokens', () => {
  it('give the system dark theme the same values as a chosen one', () => {
    expect(systemDark.size).toBeGreaterThan(0);
    expect([...systemDark.entries()]).toEqual([...chosenDark.entries()]);
  });

  it('set every colour role in both themes', () => {
    expect([...chosenDark.keys()].sort()).toEqual([...light.keys()].sort());
  });

  describe.each(THEMES)('%s', (_name, theme) => {
    const get = (token: string) => {
      const value = theme.get(token);
      if (!value) throw new Error(`--${token} is not set`);
      return value;
    };

    it.each(TEXT.flatMap((fg) => GROUNDS.map((bg) => [fg, bg])))(
      '--%s on --%s reads at 4.5:1 or better',
      (fg, bg) => {
        expect(contrast(get(fg), get(bg))).toBeGreaterThanOrEqual(4.5);
      },
    );

    it('puts the primary action text on the accent at 4.5:1 or better', () => {
      expect(contrast(get('accent-ink'), get('accent'))).toBeGreaterThanOrEqual(4.5);
    });

    it.each(['ok', 'warn', 'bad', 'accent', 'text-muted'])(
      "draws a status mark's glyph (--bg on --%s) at 4.5:1 or better",
      (mark) => {
        expect(contrast(get('bg'), get(mark))).toBeGreaterThanOrEqual(4.5);
      },
    );

    it.each(GROUNDS)('outlines a form control (--line-strong) at 3:1 or better on --%s', (bg) => {
      expect(contrast(get('line-strong'), get(bg))).toBeGreaterThanOrEqual(3);
    });
  });
});
