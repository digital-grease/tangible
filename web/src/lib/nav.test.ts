// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
import { describe, expect, it } from 'vitest';
import { NAV, countLabel, counts, isCurrent, navFor, whereAmI } from './nav';
import { ICONS } from './components/ui/icons';

describe('the navigation', () => {
  it('shows the administrators their group, and nobody else', () => {
    const labels = (role: string) => navFor(role).map((group) => group.label);
    expect(labels('administrator')).toEqual([null, 'Collection', 'Activity', 'Administration']);
    expect(labels('operator')).toEqual([null, 'Collection', 'Activity']);
    expect(labels('viewer')).toEqual([null, 'Collection', 'Activity']);
  });

  it('uses the renamed pages and only icons that exist', () => {
    const items = NAV.flatMap((group) => group.items);
    expect(items[0]).toMatchObject({ href: '/', label: 'Overview' });
    expect(items.find((item) => item.href === '/discs')?.label).toBe('Physical copies');
    for (const item of items) expect(ICONS[item.icon]).toBeTruthy();
    expect(new Set(items.map((item) => item.href)).size).toBe(items.length);
  });

  it('marks a page current, and the page above a detail page', () => {
    expect(isCurrent({ href: '/' }, '/')).toBe(true);
    expect(isCurrent({ href: '/' }, '/library')).toBe(false);
    expect(isCurrent({ href: '/library' }, '/library')).toBe(true);
    expect(isCurrent({ href: '/library' }, '/library/0193a51c')).toBe(true);
    expect(isCurrent({ href: '/burns' }, '/burns/new')).toBe(true);
    expect(isCurrent({ href: '/dats' }, '/datsheet')).toBe(false);
  });

  it('says where a page sits, for the top bar', () => {
    expect(whereAmI('/library/0193a51c')).toMatchObject({
      group: 'Collection',
      item: { label: 'Library' },
    });
    expect(whereAmI('/')).toMatchObject({ group: null, item: { label: 'Overview' } });
    expect(whereAmI('/login')).toBeNull();
  });

  it('counts what is under way or waiting, in figures and in words', () => {
    const figures = counts({
      imports: { active: 2, failed: 1 },
      burns: { active: 1, needing_attention: 1, queued: 3, waiting_for_media: 1 },
    });
    expect(figures).toEqual({ imports: 2, burns: 5 });
    expect(countLabel('imports', 2)).toBe('2 under way');
    expect(countLabel('burns', 5)).toBe('5 queued, waiting for a disc or under way');
  });
});
