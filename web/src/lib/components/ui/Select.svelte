<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts" module>
  export interface Choice {
    value: string;
    label: string;
  }
</script>

<script lang="ts">
  // A labelled native select: the commonest field, in one line.
  //
  //   <Select label="Format" options={formats} bind:value={format} />
  //
  // Native, so the browser and screen readers handle it as they always do.
  import Field from './Field.svelte';

  let {
    label,
    options,
    value = $bindable(''),
    hint,
    error,
    disabled = false,
    onchange,
  }: {
    label: string;
    options: readonly Choice[];
    value?: string;
    hint?: string;
    error?: string | null;
    disabled?: boolean;
    onchange?: (value: string) => void;
  } = $props();
</script>

<Field {label} {hint} {error}>
  {#snippet children({ id, describedBy, invalid })}
    <select
      {id}
      bind:value
      {disabled}
      aria-describedby={describedBy}
      aria-invalid={invalid || undefined}
      onchange={() => onchange?.(value)}
    >
      {#each options as option (option.value)}
        <option value={option.value}>{option.label}</option>
      {/each}
    </select>
  {/snippet}
</Field>
