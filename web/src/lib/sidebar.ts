// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

/**
 * Whether the shell's sidebar shows its labels or its icons only
 * (docs/plans/ui-overhaul.md, "The shell").
 *
 * Left alone, it shows icons only below 1200 pixels, and below 900 it is a
 * drawer instead. A person can expand or collapse it at any width from 900
 * up, and the choice is remembered in this browser.
 */

/** Left alone (`auto`), or as the person last set it. */
export type SidebarChoice = 'auto' | 'full' | 'compact';

export const SIDEBAR_KEY = 'tangible.sidebar';
/** Below this width the sidebar, left alone, shows icons only. */
export const ICONS_ONLY_BELOW = 1200;
/** Below this width the sidebar is a drawer, whatever was chosen. */
export const DRAWER_BELOW = 900;

/** Whether the sidebar shows icons only at this width, given the choice. */
export function sidebarCompact(choice: SidebarChoice, width: number): boolean {
  if (width < DRAWER_BELOW) return false;
  if (choice === 'full') return false;
  if (choice === 'compact') return true;
  return width < ICONS_ONLY_BELOW;
}

/** The choice that turns the sidebar the other way from how it is now. */
export function toggledSidebar(compactNow: boolean): SidebarChoice {
  return compactNow ? 'full' : 'compact';
}

function isSidebarChoice(value: unknown): value is SidebarChoice {
  return value === 'auto' || value === 'full' || value === 'compact';
}

/** The remembered choice, or `auto` where there is none or storage is unavailable. */
export function loadSidebar(): SidebarChoice {
  try {
    const value = localStorage.getItem(SIDEBAR_KEY);
    return isSidebarChoice(value) ? value : 'auto';
  } catch {
    return 'auto';
  }
}

/** Remember the choice; where storage is unavailable it still applies to this page. */
export function saveSidebar(choice: SidebarChoice): void {
  try {
    localStorage.setItem(SIDEBAR_KEY, choice);
  } catch {
    // Not remembered.
  }
}
