// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later
// @vitest-environment jsdom
import { fireEvent, render } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import { axeViolations } from '$lib/testing/axe';
import Thumbnail from './Thumbnail.svelte';

const AT = '/api/v1/metadata/thumbnails/0123456789abcdef0123456789abcdef';
const OTHER = '/api/v1/metadata/thumbnails/fedcba9876543210fedcba9876543210';

describe('Thumbnail', () => {
  it("shows the picture at Tangible's address, as decoration", async () => {
    const { container } = render(Thumbnail, { url: AT });
    const img = container.querySelector('img.thumbnail');
    expect(img?.getAttribute('src')).toBe(AT);
    expect(img?.getAttribute('alt')).toBe('');
    expect(img?.getAttribute('loading')).toBe('lazy');
    expect(await axeViolations(container)).toEqual([]);
  });

  it('draws a disc for a result with no picture', async () => {
    for (const url of [null, undefined, '']) {
      const { container, unmount } = render(Thumbnail, { url });
      expect(container.querySelector('img')).toBeNull();
      expect(container.querySelector('.thumbnail.none svg[aria-hidden="true"]')).not.toBeNull();
      expect(await axeViolations(container)).toEqual([]);
      unmount();
    }
  });

  it('draws a disc when the provider has none, and tries another address afresh', async () => {
    const { container, rerender } = render(Thumbnail, { url: AT });
    await fireEvent.error(container.querySelector('img') as HTMLImageElement);
    expect(container.querySelector('img')).toBeNull();
    expect(container.querySelector('.thumbnail.none')).not.toBeNull();
    await rerender({ url: OTHER });
    expect(container.querySelector('img')?.getAttribute('src')).toBe(OTHER);
  });
});
