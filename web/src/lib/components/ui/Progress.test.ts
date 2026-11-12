// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { render } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import { axeViolations } from '$lib/testing/axe';
import Progress from './Progress.svelte';

describe('Progress', () => {
  it('is a named progress bar with its value as a percentage', async () => {
    const { getByRole, container } = render(Progress, {
      value: 0.43,
      label: 'Writing',
      tone: 'laser',
    });
    const bar = getByRole('progressbar', { name: 'Writing' });
    expect(bar.getAttribute('aria-valuenow')).toBe('43');
    expect(bar.getAttribute('aria-valuetext')).toBe('43%');
    expect(await axeViolations(container)).toEqual([]);
  });

  it('keeps a value out of range within 0 and 100', () => {
    const over = render(Progress, { value: 1.7, label: 'Over' });
    expect(over.getByRole('progressbar').getAttribute('aria-valuenow')).toBe('100');
    over.unmount();
    const under = render(Progress, { value: -0.2, label: 'Under' });
    expect(under.getByRole('progressbar').getAttribute('aria-valuenow')).toBe('0');
  });

  it('says so when no progress has been reported, rather than showing nothing', async () => {
    const { getByRole, container } = render(Progress, { value: null, label: 'Waiting' });
    const bar = getByRole('progressbar', { name: 'Waiting' });
    expect(bar.hasAttribute('aria-valuenow')).toBe(false);
    expect(bar.getAttribute('aria-valuetext')).toBe('Not reported yet');
    expect(await axeViolations(container)).toEqual([]);
  });
});
