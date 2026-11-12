// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { render } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import { axeViolations } from '$lib/testing/axe';
import UiHarness from '$lib/testing/UiHarness.svelte';
import EmptyState from './EmptyState.svelte';

describe('EmptyState', () => {
  it('says there is nothing yet and offers the next thing to do', async () => {
    const { getByRole, getByText, container } = render(UiHarness, { show: 'empty' });
    expect(getByRole('heading', { name: 'No artifacts yet' })).toBeTruthy();
    expect(getByText(/Import an authorized disc image/)).toBeTruthy();
    expect(getByRole('link', { name: 'Import one' }).getAttribute('href')).toBe('/imports');
    expect(await axeViolations(container)).toEqual([]);
  });
});

describe('EmptyState inside a panel', () => {
  it('drops its box and takes the heading level it is given', () => {
    const { container, getByRole } = render(EmptyState, {
      title: 'No drives yet',
      inset: true,
      level: 3,
    });
    expect(getByRole('heading', { name: 'No drives yet', level: 3 })).toBeTruthy();
    expect(container.querySelector('.empty')?.classList.contains('inset')).toBe(true);
  });
});
