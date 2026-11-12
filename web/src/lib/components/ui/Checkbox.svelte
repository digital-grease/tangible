<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // A checkbox with its words beside it, the whole line clickable, and an
  // optional hint under it.
  let {
    label,
    checked = $bindable(false),
    hint,
    disabled = false,
    onchange,
  }: {
    label: string;
    checked?: boolean;
    hint?: string;
    disabled?: boolean;
    onchange?: (checked: boolean) => void;
  } = $props();

  const uid = $props.id();
</script>

<div class="checkbox">
  <label>
    <input
      type="checkbox"
      bind:checked
      {disabled}
      aria-describedby={hint ? `${uid}-hint` : undefined}
      onchange={() => onchange?.(checked)}
    />
    <span>{label}</span>
  </label>
  {#if hint}<p class="hint" id="{uid}-hint">{hint}</p>{/if}
</div>

<style>
  label {
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    font-weight: var(--weight-medium);
    cursor: pointer;
  }
  .hint {
    margin: var(--space-1) 0 0 1.5rem;
    font-size: var(--text-sm);
    color: var(--text-muted);
  }
</style>
