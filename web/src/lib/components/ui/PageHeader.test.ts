// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { render } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import { axeViolations } from '$lib/testing/axe';
import UiHarness from '$lib/testing/UiHarness.svelte';

describe('PageHeader', () => {
  it('names the page, says where it sits, and holds its actions', async () => {
    const { getByRole, container } = render(UiHarness, { show: 'header' });
    expect(getByRole('heading', { level: 1, name: 'homebrew-comp-vol2.cue' })).toBeTruthy();
    const trail = getByRole('navigation', { name: 'Breadcrumb' });
    expect(trail.querySelector('a')?.getAttribute('href')).toBe('/library');
    expect(trail.querySelector('[aria-current="page"]')?.textContent).toBe(
      'homebrew-comp-vol2.cue',
    );
    expect(getByRole('button', { name: 'Create burn job' })).toBeTruthy();
    expect(getByRole('button', { name: 'More actions' })).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);
  });
});
