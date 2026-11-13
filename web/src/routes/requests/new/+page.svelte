<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Ask for an item (ADR 0021). The requester says what it is and attests,
  // by a box they tick themselves on every request, that they own it and
  // want a digital image of it. The server refuses a request without it and
  // does not keep it.
  //
  // Until metadata search is built (docs/plans/requests.md Q2), the item is
  // described by hand.
  import { goto } from '$app/navigation';
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import Callout from '$lib/components/ui/Callout.svelte';
  import Checkbox from '$lib/components/ui/Checkbox.svelte';
  import Field from '$lib/components/ui/Field.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import Select from '$lib/components/ui/Select.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import {
    ATTESTATION,
    DETAIL_FIELDS,
    KINDS,
    createRequest,
    detailsOf,
    imdbId,
    requestRefusal,
    type DetailName,
    type RequestRef,
  } from '$lib/requests';
  import type { Problem } from '$lib/session';

  /** As the server bounds them. */
  const MAX_TITLE = 300;
  const MAX_DETAIL = 200;
  const MAX_NOTE = 2000;

  let kind = $state('game');
  let title = $state('');
  let details = $state<Record<DetailName, string>>({
    platform: '',
    year: '',
    region: '',
    edition: '',
  });
  let imdb = $state('');
  let note = $state('');
  let attests = $state(false);
  let submitting = $state(false);
  let tried = $state(false);
  let problem = $state<Problem | null>(null);

  const screen = $derived(kind === 'movie' || kind === 'television');
  const imdbFound = $derived(screen ? imdbId(imdb) : null);
  const titleError = $derived(tried && !title.trim() ? 'Say what the item is.' : null);
  const imdbError = $derived(
    imdbFound === undefined
      ? 'Not an IMDb title ID (tt followed by digits) or IMDb address.'
      : null,
  );
  const attestError = $derived(tried && !attests ? 'A request cannot be made without this.' : null);

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (submitting) return;
    tried = true;
    problem = null;
    if (!title.trim() || imdbError || !attests) return;
    const refs: RequestRef[] = imdbFound ? [{ provider: 'imdb', external_id: imdbFound }] : [];
    submitting = true;
    const result = await createRequest({
      kind,
      title: title.trim(),
      details: detailsOf(details),
      refs,
      note: note.trim() || null,
      attests_ownership: attests,
    });
    submitting = false;
    if (result.kind === 'error') {
      problem = result.problem;
      return;
    }
    await goto(`/requests/${result.data.id}`);
  }
</script>

<svelte:head><title>Ask for an item — Tangible</title></svelte:head>

<PageHeader
  title="Ask for an item"
  crumbs={[{ label: 'Requests', href: '/requests' }, { label: 'Ask for an item' }]}
  description="Ask for a digital image of an item you own. An operator or administrator approves the request before anything is searched for or downloaded."
/>

<form class="layout" onsubmit={submit} novalidate>
  <section aria-labelledby="item-heading" class="panel">
    <h2 id="item-heading">The item</h2>
    <div class="fields">
      <Select label="Kind" options={KINDS} bind:value={kind} />
      <Field label="Title" required error={titleError} hint="As it appears on the item.">
        {#snippet children({ id, describedBy, invalid })}
          <input
            {id}
            bind:value={title}
            required
            maxlength={MAX_TITLE}
            autocomplete="off"
            aria-describedby={describedBy}
            aria-invalid={invalid || undefined}
          />
        {/snippet}
      </Field>
      <div class="pairs">
        {#each DETAIL_FIELDS as field (field.name)}
          <Field label={field.label} hint={field.hint}>
            {#snippet children({ id, describedBy })}
              <input
                {id}
                bind:value={details[field.name]}
                maxlength={MAX_DETAIL}
                autocomplete="off"
                aria-describedby={describedBy}
              />
            {/snippet}
          </Field>
        {/each}
      </div>
      {#if screen}
        <Field
          label="IMDb title"
          hint="Optional: its IMDb ID, such as tt0083658, or its IMDb address. It makes finding the right release likelier."
          error={imdbError}
        >
          {#snippet children({ id, describedBy, invalid })}
            <input
              {id}
              bind:value={imdb}
              autocomplete="off"
              spellcheck="false"
              aria-describedby={describedBy}
              aria-invalid={invalid || undefined}
            />
          {/snippet}
        </Field>
      {/if}
      <Field label="Note" hint="Optional: anything that helps, such as which disc of a set.">
        {#snippet children({ id, describedBy })}
          <textarea
            {id}
            bind:value={note}
            rows="3"
            maxlength={MAX_NOTE}
            aria-describedby={describedBy}></textarea>
        {/snippet}
      </Field>
    </div>
  </section>

  <section aria-labelledby="attest-heading" class="panel">
    <h2 id="attest-heading">Before you ask</h2>
    <Callout tone="info">
      Whether making a copy of an item is lawful where you are is for you to determine. Tangible
      does not check what you attest.
    </Callout>
    <div class="attest" class:invalid={!!attestError}>
      <Checkbox label={ATTESTATION} bind:checked={attests} />
      {#if attestError}<p class="error"><StatusMark tone="bad" label={attestError} /></p>{/if}
    </div>
    {#if problem}
      {@const advice = requestRefusal(problem.code)}
      {#if advice}<p class="advice">{advice}</p>{/if}
      <ErrorPanel {problem} area="other" endpoint="POST /api/v1/requests" level={3} refusal />
    {/if}
    <Button type="submit" variant="primary" icon="request" disabled={submitting}>
      {submitting ? 'Asking…' : 'Ask for this item'}
    </Button>
  </section>
</form>

<style>
  .layout {
    display: grid;
    grid-template-columns: minmax(0, 1.6fr) minmax(0, 1fr);
    gap: var(--space-4);
    align-items: start;
  }
  @media (max-width: 1099px) {
    .layout {
      grid-template-columns: minmax(0, 1fr);
    }
  }
  .panel {
    display: grid;
    gap: var(--space-3);
    padding: var(--space-3) var(--space-4) var(--space-4);
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--radius-lg);
  }
  .panel h2 {
    margin: 0;
    font-size: var(--text-base);
  }
  .fields {
    display: grid;
    gap: var(--space-3);
  }
  .pairs {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(14rem, 1fr));
    gap: var(--space-3);
  }
  .attest {
    padding: var(--space-3);
    border: 1px solid var(--line);
    border-radius: var(--radius);
  }
  .attest.invalid {
    border-color: var(--bad);
  }
  .error {
    margin: var(--space-2) 0 0 1.5rem;
    font-size: var(--text-sm);
  }
  .advice {
    margin: 0;
  }
</style>
