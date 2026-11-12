// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { createRawSnippet } from 'svelte';
import { render } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import { axeViolations } from '$lib/testing/axe';
import Panel from './Panel.svelte';

const body = createRawSnippet(() => ({ render: () => '<p>Nothing needs you.</p>' }));

describe('Panel', () => {
  it('is a region named by its heading when it has a title', async () => {
    const { getByRole, container } = render(Panel, {
      title: 'Needs attention',
      aside: '3',
      level: 3,
      children: body,
    });
    expect(getByRole('heading', { level: 3, name: /Needs attention/ })).toBeTruthy();
    expect(getByRole('region', { name: /Needs attention/ })).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);
  });

  it('is only a surface without one', () => {
    const { queryByRole, getByText } = render(Panel, { children: body });
    expect(queryByRole('region')).toBeNull();
    expect(getByText('Nothing needs you.')).toBeTruthy();
  });
});
