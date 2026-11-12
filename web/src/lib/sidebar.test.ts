// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import { afterEach, describe, expect, it, vi } from 'vitest';
import { SIDEBAR_KEY, loadSidebar, saveSidebar, sidebarCompact, toggledSidebar } from './sidebar';

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('the sidebar', () => {
  it('shows icons only below 1200 pixels when left alone', () => {
    expect(sidebarCompact('auto', 1440)).toBe(false);
    expect(sidebarCompact('auto', 1199)).toBe(true);
    expect(sidebarCompact('auto', 900)).toBe(true);
  });

  it('follows the choice at any width from 900 up', () => {
    expect(sidebarCompact('full', 1000)).toBe(false);
    expect(sidebarCompact('compact', 1440)).toBe(true);
  });

  it('is a drawer below 900 pixels, whatever was chosen', () => {
    expect(sidebarCompact('compact', 899)).toBe(false);
    expect(sidebarCompact('auto', 600)).toBe(false);
  });

  it('turns the other way from how it is', () => {
    expect(toggledSidebar(true)).toBe('full');
    expect(toggledSidebar(false)).toBe('compact');
  });

  it('remembers the choice, and is left alone without storage', () => {
    const stored = new Map<string, string>();
    vi.stubGlobal('localStorage', {
      getItem: (key: string) => stored.get(key) ?? null,
      setItem: (key: string, value: string) => void stored.set(key, value),
    });
    expect(loadSidebar()).toBe('auto');
    saveSidebar('full');
    expect(stored.get(SIDEBAR_KEY)).toBe('full');
    expect(loadSidebar()).toBe('full');
    stored.set(SIDEBAR_KEY, 'sideways');
    expect(loadSidebar()).toBe('auto');

    vi.stubGlobal('localStorage', {
      getItem: () => {
        throw new Error('denied');
      },
      setItem: () => {
        throw new Error('denied');
      },
    });
    expect(loadSidebar()).toBe('auto');
    expect(() => saveSidebar('compact')).not.toThrow();
  });
});
