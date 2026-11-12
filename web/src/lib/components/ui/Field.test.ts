// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { render } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import { axeViolations } from '$lib/testing/axe';
import UiHarness from '$lib/testing/UiHarness.svelte';

/** The text of every element an element's aria-describedby names. */
function description(element: HTMLElement): string {
  return (element.getAttribute('aria-describedby') ?? '')
    .split(' ')
    .filter(Boolean)
    .map((id) => document.getElementById(id)?.textContent?.trim() ?? '')
    .join(' ');
}

describe('Field', () => {
  it('labels its control and describes it by its hint', async () => {
    const { getByLabelText, container } = render(UiHarness, { show: 'field' });
    const input = getByLabelText(/Name/) as HTMLInputElement;
    expect(input.tagName).toBe('INPUT');
    expect(description(input)).toBe('Part of the file name; case does not matter.');
    expect(input.hasAttribute('aria-invalid')).toBe(false);
    expect(await axeViolations(container)).toEqual([]);
  });

  it('says what is wrong in words, and marks the control invalid', async () => {
    const { getByLabelText, getByText, container } = render(UiHarness, {
      show: 'field',
      fieldError: 'At most 200 characters.',
    });
    const input = getByLabelText(/Name/);
    expect(input.getAttribute('aria-invalid')).toBe('true');
    expect(getByText('At most 200 characters.')).toBeTruthy();
    expect(description(input)).toContain('At most 200 characters.');
    expect(await axeViolations(container)).toEqual([]);
  });
});
