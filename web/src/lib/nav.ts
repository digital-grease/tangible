// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

/**
 * The navigation (docs/plans/ui-overhaul.md, decision 1): every page, in
 * the groups the sidebar shows them in.
 *
 * The administrators' group is shown to administrators only, for tidiness;
 * the server is what refuses everyone else.
 */
import type { IconName } from './components/ui/icons';
import type { Dashboard } from './dashboard';

/** Which figure a page shows beside its name. */
export type CountKey = 'imports' | 'burns';

export interface NavItem {
  href: string;
  label: string;
  icon: IconName;
  count?: CountKey;
}

export interface NavGroup {
  /** The group's heading; `null` for the ungrouped first item. */
  label: string | null;
  items: readonly NavItem[];
  administratorsOnly?: boolean;
}

export const NAV: readonly NavGroup[] = [
  { label: null, items: [{ href: '/', label: 'Overview', icon: 'overview' }] },
  {
    label: 'Collection',
    items: [
      { href: '/library', label: 'Library', icon: 'library' },
      { href: '/catalog', label: 'Catalog', icon: 'catalog' },
      { href: '/discs', label: 'Physical copies', icon: 'disc' },
    ],
  },
  {
    label: 'Activity',
    items: [
      { href: '/imports', label: 'Imports', icon: 'import', count: 'imports' },
      { href: '/rips', label: 'Rips', icon: 'rip' },
      { href: '/burns', label: 'Burns', icon: 'burn', count: 'burns' },
    ],
  },
  {
    label: 'Administration',
    administratorsOnly: true,
    items: [
      { href: '/workers', label: 'Workers and drives', icon: 'worker' },
      { href: '/users', label: 'Accounts', icon: 'user' },
      { href: '/integrations', label: 'Integrations', icon: 'integration' },
      { href: '/storage', label: 'Storage', icon: 'storage' },
      { href: '/dats', label: 'DATs', icon: 'dat' },
      { href: '/audit', label: 'Audit log', icon: 'audit' },
      { href: '/diagnostics', label: 'Diagnostics', icon: 'diagnostics' },
    ],
  },
];

/** The groups a role sees. */
export function navFor(role: string | null | undefined): NavGroup[] {
  return NAV.filter((group) => !group.administratorsOnly || role === 'administrator');
}

/** Whether `pathname` is the item's page or one beneath it. */
export function isCurrent(item: Pick<NavItem, 'href'>, pathname: string): boolean {
  if (item.href === '/') return pathname === '/';
  return pathname === item.href || pathname.startsWith(`${item.href}/`);
}

/** The group and item a page belongs to, for the top bar. */
export function whereAmI(
  pathname: string,
  groups: readonly NavGroup[] = NAV,
): { group: string | null; item: NavItem } | null {
  for (const group of groups) {
    for (const item of group.items) {
      if (isCurrent(item, pathname)) return { group: group.label, item };
    }
  }
  return null;
}

/** The figures beside Imports and Burns: what is under way or waiting. */
export function counts(dashboard: Pick<Dashboard, 'imports' | 'burns'>): Record<CountKey, number> {
  return {
    imports: dashboard.imports.active,
    burns: dashboard.burns.queued + dashboard.burns.waiting_for_media + dashboard.burns.active,
  };
}

/** A figure in words, for a screen reader: the number alone says nothing. */
export function countLabel(key: CountKey, count: number): string {
  return key === 'imports'
    ? `${count} under way`
    : `${count} queued, waiting for a disc or under way`;
}
