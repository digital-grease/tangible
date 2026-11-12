<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts" module>
  /** What a field hands its control. */
  export interface ControlIds {
    /** For the control's `id`; the label points at it. */
    id: string;
    /** For the control's `aria-describedby`. */
    describedBy: string | undefined;
    /** For the control's `aria-invalid`. */
    invalid: boolean;
  }
</script>

<script lang="ts">
  // A labelled form control with its hint and its error.
  //
  // The control itself is the page's, rendered by the `children` snippet,
  // which is handed the id the label points at and the ids of the hint and
  // error to describe it by; so any input, select or textarea fits, and the
  // page keeps its own binding. The error is said in words with a mark, never
  // by a red border alone.
  import type { Snippet } from 'svelte';
  import StatusMark from './StatusMark.svelte';

  let {
    label,
    hint,
    error,
    required = false,
    children,
  }: {
    label: string;
    hint?: string;
    /** What is wrong with the value, or nothing. */
    error?: string | null;
    /** Marks the label; the control still needs its own `required`. */
    required?: boolean;
    children: Snippet<[ControlIds]>;
  } = $props();

  const uid = $props.id();
  const id = `${uid}-control`;
  const describedBy = $derived(
    [hint ? `${uid}-hint` : null, error ? `${uid}-error` : null].filter(Boolean).join(' ') ||
      undefined,
  );
</script>

<div class="field" class:invalid={!!error}>
  <label for={id}>
    {label}{#if required}<span class="required" aria-hidden="true">*</span>{/if}
  </label>
  {@render children({ id, describedBy, invalid: !!error })}
  {#if hint}<p class="hint" id="{uid}-hint">{hint}</p>{/if}
  {#if error}<p class="error" id="{uid}-error"><StatusMark tone="bad" label={error} /></p>{/if}
</div>

<style>
  .field {
    display: grid;
    gap: var(--space-1);
    align-content: start;
    min-width: 0;
  }
  label {
    font-weight: var(--weight-medium);
  }
  .required {
    margin-left: 0.2em;
    color: var(--bad);
  }
  .hint,
  .error {
    margin: 0;
    font-size: var(--text-sm);
  }
  .hint {
    color: var(--text-muted);
  }
  .invalid :global(input),
  .invalid :global(select),
  .invalid :global(textarea) {
    border-color: var(--bad);
  }
</style>
