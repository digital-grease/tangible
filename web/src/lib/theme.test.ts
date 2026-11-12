// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { afterEach, describe, expect, it } from 'vitest';
import {
  THEME_CHOICES,
  THEME_KEY,
  applyTheme,
  isThemeChoice,
  loadTheme,
  saveTheme,
  themeLabel,
  type StorageLike,
} from './theme';

function memory(initial: Record<string, string> = {}): StorageLike & { data: Map<string, string> } {
  const data = new Map(Object.entries(initial));
  return {
    data,
    getItem: (key) => data.get(key) ?? null,
    setItem: (key, value) => void data.set(key, value),
    removeItem: (key) => void data.delete(key),
  };
}

/** Storage that refuses everything, as a private window or blocked site data can. */
const refusing: StorageLike = {
  getItem: () => {
    throw new Error('SecurityError');
  },
  setItem: () => {
    throw new Error('QuotaExceededError');
  },
  removeItem: () => {
    throw new Error('SecurityError');
  },
};

afterEach(() => {
  delete document.documentElement.dataset.theme;
});

describe('theme', () => {
  it('follows the system when nothing was chosen', () => {
    expect(loadTheme(memory())).toBe('system');
  });

  it('follows the system when there is no storage at all', () => {
    expect(loadTheme(null)).toBe('system');
  });

  it('reads back a remembered choice', () => {
    expect(loadTheme(memory({ [THEME_KEY]: 'dark' }))).toBe('dark');
    expect(loadTheme(memory({ [THEME_KEY]: 'light' }))).toBe('light');
  });

  it('ignores a stored value it does not know', () => {
    expect(loadTheme(memory({ [THEME_KEY]: 'sepia' }))).toBe('system');
  });

  it('follows the system when storage refuses to be read', () => {
    expect(loadTheme(refusing)).toBe('system');
  });

  it('remembers a choice, and following the system as no choice', () => {
    const storage = memory();
    saveTheme('dark', storage);
    expect(storage.data.get(THEME_KEY)).toBe('dark');
    saveTheme('system', storage);
    expect(storage.data.has(THEME_KEY)).toBe(false);
  });

  it('does not throw when storage refuses to be written', () => {
    expect(() => saveTheme('light', refusing)).not.toThrow();
    expect(() => saveTheme('system', refusing)).not.toThrow();
  });

  it('sets the root element for a choice and clears it to follow the system', () => {
    const root = document.documentElement;
    applyTheme('dark', root);
    expect(root.dataset.theme).toBe('dark');
    applyTheme('light', root);
    expect(root.dataset.theme).toBe('light');
    applyTheme('system', root);
    expect(root.dataset.theme).toBeUndefined();
  });

  it('names every choice, and knows only those', () => {
    for (const choice of THEME_CHOICES) {
      expect(isThemeChoice(choice)).toBe(true);
      expect(themeLabel(choice)).not.toBe('');
    }
    expect(isThemeChoice('auto')).toBe(false);
    expect(isThemeChoice(null)).toBe(false);
  });
});
