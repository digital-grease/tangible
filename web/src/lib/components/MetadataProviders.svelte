<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // The metadata providers people search to find an item to ask for (ADR
  // 0021): MusicBrainz, TMDB and IGDB, one of each, on the Integrations
  // page. Each is added with its key, if it takes one, which is typed, sent
  // and never shown again; tried; turned off and on; given a new key; or
  // removed, which asks first.
  import ErrorPanel from './ErrorPanel.svelte';
  import Button from './ui/Button.svelte';
  import Callout from './ui/Callout.svelte';
  import Confirm from './ui/Confirm.svelte';
  import Field from './ui/Field.svelte';
  import Panel from './ui/Panel.svelte';
  import StatusMark from './ui/StatusMark.svelte';
  import Table from './ui/Table.svelte';
  import {
    PROVIDER_KINDS,
    createIntegration,
    createProviderRequest,
    deleteIntegration,
    integrationRefusal,
    integrationStatusLabel,
    integrationStatusTone,
    providerReady,
    providerTestOutcome,
    providerWording,
    secretsLabel,
    testIntegration,
    updateIntegration,
    updateProviderRequest,
    type IntegrationView,
    type ProviderKind,
  } from '$lib/integrations';
  import type { Problem } from '$lib/session';

  let {
    items,
    onchanged,
  }: {
    /** Every integration; the providers among them are shown. */
    items: IntegrationView[];
    /** Called after anything changed, to load the list again. */
    onchanged: () => Promise<void>;
  } = $props();

  /** The provider whose form is open, and whether it adds or changes a key. */
  let open = $state<{ kind: ProviderKind; item: IntegrationView | null } | null>(null);
  let secrets = $state<Record<string, string>>({});
  let working = $state(false);
  let refusal = $state<string | null>(null);
  let said = $state<{ text: string; ok: boolean } | null>(null);
  let removing = $state<IntegrationView | null>(null);
  let removeProblem = $state<Problem | null>(null);

  const configured = (kind: ProviderKind) => items.find((item) => item.kind === kind) ?? null;
  const wording = $derived(open ? providerWording(open.kind) : null);
  /** Whether the open form adds a provider, rather than changing a key. */
  const adding = $derived(open !== null && open.item === null);

  function openForm(kind: ProviderKind, item: IntegrationView | null) {
    open = { kind, item };
    secrets = Object.fromEntries(providerWording(kind).secrets.map((s) => [s.field, '']));
    refusal = null;
    said = null;
  }

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (!open || working) return;
    working = true;
    refusal = null;
    const { kind, item } = open;
    const result = item
      ? await updateIntegration(item.id, updateProviderRequest(secrets))
      : await createIntegration(createProviderRequest(kind, secrets));
    working = false;
    // Whatever happened, what was typed is not kept in the page.
    secrets = Object.fromEntries(Object.keys(secrets).map((field) => [field, '']));
    if (result.kind === 'error') {
      refusal = integrationRefusal(result.problem);
      return;
    }
    said = {
      text: item
        ? `Saved ${result.data.name}'s key.`
        : `Added ${result.data.name}. Try it to check it answers.`,
      ok: true,
    };
    open = null;
    await onchanged();
  }

  async function test(item: IntegrationView) {
    working = true;
    said = null;
    const result = await testIntegration(item.id);
    working = false;
    said =
      result.kind === 'ok'
        ? { text: providerTestOutcome(item.name, result.data), ok: result.data.ok }
        : { text: `${item.name} could not be tried: ${result.problem.detail}`, ok: false };
    await onchanged();
  }

  async function toggle(item: IntegrationView) {
    working = true;
    said = null;
    const result = await updateIntegration(item.id, { enabled: !item.enabled });
    working = false;
    said =
      result.kind === 'ok'
        ? {
            text: result.data.enabled
              ? `${item.name} is turned on.`
              : `${item.name} is turned off: searches do not ask it.`,
            ok: true,
          }
        : { text: result.problem.detail, ok: false };
    await onchanged();
  }

  async function remove(item: IntegrationView) {
    removeProblem = null;
    const result = await deleteIntegration(item.id);
    if (result.kind === 'error') {
      removeProblem = result.problem;
      return;
    }
    removing = null;
    if (open?.item?.id === item.id) open = null;
    said = { text: `Removed ${item.name}.`, ok: true };
    await onchanged();
  }
</script>

