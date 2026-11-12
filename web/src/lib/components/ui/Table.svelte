<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // A data table that scrolls sideways inside itself, never the page
  // (docs/plans/ui-overhaul.md, decision 10).
  //
  // The page writes the thead and tbody; this gives them the dense style,
  // a caption (hidden unless `showCaption`), and a scrolling area a keyboard
  // can reach and a screen reader can name. The area is a named group, not
  // a landmark: a table usually sits in a Panel that is already a region of
  // the same name, and two landmarks of one name help nobody. A cell or
  // heading with class `num` is right-aligned with tabular figures. With
  // `maxHeight` the area also scrolls down, and the headings stay in view.
  import type { Snippet } from 'svelte';

  let {
    caption,
    showCaption = false,
    maxHeight,
    children,
  }: {
    /** What the table holds; its caption and the scrolling region's name. */
    caption: string;
    showCaption?: boolean;
    /** A CSS length, such as `24rem`. */
    maxHeight?: string;
    children: Snippet;
  } = $props();
</script>

<!-- Focusable so a keyboard can scroll it (axe's scrollable-region-focusable
     rule, WCAG 2.1.1), and named so that stop makes sense to a screen reader.
     Svelte's lint does not know scrolling areas; the stop is deliberate. -->
<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
<div
  class="table-region"
  class:scrolls={!!maxHeight}
  role="group"
  aria-label={caption}
  tabindex="0"
  style:max-height={maxHeight}
>
  <table>
    <caption class:visually-hidden={!showCaption}>{caption}</caption>
    {@render children()}
  </table>
</div>

<style>
  .table-region {
    overflow-x: auto;
    max-width: 100%;
  }
  .table-region.scrolls {
    overflow-y: auto;
  }
  table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--text-sm);
  }
  caption {
    text-align: left;
    font-weight: var(--weight-strong);
    padding: var(--space-2) var(--space-3);
  }
  .table-region :global(th),
  .table-region :global(td) {
    padding: 0.4375rem var(--space-3);
    border-bottom: 1px solid var(--line-2);
    text-align: left;
    vertical-align: middle;
  }
  .table-region :global(thead th) {
    background: var(--surface-2);
    border-bottom-color: var(--line);
    color: var(--text-muted);
    font-size: 0.6875rem;
    font-weight: var(--weight-strong);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    white-space: nowrap;
  }
  .scrolls :global(thead th) {
    position: sticky;
    top: 0;
    z-index: 1;
  }
  .table-region :global(tbody tr:hover) {
    background: color-mix(in srgb, var(--accent) 5%, transparent);
  }
  .table-region :global(tbody th) {
    font-weight: var(--weight-strong);
  }
  .table-region :global(.num) {
    text-align: right;
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
  }
  .table-region :global(tbody tr:last-child > *) {
    border-bottom: 0;
  }
</style>
