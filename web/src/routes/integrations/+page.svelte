<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Integrations, for administrators (ADR 0016; UX.md 13). A download
  // client (qBittorrent, SABnzbd, NZBGet), S3-compatible storage or RomM's
  // API is configured here; a password, API key or token is typed, sent, and
  // never shown again. The RomM export is listed, and kept by its own
  // setting. Laid out in the new design (docs/plans/ui-overhaul.md, U8): the
  // list in one panel, the form in another; removing asks first, under the
  // list, and takes the keyboard to the question.
  import Confirm from '$lib/components/ui/Confirm.svelte';
  import Callout from '$lib/components/ui/Callout.svelte';
  import { onMount, tick } from 'svelte';
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import Checkbox from '$lib/components/ui/Checkbox.svelte';
  import EmptyState from '$lib/components/ui/EmptyState.svelte';
  import Field from '$lib/components/ui/Field.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import Panel from '$lib/components/ui/Panel.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import Table from '$lib/components/ui/Table.svelte';
  import { when } from '$lib/overview';
  import { loadImportSources } from '$lib/imports';
  import {
    CLIENT_KINDS,
    afterImportWording,
    clientFormFrom,
    clientWording,
    createClientRequest,
    createIntegration,
    deleteIntegration,
    emptyClientForm,
    integrationRefusal,
    integrationStatusLabel,
    integrationStatusTone,
    isClientKind,
    kindLabel,
    loadIntegrations,
    removalBlocked,
    removesAfterImport,
    secretsLabel,
    testIntegration,
    testOutcome,
    updateClientRequest,
    updateIntegration,
    versionLabel,
    type ClientForm,
    type IntegrationCheckView,
    type IntegrationView,
  } from '$lib/integrations';
  import type { Problem } from '$lib/session';

  type View =
    | { kind: 'loading' }
    | { kind: 'ready'; items: IntegrationView[]; keyLoaded: boolean }
    | { kind: 'forbidden' }
    | { kind: 'error'; problem: Problem };

  let view = $state<View>({ kind: 'loading' });
  /** The watched roots a download folder may be mapped to; `null` if unknown. */
  let roots = $state<string[] | null>(null);
  /** Those Tangible may delete imported Usenet downloads in. */
  let removableRoots = $state<string[]>([]);

  /** The integration being edited, or `null` while adding one. */
  let editing = $state<IntegrationView | null>(null);
  let form = $state<ClientForm>(emptyClientForm('qbittorrent', []));
  let submitting = $state(false);
  let refusal = $state<string | null>(null);
  let done = $state<string | null>(null);

  const wording = $derived(clientWording(form.kind));
  const after = $derived(afterImportWording(form.kind));
  /** Why removing is not offered, when it is not. */
  const blocked = $derived(removalBlocked(form, removableRoots));
  // A root changed to one Tangible may not delete in takes the choice back,
  // once the roots are known.
  $effect(() => {
    if (roots !== null && blocked !== null && form.afterImport === 'remove') {
      form.afterImport = 'keep';
    }
  });
  /** What the secret is called in sentences: "password" or "API key". */
  const secretNoun = $derived(
    form.kind === 's3'
      ? 'keys'
      : wording.secretField === 'api_key'
        ? 'API key'
        : wording.secretField === 'token'
          ? 'token'
          : 'password',
  );

  /** The integration being tried, and what trying said. */
  let testing = $state<string | null>(null);
  let tested = $state<{
    id: string;
    text: string;
    ok: boolean;
    checks: IntegrationCheckView[];
  } | null>(null);

  /** Removing is a second, deliberate action. */
  let confirmingRemove = $state<string | null>(null);
  let removeProblem = $state<Problem | null>(null);
  const removing = $derived(
    view.kind === 'ready'
      ? (view.items.find((item) => item.id === confirmingRemove) ?? null)
      : null,
  );

  function startRemoving(item: IntegrationView) {
    confirmingRemove = item.id;
    removeProblem = null;
  }

  /** The form, to take the keyboard there when an integration is to be edited. */
  let formHeading = $state<HTMLElement | null>(null);

  async function refresh() {
    const result = await loadIntegrations();
    if (result.kind === 'ok') {
      view = {
        kind: 'ready',
        items: result.data.items,
        keyLoaded: result.data.secret_key_loaded,
      };
    } else if (result.problem.code === 'FORBIDDEN') {
      view = { kind: 'forbidden' };
    } else {
      view = { kind: 'error', problem: result.problem };
    }
  }

  onMount(async () => {
    const [, sources] = await Promise.all([refresh(), loadImportSources()]);
    roots = sources.kind === 'ready' ? sources.data.watch_roots : null;
    // A server from before removal says nothing: none.
    removableRoots = sources.kind === 'ready' ? (sources.data.removable_roots ?? []) : [];
    if (!editing) form = emptyClientForm(form.kind, roots ?? []);
  });

  function startEditing(item: IntegrationView) {
    const kind = item.kind;
    if (!isClientKind(kind)) return;
    editing = item;
    form = clientFormFrom({ ...item, kind });
    refusal = null;
    done = null;
    void tick().then(() => formHeading?.focus());
  }

  function startAdding() {
    editing = null;
    form = emptyClientForm(form.kind, roots ?? []);
    refusal = null;
  }

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (submitting) return;
    submitting = true;
    refusal = null;
    done = null;
    const result = editing
      ? await updateIntegration(editing.id, updateClientRequest(form))
      : await createIntegration(createClientRequest(form));
    submitting = false;
    // Whatever happened, the secrets typed are not kept in the page.
    form.secret = '';
    form.accessKey = '';
    form.clearSecret = false;
    if (result.kind === 'error') {
      refusal = integrationRefusal(result.problem);
      return;
    }
    done = editing ? `Saved ${result.data.name}.` : `Added ${result.data.name}.`;
    editing = null;
    form = emptyClientForm(form.kind, roots ?? []);
    await refresh();
  }

  async function test(item: IntegrationView) {
    testing = item.id;
    tested = null;
    const result = await testIntegration(item.id);
    testing = null;
    tested =
      result.kind === 'ok'
        ? {
            id: item.id,
            text: testOutcome(item.name, result.data, item.kind),
            ok: result.data.ok,
            checks: result.data.checks,
          }
        : {
            id: item.id,
            text: `${item.name} could not be tried: ${result.problem.detail}`,
            ok: false,
            checks: [],
          };
    await refresh();
  }

  async function remove(item: IntegrationView) {
    removeProblem = null;
    const result = await deleteIntegration(item.id);
    if (result.kind === 'error') {
      removeProblem = result.problem;
      return;
    }
    confirmingRemove = null;
    if (editing?.id === item.id) startAdding();
    done = `Removed ${item.name}.`;
    await refresh();
  }

  /** The full date and time, for a time's tooltip. */
  function full(value: string): string {
    return new Date(value).toLocaleString();
  }