<Panel title="Metadata providers" flush>
  <p class="intro">
    Searched when somebody looks for an item to ask for, alongside the DATs loaded here. A search
    sends a provider only the words searched for. None is asked until it is added.
  </p>
  <Table caption="Metadata providers">
    <thead>
      <tr>
        <th scope="col">Provider</th>
        <th scope="col">Status</th>
        <th scope="col">Key</th>
        <th scope="col"><span class="visually-hidden">Actions</span></th>
      </tr>
    </thead>
    <tbody>
      {#each PROVIDER_KINDS as kind (kind)}
        {@const item = configured(kind)}
        {@const words = providerWording(kind)}
        <tr>
          <th scope="row">
            {words.label}
            <span class="sub">{words.covers}</span>
          </th>
          <td>
            {#if item}
              <StatusMark tone={integrationStatusTone(item)} label={integrationStatusLabel(item)} />
              {#if item.last_error_code}<code class="small">{item.last_error_code}</code>{/if}
            {:else}
              <span class="quiet">Not added</span>
            {/if}
          </td>
          <td>{item ? secretsLabel(item) : '·'}</td>
          <td>
            <div class="actions">
              {#if item}
                <Button
                  size="sm"
                  onclick={() => test(item)}
                  disabled={working || !item.enabled || !providerReady(item)}
                  label={`Test ${item.name}`}>Test</Button
                >
                <Button
                  size="sm"
                  variant="quiet"
                  onclick={() => toggle(item)}
                  disabled={working}
                  label={`${item.enabled ? 'Turn off' : 'Turn on'} ${item.name}`}
                  >{item.enabled ? 'Turn off' : 'Turn on'}</Button
                >
                {#if words.secrets.length > 0}
                  <Button
                    size="sm"
                    variant="quiet"
                    onclick={() => openForm(kind, item)}
                    label={`Change ${item.name}'s key`}>Change key</Button
                  >
                {/if}
                <Button
                  size="sm"
                  variant="danger"
                  onclick={() => {
                    removing = item;
                    removeProblem = null;
                  }}
                  label={`Remove ${item.name}`}>Remove</Button
                >
              {:else}
                <Button size="sm" icon="plus" onclick={() => openForm(kind, null)}
                  >Add {words.label}</Button
                >
              {/if}
            </div>
          </td>
        </tr>
      {/each}
    </tbody>
  </Table>

  {#if removing}
    {@const item = removing}
    <div class="padded">
      <Confirm title={`Remove ${item.name}?`}>
        <p>
          Its key is deleted, and searches no longer ask it. Requests already made keep what they
          found.
        </p>
        {#if removeProblem}<ErrorPanel problem={removeProblem} area="other" refusal />{/if}
        {#snippet actions()}
          <Button variant="danger" onclick={() => remove(item)}>Yes, remove {item.name}</Button>
          <Button onclick={() => (removing = null)}>Keep it</Button>
        {/snippet}
      </Confirm>
    </div>
  {/if}

  {#if open && wording}
    <form class="padded form" onsubmit={submit} aria-labelledby="provider-form-heading">
      <h3 id="provider-form-heading">
        {open.item ? `A new key for ${wording.label}` : `Add ${wording.label}`}
      </h3>
      <p class="quiet">{wording.getting}</p>
      {#each wording.secrets as secret (secret.field)}
        <Field label={secret.label} hint={secret.hint || undefined} required={adding}>
          {#snippet children({ id, describedBy })}
            <input
              {id}
              type="password"
              autocomplete="off"
              spellcheck="false"
              bind:value={secrets[secret.field]}
              required={adding}
              aria-describedby={describedBy}
            />
          {/snippet}
        </Field>
      {/each}
      {#if refusal}<Callout tone="bad"><p>{refusal}</p></Callout>{/if}
      <div class="actions">
        <Button type="submit" variant="primary" disabled={working}>
          {working ? 'Saving…' : open.item ? 'Save the key' : `Add ${wording.label}`}
        </Button>
        <Button onclick={() => (open = null)} disabled={working}>Cancel</Button>
      </div>
    </form>
  {/if}

  <div aria-live="polite">
    {#if said}
      <p class="padded said"><StatusMark tone={said.ok ? 'ok' : 'bad'} label={said.text} /></p>
    {/if}
  </div>
</Panel>

<style>
  .intro {
    margin: 0;
    padding: var(--space-3) var(--space-4);
    color: var(--text-muted);
    max-width: 75ch;
  }
  .sub {
    display: block;
    font-weight: var(--weight-regular);
    font-size: var(--text-sm);
    color: var(--text-muted);
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    align-items: center;
  }
  .padded {
    padding: var(--space-3) var(--space-4);
  }
  .form {
    display: grid;
    gap: var(--space-3);
    max-width: 40rem;
    border-top: 1px solid var(--line);
  }
  .form h3 {
    margin: 0;
    font-size: var(--text-base);
  }
  .quiet {
    color: var(--text-muted);
    margin: 0;
  }
  .small {
    font-size: var(--text-xs);
  }
  .said {
    margin: 0;
  }
  .said :global(.mark) {
    white-space: normal;
  }
</style>
