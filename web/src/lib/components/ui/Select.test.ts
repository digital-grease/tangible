// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import { axeViolations } from '$lib/testing/axe';
import UiHarness from '$lib/testing/UiHarness.svelte';

describe('Select', () => {
  it('is a labelled native select whose choice binds back', async () => {
    const { getByLabelText, getByTestId, container } = render(UiHarness, { show: 'select' });
    const select = getByLabelText('Format') as HTMLSelectElement;
    expect([...select.options].map((option) => option.textContent)).toEqual([
      'Any',
      'ISO',
      'CUE/BIN',
    ]);
    await fireEvent.change(select, { target: { value: 'cue_bin' } });
    expect(getByTestId('format').textContent).toBe('cue_bin');
    expect(await axeViolations(container)).toEqual([]);
  });
});
