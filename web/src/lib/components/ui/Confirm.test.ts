// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render } from '@testing-library/svelte';
import { describe, expect, it, vi } from 'vitest';
import { axeViolations } from '$lib/testing/axe';
import UiHarness from '$lib/testing/UiHarness.svelte';

describe('Confirm', () => {
  it('takes the keyboard to its question, and does nothing until answered', async () => {
    const onselect = vi.fn();
    const { getByRole, queryByRole, container } = render(UiHarness, {
      show: 'confirm',
      onselect,
    });
    expect(queryByRole('form')).toBeNull();
    await fireEvent.click(getByRole('button', { name: 'Erase the disc in sr0…' }));

    const heading = getByRole('heading', { name: 'Erase the disc in sr0' });
    await vi.waitFor(() => expect(document.activeElement).toBe(heading));
    expect(getByRole('form', { name: 'Erase the disc in sr0' })).toBeTruthy();
    expect(await axeViolations(container)).toEqual([]);

    const submit = getByRole('button', { name: 'Erase the disc in sr0' }) as HTMLButtonElement;
    expect(submit.disabled).toBe(true);
    await fireEvent.click(
      getByRole('checkbox', { name: 'I understand everything on the disc will be destroyed' }),
    );
    await fireEvent.click(submit);
    expect(onselect).toHaveBeenCalledWith('erase');

    await fireEvent.click(getByRole('button', { name: 'Keep the disc' }));
    expect(queryByRole('form')).toBeNull();
  });
});
