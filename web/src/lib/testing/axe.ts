// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

/**
 * Accessibility checks for component tests (docs/plans/ui-overhaul.md,
 * decision 11). Imported by tests only, so axe-core is never bundled.
 *
 * Two kinds of rule are off. jsdom does not lay pages out or compute colour,
 * so colour contrast is checked on the design tokens instead
 * (styles/tokens.test.ts). And a component rendered on its own is not a
 * page, so the rules about landmarks, a main heading and the page's
 * language do not apply to it.
 */
import axe from 'axe-core';

const OFF = [
  'color-contrast',
  'region',
  'landmark-one-main',
  'page-has-heading-one',
  'html-has-lang',
  'document-title',
];

/** Each violation in `container`, as "rule: what it asks (where)". */
export async function axeViolations(container: Element): Promise<string[]> {
  const results = await axe.run(container, {
    rules: Object.fromEntries(OFF.map((rule) => [rule, { enabled: false }])),
  });
  return results.violations.map(
    (violation) =>
      `${violation.id}: ${violation.help} (${violation.nodes
        .map((node) => node.target.join(' '))
        .join(', ')})`,
  );
}
