// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { createRawSnippet } from 'svelte';
import { fireEvent, render } from '@testing-library/svelte';
import { describe, expect, it, vi } from 'vitest';
import { axeViolations } from '$lib/testing/axe';
import Button from './Button.svelte';

const words = (text: string) => createRawSnippet(() => ({ render: () => `<span>${text}</span>` }));

describe('Button', () => {
  it('is a plain button by default, so it never submits a form by accident', async () => {
    const onclick = vi.fn();
    const { getByRole, container } = render(Button, { children: words('Show'), onclick });
    const button = getByRole('button', { name: 'Show' });
    expect(button.getAttribute('type')).toBe('button');
    expect(button.classList.contains('secondary')).toBe(true);
    await fireEvent.click(button);
    expect(onclick).toHaveBeenCalledOnce();
    expect(await axeViolations(container)).toEqual([]);
  });

  it('can submit when asked', () => {
    const { getByRole } = render(Button, {
      children: words('Sign in'),
      type: 'submit',
      variant: 'primary',
    });
    const button = getByRole('button', { name: 'Sign in' });
    expect(button.getAttribute('type')).toBe('submit');
    expect(button.classList.contains('primary')).toBe(true);
  });

  it('takes its name from its label when it shows only an icon', async () => {
    const { getByRole, container } = render(Button, { icon: 'more', label: 'More actions' });
    const button = getByRole('button', { name: 'More actions' });
    expect(button.getAttribute('title')).toBe('More actions');
    expect(button.classList.contains('icon-only')).toBe(true);
    expect(await axeViolations(container)).toEqual([]);
  });

  it('is a link when it goes somewhere', async () => {
    const { getByRole, container } = render(Button, {
      children: words('Import one'),
      href: '/imports',
      variant: 'primary',
    });
    const link = getByRole('link', { name: 'Import one' });
    expect(link.getAttribute('href')).toBe('/imports');
    expect(await axeViolations(container)).toEqual([]);
  });

  it('is natively disabled, which is what stops a browser clicking it', () => {
    // jsdom delivers a synthetic click even to a disabled button; a browser
    // does not deliver a person's. So the test is that the attribute is set.
    const { getByRole } = render(Button, {
      children: words('Delete this artifact'),
      variant: 'danger',
      disabled: true,
    });
    const button = getByRole('button', { name: 'Delete this artifact' }) as HTMLButtonElement;
    expect(button.disabled).toBe(true);
    expect(button.classList.contains('danger')).toBe(true);
  });
});
