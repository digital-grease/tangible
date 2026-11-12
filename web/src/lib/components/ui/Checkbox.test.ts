// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import { axeViolations } from '$lib/testing/axe';
import UiHarness from '$lib/testing/UiHarness.svelte';

describe('Checkbox', () => {
  it('toggles from its words, binds back, and is described by its hint', async () => {
    const { getByLabelText, getByText, getByTestId, container } = render(UiHarness, {
      show: 'checkbox',
    });
    const box = getByLabelText('Only deleted artifacts') as HTMLInputElement;
    expect(box.checked).toBe(false);
    await fireEvent.click(getByText('Only deleted artifacts'));
    expect(box.checked).toBe(true);
    expect(getByTestId('deleted').textContent).toBe('yes');
    const hint = document.getElementById(box.getAttribute('aria-describedby') ?? '');
    expect(hint?.textContent).toBe('Kept until collected.');
    expect(await axeViolations(container)).toEqual([]);
  });
});
