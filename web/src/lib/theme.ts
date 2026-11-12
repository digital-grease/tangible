// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

/**
 * The colour theme (docs/plans/ui-overhaul.md, decision 2).
 *
 * The theme follows the system's setting until the person chooses one. With
 * no `data-theme` on the root element, tokens.css applies the system's
 * preference by itself, so the page is right before any script runs; a
 * choice sets the attribute.
 *
 * The choice is remembered in this browser's localStorage, a convenience
 * only: where storage is refused (a private window, blocked site data) the
 * choice still applies until the page is closed, and the next visit follows
 * the system again.
 */

export type ThemeChoice = 'system' | 'light' | 'dark';

/** The choices, in the order a menu offers them. */
export const THEME_CHOICES: readonly ThemeChoice[] = ['system', 'light', 'dark'];

/** Where the choice is kept. */
export const THEME_KEY = 'tangible.theme';

/** The part of Storage this module uses, so a test can stand one in. */
export interface StorageLike {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

/** This browser's localStorage, or `null` where it is missing or refused. */
function browserStorage(): StorageLike | null {
  try {
    return typeof localStorage === 'undefined' ? null : localStorage;
  } catch {
    return null;
  }
}

export function isThemeChoice(value: unknown): value is ThemeChoice {
  return value === 'system' || value === 'light' || value === 'dark';
}

/** The remembered choice; following the system when there is none. */
export function loadTheme(storage: StorageLike | null = browserStorage()): ThemeChoice {
  if (!storage) return 'system';
  try {
    const stored = storage.getItem(THEME_KEY);
    return isThemeChoice(stored) ? stored : 'system';
  } catch {
    return 'system';
  }
}

/** Remember a choice. Following the system is remembered as no choice. */
export function saveTheme(
  choice: ThemeChoice,
  storage: StorageLike | null = browserStorage(),
): void {
  if (!storage) return;
  try {
    if (choice === 'system') storage.removeItem(THEME_KEY);
    else storage.setItem(THEME_KEY, choice);
  } catch {
    // Not remembered; the choice still applies to this page.
  }
}

/** Show a choice: set or clear the root element's `data-theme`. */
export function applyTheme(
  choice: ThemeChoice,
  root: HTMLElement | null = typeof document === 'undefined' ? null : document.documentElement,
): void {
  if (!root) return;
  if (choice === 'system') delete root.dataset.theme;
  else root.dataset.theme = choice;
}

/** A choice in words. */
export function themeLabel(choice: ThemeChoice): string {
  switch (choice) {
    case 'system':
      return 'Follow the system';
    case 'light':
      return 'Light';
    case 'dark':
      return 'Dark';
  }
}
