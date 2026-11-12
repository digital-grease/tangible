// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

/**
 * What the command bar offers (docs/plans/ui-overhaul.md, decision 8): the
 * three global actions, every page the person may see, and artifacts found
 * by name. Pure; CommandBar.svelte shows it.
 */
import type { IconName } from './components/ui/icons';
import type { NavGroup } from './nav';

export interface Command {
  section: 'Actions' | 'Go to' | 'Library';
  label: string;
  /** Muted words after the label, such as an artifact's format. */
  detail?: string;
  href: string;
  icon: IconName;
}

/** The global actions (UX.md 2), each also a link elsewhere on the page. */
export const ACTIONS: readonly Command[] = [
  { section: 'Actions', label: 'Import a file or a URL', href: '/imports', icon: 'import' },
  { section: 'Actions', label: 'Rip a disc', href: '/rips', icon: 'rip' },
  { section: 'Actions', label: 'Create a burn job', href: '/burns/new', icon: 'burn' },
];

/** Whether every word of `query` appears in `text`, in any case. */
export function matches(text: string, query: string): boolean {
  const haystack = text.toLowerCase();
  return query
    .toLowerCase()
    .split(/\s+/)
    .filter(Boolean)
    .every((word) => haystack.includes(word));
}

/**
 * The options for `query`: actions and pages that match it (all of them for
 * an empty query), then the library's matches, which the server found.
 */
export function commandOptions(
  groups: readonly NavGroup[],
  query: string,
  library: readonly Command[] = [],
): Command[] {
  const pages: Command[] = groups.flatMap((group) =>
    group.items.map((item) => ({
      section: 'Go to' as const,
      label: item.label,
      detail: group.label ?? undefined,
      href: item.href,
      icon: item.icon,
    })),
  );
  const local = [...ACTIONS, ...pages].filter(
    (option) => !query.trim() || matches(`${option.label} ${option.detail ?? ''}`, query),
  );
  return [...local, ...library];
}