</script>

<svelte:head><title>Integrations — Tangible</title></svelte:head>

<PageHeader
  title="Integrations"
  description="Download clients you already run, S3-compatible storage, and the API of the RomM the export writes into."
/>

<p class="lede">
  Tangible hands a client a torrent or an NZB and imports what it finishes; operators may import
  objects from the storage; RomM is asked only to find and scan what the export wrote. Tangible
  never searches for anything, never asks a client to run a script, and never changes anything in
  RomM. It removes what it imported from a client or bucket only where you choose that below, and
  only once the library's copy has been checked.
</p>

<div>
  <div aria-live="polite" aria-busy={view.kind === 'loading'}>
    {#if view.kind === 'loading'}
      <p class="quiet">Loading integrations…</p>
    {:else if view.kind === 'forbidden'}
      <EmptyState title="Administrators only" icon="lock">
        <p>Only an administrator can see or change integrations.</p>
      </EmptyState>
    {:else if view.kind === 'error'}
      <ErrorPanel
        problem={view.problem}
        area="import"
        endpoint="GET /api/v1/integrations"
        heading="Integrations could not be loaded"
      />
    {:else}
      {#if !view.keyLoaded}
        <Callout tone="warn" role="note">
          <StatusMark tone="warn" label="No secret key." />
          <p>
            This server cannot store a password or API key until an administrator sets
            <code>TANGIBLE_SECRET_KEY_FILE</code> and restarts it (deploy/README.md, "Integration secrets").
            Integrations can still be added without one.
          </p>
        </Callout>
      {/if}
      <Panel title="Configured" aside={`${view.items.length}`} flush>
        {#if view.items.length === 0}
          <EmptyState inset level={3} title="No integrations yet" icon="integration">
            <p>Add one with the form below.</p>
          </EmptyState>
        {:else}
          <Table caption="Configured integrations">
            <thead>
              <tr>
                <th scope="col">Name</th>
                <th scope="col">Kind</th>
                <th scope="col">Status</th>
                <th scope="col">Secret</th>
                <th scope="col">Last worked</th>
                <th scope="col"><span class="visually-hidden">Actions</span></th>
              </tr>
            </thead>
            <tbody>
              {#each view.items as item (item.id)}
                <tr>
                  <th scope="row">{item.name}</th>
                  <td>{kindLabel(item.kind)}</td>
                  <td>
                    <div class="cell">
                      <StatusMark
                        tone={integrationStatusTone(item)}
                        label={integrationStatusLabel(item)}
                      />
                      {#if item.last_error_code}<code class="small">{item.last_error_code}</code
                        >{/if}
                      {#if item.client_version}
                        <span class="small quiet"
                          >{versionLabel(item.kind)} {item.client_version}</span
                        >
                      {/if}
                    </div>
                  </td>
                  <td>{secretsLabel(item)}</td>
                  <td>
                    {#if item.last_success_at}
                      <time datetime={item.last_success_at} title={full(item.last_success_at)}
                        >{when(item.last_success_at)}</time
                      >
                    {:else}
                      <span class="quiet">Never</span>
                    {/if}
                  </td>
                  <td>
                    {#if item.managed}
                      <span class="small quiet">Kept by <code>TANGIBLE_ROMM_EXPORT_ROOT</code></span
                      >
                    {:else}
                      <div class="row-actions">
                        {#if isClientKind(item.kind)}
                          <Button
                            size="sm"
                            onclick={() => test(item)}
                            disabled={testing !== null || !item.enabled}
                            label={`Test ${item.name}`}
                            >{testing === item.id ? 'Testing…' : 'Test'}</Button
                          >
                          <Button
                            size="sm"
                            variant="quiet"
                            onclick={() => startEditing(item)}
                            label={`Edit ${item.name}`}>Edit</Button
                          >
                        {/if}
                        <Button
                          size="sm"
                          variant="danger"
                          onclick={() => startRemoving(item)}
                          label={`Remove ${item.name}`}>Remove</Button
                        >
                      </div>
                    {/if}
                  </td>
                </tr>
              {/each}
            </tbody>
          </Table>
        {/if}

        {#if removing}
          {@const item = removing}
          {#key item.id}
            <Confirm title={`Remove ${item.name}?`}>
              <p>
                Its settings and secret are deleted. Nothing in the client or the library is
                touched.
              </p>
              {#if removeProblem}
                <ErrorPanel problem={removeProblem} area="import" refusal />
              {/if}

              {#snippet actions()}
                <Button variant="danger" onclick={() => remove(item)}
                  >Yes, remove {item.name}</Button
                >
                <Button onclick={() => (confirmingRemove = null)}>Keep it</Button>
              {/snippet}
            </Confirm>
          {/key}
        {/if}
      </Panel>
    {/if}
  </div>

  <div aria-live="polite" class="results">
    {#if done}<p class="done"><StatusMark tone="ok" label={done} /></p>{/if}
    {#if tested}
      <Callout tone={tested.ok ? 'info' : 'bad'}>
        <p class="outcome">
          <StatusMark
            tone={tested.ok ? 'ok' : 'bad'}
            label={tested.ok ? 'Working.' : 'Not working.'}
          />
          <span>{tested.text}</span>
        </p>
        {#if tested.checks.length > 0}
          <ul class="checks" aria-label="What was checked">
            {#each tested.checks as check (check.name)}
              <li>
                <StatusMark
                  tone={check.ok ? 'ok' : 'bad'}
                  label={`${check.name}: ${check.ok ? 'yes' : 'no'}.`}
                />
                <span>{check.detail}</span>
              </li>
            {/each}
          </ul>
        {/if}
      </Callout>
    {/if}
  </div>

  {#if view.kind === 'ready'}
    <section class="panel-like" aria-labelledby="form-heading">
      <header>
        <h2 id="form-heading" tabindex="-1" bind:this={formHeading}>
          {editing ? `Edit ${editing.name}` : 'Add an integration'}
        </h2>
      </header>
      <div class="body">
        {#if wording.downloads && roots !== null && roots.length === 0}
          <Callout tone="warn" role="note"
            ><p>
              No watched roots are configured. Tangible reads a client's finished downloads from a
              folder mounted into its container and listed in <code>TANGIBLE_WATCH_ROOTS</code>; add
              one first.
            </p></Callout
          >
        {/if}
        <form class="client" onsubmit={submit}>
          {#if !editing}
            <Field label="Kind" hint={wording.purpose}>
              {#snippet children({ id, describedBy })}
                <select {id} name="kind" bind:value={form.kind} aria-describedby={describedBy}>
                  {#each CLIENT_KINDS as kind (kind)}
                    <option value={kind}>{kindLabel(kind)}</option>
                  {/each}
                </select>
              {/snippet}
            </Field>
          {/if}
          <Field label="Name">
            {#snippet children({ id })}
              <input {id} name="name" autocomplete="off" bind:value={form.name} required />
            {/snippet}
          </Field>
          <Field
            label={wording.addressLabel}
            hint="A LAN address is fine. Not this machine's loopback address, and no user name, password or query in it."
          >
            {#snippet children({ id, describedBy })}
              <input
                {id}
                name="base_url"
                type="url"
                inputmode="url"
                autocomplete="off"
                spellcheck="false"
                placeholder={wording.addressExample}
                bind:value={form.baseUrl}
                required
                aria-describedby={describedBy}
              />
            {/snippet}
          </Field>
          {#if wording.usernameLabel}
            <Field label={wording.usernameLabel}>
              {#snippet children({ id })}
                <input
                  {id}
                  name="username"
                  autocomplete="off"
                  autocapitalize="none"
                  spellcheck="false"
                  bind:value={form.username}
                  required
                />
              {/snippet}
            </Field>
          {/if}
          {#if form.kind === 's3'}
            <Field label="Access key id">
              {#snippet children({ id })}
                <input
                  {id}
                  name="access_key_id"
                  type="password"
                  autocomplete="off"
                  spellcheck="false"
                  bind:value={form.accessKey}
                  disabled={form.clearSecret}
                />
              {/snippet}
            </Field>
          {/if}
          <Field
            label={wording.secretLabel}
            hint={editing
              ? 'Leave empty to keep what is stored. It is never shown again, here or anywhere.'
              : 'Stored sealed and never shown again, here or anywhere.'}
          >
            {#snippet children({ id, describedBy })}
              <input
                {id}
                name="secret"
                type="password"
                autocomplete="new-password"
                bind:value={form.secret}
                disabled={form.clearSecret}
                aria-describedby={describedBy}
              />
            {/snippet}
          </Field>
          {#if editing && editing.secrets.some((secret) => secret.field === wording.secretField && secret.set)}
            <Checkbox label={`Remove the stored ${secretNoun}`} bind:checked={form.clearSecret} />
          {/if}
          {#if form.kind === 's3'}
            <Field label="Region" hint="The region signatures are made for; us-east-1 if unsure.">
              {#snippet children({ id, describedBy })}
                <input
                  {id}
                  name="region"
                  autocomplete="off"
                  spellcheck="false"
                  bind:value={form.region}
                  aria-describedby={describedBy}
                />
              {/snippet}
            </Field>
            <Checkbox
              label="Bucket in the path (host/bucket/key)"
              hint="On for most self-hosted servers; off for AWS's newer buckets, which are reached as bucket.host. An endpoint given as an address needs it on."
              bind:checked={form.pathStyle}
            />
            <Field
              label="Buckets operators may import from"
              hint="One a line. Tangible never lists or writes them, and deletes only as chosen below."
            >
              {#snippet children({ id, describedBy })}
                <textarea
                  {id}
                  name="buckets"
                  rows="3"
                  spellcheck="false"
                  bind:value={form.buckets}
                  required
                  aria-describedby={describedBy}
                ></textarea>
              {/snippet}
            </Field>
          {:else if form.kind === 'romm_api'}
            <Field
              label="RomM address, as your browser reaches it"
              hint="For links to games' pages in RomM. Leave empty when it is the address above. The server never fetches it."
            >
              {#snippet children({ id, describedBy })}
                <input
                  {id}
                  name="web_url"
                  type="url"
                  inputmode="url"
                  autocomplete="off"
                  spellcheck="false"
                  placeholder="https://romm.example.net/"
                  bind:value={form.webUrl}
                  aria-describedby={describedBy}
                />
              {/snippet}
            </Field>
            <Checkbox
              label="Ask RomM to scan what the export changes"
              hint="A quick scan of each platform folder the export wrote, renamed or took out (RomM 5.4 or later). Off, scanning is left to RomM's own schedule."
              bind:checked={form.scanAfterExport}
            />
            <Callout role="note"
              ><p>
                Make the client token in RomM (Settings, then Client API Tokens) with exactly the
                scopes
                <code>platforms.read</code>, <code>roms.read</code> and <code>tasks.run</code>, then
                Test it here.
              </p></Callout
            >
          {:else}
            <Field
              label="Category"
              hint={`Tangible adds its jobs under this category and follows only them.${
                form.kind !== 'qbittorrent'
                  ? ` It must exist in ${kindLabel(form.kind)} first, with its own folder.`
                  : ''
              }`}
            >
              {#snippet children({ id, describedBy })}
                <input
                  {id}
                  name="category"
                  autocomplete="off"
                  bind:value={form.category}
                  aria-describedby={describedBy}
                />
              {/snippet}
            </Field>
            <Field
              label={`Download folder, as ${kindLabel(form.kind)} sees it`}
              hint={`Where the category's finished downloads go, as an absolute path inside ${kindLabel(form.kind)}'s own container.`}
            >
              {#snippet children({ id, describedBy })}
                <input
                  {id}
                  name="client_path"
                  autocomplete="off"
                  spellcheck="false"
                  placeholder={wording.folderExample}
                  bind:value={form.clientPath}
                  required
                  aria-describedby={describedBy}
                />
              {/snippet}
            </Field>
            <Field
              label="The same folder, as mounted here"
              hint="The watched root where Tangible's container sees that folder, read-only unless it is listed in TANGIBLE_REMOVABLE_ROOTS."
            >
              {#snippet children({ id, describedBy })}
                <select
                  {id}
                  name="local_root"
                  bind:value={form.localRoot}
                  required
                  aria-describedby={describedBy}
                >
                  <option value="" disabled>Choose a watched root</option>
                  {#each roots ?? [] as root (root)}
                    <option value={root}>{root}</option>
                  {/each}
                </select>
              {/snippet}
            </Field>
          {/if}
          {#if removesAfterImport(form.kind)}
            <fieldset aria-describedby="after-import-hint">
              <legend>After an import is in the library</legend>
              <label class="choice">
                <input
                  type="radio"
                  name="after_import"
                  value="keep"
                  bind:group={form.afterImport}
                />
                <span>{after.keep}</span>
              </label>
              <label class="choice">
                <input
                  type="radio"
                  name="after_import"
                  value="remove"
                  bind:group={form.afterImport}
                  disabled={blocked !== null}
                />
                <span>{after.remove}</span>
              </label>
              <p class="hint" id="after-import-hint">
                Removing happens only after an integrity check reads the library's copy back and
                finds it whole, and only for imports made while this is chosen. Anything moved or
                changed since it was imported is left, and the import says why. {after.needs}
              </p>
              {#if blocked !== null}
                <p role="note" class="hint blocked">{blocked}</p>
              {/if}
            </fieldset>
          {/if}
          <Checkbox label="In use" bind:checked={form.enabled} />

          <div role="alert">
            {#if refusal}
              <p class="problem"><strong>Not saved.</strong> {refusal}</p>
            {/if}
          </div>

          <div class="buttons">
            <Button type="submit" variant="primary" disabled={submitting}>
              {#if submitting}Saving…{:else if editing}Save changes{:else}Add {kindLabel(
                  form.kind,
                )}{/if}
            </Button>
            {#if editing}
              <Button onclick={startAdding} disabled={submitting}>Cancel editing</Button>
            {/if}
          </div>
        </form>
      </div>
    </section>
  {/if}
</div>

<style>
  .lede {
    margin: 0 0 var(--space-5);
    max-width: 85ch;
    color: var(--text-muted);
  }
  /* The page is spaced by margins, not a grid's gap, so the results region
     takes no room while it is empty. */
  .small {
    font-size: var(--text-xs);
  }
  .cell {
    display: grid;
    gap: 0.125rem;
    justify-items: start;
  }
  .row-actions {
    display: flex;
    gap: var(--space-2);
    justify-content: flex-end;
    white-space: nowrap;
  }
  .results :global(.callout) {
    margin: var(--space-4) 0 0;
  }
  .done {
    margin: var(--space-4) 0 0;
  }
  .outcome {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    align-items: baseline;
    margin: 0;
  }
  .checks {
    display: grid;
    gap: var(--space-1);
    margin: var(--space-3) 0 0;
    padding: 0;
    list-style: none;
  }
  .checks li {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    align-items: baseline;
  }
  .buttons {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
  }
  /* The form's panel is drawn here rather than by Panel, so its heading can
     take the keyboard when an integration is opened for editing. */
  .panel-like {
    margin-top: var(--space-5);
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--radius-lg);
  }
  .panel-like header {
    padding: var(--space-3) var(--space-4);
    border-bottom: 1px solid var(--line);
  }
  .panel-like h2 {
    margin: 0;
    font-size: var(--text-md);
  }
  .panel-like .body {
    padding: var(--space-4);
  }
  .client {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    gap: var(--space-4);
    max-width: 40rem;
  }
  .client input:not([type='checkbox'], [type='radio']),
  .client select,
  .client textarea {
    width: 100%;
  }
  fieldset {
    display: grid;
    gap: var(--space-2);
    margin: 0;
    padding: var(--space-2) var(--space-3) var(--space-3);
    border: 1px solid var(--line);
    border-radius: var(--radius);
  }
  legend {
    padding: 0 var(--space-1);
    font-weight: var(--weight-strong);
  }
  .choice {
    display: flex;
    gap: var(--space-2);
    align-items: baseline;
    cursor: pointer;
  }
  .hint {
    margin: 0;
    font-size: var(--text-sm);
    color: var(--text-muted);
  }
  .blocked {
    color: var(--warn);
  }
  .problem {
    margin: 0;
    color: var(--bad);
  }
</style>
