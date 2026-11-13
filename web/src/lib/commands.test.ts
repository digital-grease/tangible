// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
import { describe, expect, it } from 'vitest';
import { ACTIONS, commandOptions, matches, type Command } from './commands';
import { navFor } from './nav';

describe('the command bar', () => {
  it('offers the actions, then every page the person may see', () => {
    const forViewer = commandOptions(navFor('viewer'), '');
    expect(forViewer.slice(0, 3)).toEqual(ACTIONS);
    const pages = forViewer.filter((option) => option.section === 'Go to').map((o) => o.label);
    expect(pages).toEqual([
      'Overview',
      'Library',
      'Catalog',
      'Physical copies',
      'Requests',
      'Imports',
      'Rips',
      'Burns',
    ]);
    const forAdmin = commandOptions(navFor('administrator'), '');
    expect(forAdmin.some((option) => option.label === 'Audit log')).toBe(true);
  });

  it('matches every word of a query, in any case, against a label or its group', () => {
    expect(matches('Create a burn job', 'BURN')).toBe(true);
    expect(matches('Create a burn job', 'job create')).toBe(true);
    expect(matches('Create a burn job', 'rip')).toBe(false);
    const found = commandOptions(navFor('administrator'), 'admin').map((option) => option.label);
    expect(found).toContain('Storage');
    expect(found).not.toContain('Library');
  });

  it('puts what the library found after the rest', () => {
    const library: Command[] = [
      { section: 'Library', label: 'debian.iso', href: '/library/a1', icon: 'disc' },
    ];
    const options = commandOptions(navFor('viewer'), 'deb', library);
    expect(options.at(-1)).toEqual(library[0]);
    expect(options.filter((option) => option.section !== 'Library')).toEqual([]);
  });
});
