<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // An item's cover (ADR 0023), as Tangible fetched and checked it: never
  // loaded from a provider, so the address is always Tangible's own. Beside
  // the item's name, it says nothing more to a screen reader, and is hidden
  // from one; standing alone, it is given a label.
  let {
    url,
    size = 'small',
    label,
  }: {
    /** Tangible's address for it, from a `cover` field. */
    url: string;
    /** `small` for a row, `large` for a page's header. */
    size?: 'small' | 'large';
    /** What it is a cover of, when no name beside it says so. */
    label?: string;
  } = $props();
</script>

<img
  class="cover {size}"
  src={url}
  alt={label ?? ''}
  loading="lazy"
  decoding="async"
  width={size === 'large' ? 120 : 32}
  height={size === 'large' ? 180 : 48}
/>

<style>
  .cover {
    flex: none;
    object-fit: cover;
    background: var(--surface-2);
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
  }
  .small {
    width: 2rem;
    height: 3rem;
  }
  .large {
    width: 7.5rem;
    height: 11.25rem;
    border-radius: var(--radius);
  }
</style>
