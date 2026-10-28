<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Integrations, for administrators (ADR 0016). A download client
  // (qBittorrent, SABnzbd, NZBGet), S3-compatible storage or RomM's API is
  // configured here; a password, API key or token is typed, sent, and never
  // shown again. The RomM export is listed, and kept by its own setting.
  import { onMount } from 'svelte';
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
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

  function when(value: string | null | undefined): string {
    return value ? new Date(value).toLocaleString() : 'Never';
  }
</script>

<svelte:head><title>Integrations — Tangible</title></svelte:head>

<h1>Integrations</h1>

<p>
  Download clients you already run, which Tangible will hand a torrent or an NZB to and import from
  when it finishes; S3-compatible storage operators may import objects from; and the API of the RomM
  the export writes into. Tangible never searches for anything, never asks a client to run a script,
  and never changes anything in RomM. It removes what it imported from a client or bucket only where
  you choose that below, and only once the library's copy has been checked.
</p>

<section aria-live="polite" aria-busy={view.kind === 'loading'}>
  {#if view.kind === 'loading'}
    <p>Loading integrations…</p>
  {:else if view.kind === 'forbidden'}
    <p>Only an administrator can see or change integrations.</p>
  {:else if view.kind === 'error'}
    <ErrorPanel
      problem={view.problem}
      area="import"
      endpoint="GET /api/v1/integrations"
      heading="Integrations could not be loaded"
    />
  {:else}
    {#if !view.keyLoaded}
      <div role="note" class="notice">
        <p>
          <strong>No secret key.</strong> This server cannot store a password or API key until an
          administrator sets <code>TANGIBLE_SECRET_KEY_FILE</code> and restarts it (deploy/README.md,
          "Integration secrets"). Integrations can still be added without one.
        </p>
      </div>
    {/if}
    {#if view.items.length === 0}
      <p>No integrations yet. Add one below.</p>
    {:else}
      <table>
        <caption>Configured integrations</caption>
        <thead>
          <tr>
            <th scope="col">Name</th>
            <th scope="col">Kind</th>
            <th scope="col">Status</th>
            <th scope="col">Secret</th>
            <th scope="col">Last worked</th>
            <th scope="col">Actions</th>
          </tr>
        </thead>
        <tbody>
          {#each view.items as item (item.id)}
            <tr>
              <th scope="row">{item.name}</th>
              <td>{kindLabel(item.kind)}</td>
              <td>
                {integrationStatusLabel(item)}
                {#if item.last_error_code}<br /><small>{item.last_error_code}</small>{/if}
                {#if item.client_version}<br /><small
                    >{versionLabel(item.kind)} {item.client_version}</small
                  >{/if}
              </td>
              <td>{secretsLabel(item)}</td>
              <td>{when(item.last_success_at)}</td>
              <td>
                {#if item.managed}
                  <small>Kept by <code>TANGIBLE_ROMM_EXPORT_ROOT</code></small>
                {:else if confirmingRemove === item.id}
                  <p>
                    <strong>Remove {item.name}?</strong> Its settings and secret are deleted. Nothing
                    in the client or the library is touched.
                  </p>
                  <button type="button" onclick={() => remove(item)}>Yes, remove {item.name}</button
                  >
                  <button type="button" onclick={() => (confirmingRemove = null)}>Keep it</button>
                  {#if removeProblem}
                    <ErrorPanel problem={removeProblem} area="import" refusal />
                  {/if}
                {:else}
                  {#if isClientKind(item.kind)}
                    <button
                      type="button"
                      onclick={() => test(item)}
                      disabled={testing !== null || !item.enabled}
                      aria-label={`Test ${item.name}`}
                      >{testing === item.id ? 'Testing…' : 'Test'}</button
                    >
                    <button
                      type="button"
                      onclick={() => startEditing(item)}
                      aria-label={`Edit ${item.name}`}>Edit</button
                    >
                  {/if}
                  <button
                    type="button"
                    onclick={() => {
                      confirmingRemove = item.id;
                      removeProblem = null;
                    }}
                    aria-label={`Remove ${item.name}`}>Remove</button
                  >
                {/if}
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}
  {/if}
</section>

<div aria-live="polite">
  {#if done}<p>{done}</p>{/if}
  {#if tested}
    <p><strong>{tested.ok ? 'Working.' : 'Not working.'}</strong> {tested.text}</p>
    {#if tested.checks.length > 0}
      <ul aria-label="What was checked">
        {#each tested.checks as check (check.name)}
          <li>
            <strong>{check.name}: {check.ok ? 'yes' : 'no'}.</strong>
            {check.detail}
          </li>
        {/each}
      </ul>
    {/if}
  {/if}
</div>

{#if view.kind === 'ready'}
  <section aria-labelledby="form-heading">
    <h2 id="form-heading">
      {editing ? `Edit ${editing.name}` : 'Add an integration'}
    </h2>
    {#if wording.downloads && roots !== null && roots.length === 0}
      <p role="note">
        No watched roots are configured. Tangible reads a client's finished downloads from a folder
        mounted into its container and listed in <code>TANGIBLE_WATCH_ROOTS</code>; add one first.
      </p>
    {/if}
    <form onsubmit={submit}>
      {#if !editing}
        <p>
          <label for="kind">Kind</label><br />
          <select id="kind" name="kind" bind:value={form.kind} aria-describedby="kind-hint">
            {#each CLIENT_KINDS as kind (kind)}
              <option value={kind}>{kindLabel(kind)}</option>
            {/each}
          </select>
          <br />
          <small id="kind-hint">{wording.purpose}</small>
        </p>
      {/if}
      <p>
        <label for="name">Name</label><br />
        <input id="name" name="name" autocomplete="off" bind:value={form.name} required />
      </p>
      <p>
        <label for="base-url">{wording.addressLabel}</label><br />
        <input
          id="base-url"
          name="base_url"
          type="url"
          inputmode="url"
          autocomplete="off"
          spellcheck="false"
          placeholder={wording.addressExample}
          bind:value={form.baseUrl}
          required
          aria-describedby="base-url-hint"
        />
        <br />
        <small id="base-url-hint">
          A LAN address is fine. Not this machine's loopback address, and no user name, password or
          query in it.
        </small>
      </p>
      {#if wording.usernameLabel}
        <p>
          <label for="username">{wording.usernameLabel}</label><br />
          <input
            id="username"
            name="username"
            autocomplete="off"
            autocapitalize="none"
            spellcheck="false"
            bind:value={form.username}
            required
          />
        </p>
      {/if}
      {#if form.kind === 's3'}
        <p>
          <label for="access-key">Access key id</label><br />
          <input
            id="access-key"
            name="access_key_id"
            type="password"
            autocomplete="off"
            spellcheck="false"
            bind:value={form.accessKey}
            disabled={form.clearSecret}
          />
        </p>
      {/if}
      <p>
        <label for="secret">{wording.secretLabel}</label><br />
        <input
          id="secret"
          name="secret"
          type="password"
          autocomplete="new-password"
          bind:value={form.secret}
          disabled={form.clearSecret}
          aria-describedby="secret-hint"
        />
        <br />
        <small id="secret-hint">
          {#if editing}
            Leave empty to keep what is stored. It is never shown again, here or anywhere.
          {:else}
            Stored sealed and never shown again, here or anywhere.
          {/if}
        </small>
      </p>
      {#if editing && editing.secrets.some((secret) => secret.field === wording.secretField && secret.set)}
        <p>
          <label>
            <input type="checkbox" name="clear_secret" bind:checked={form.clearSecret} />
            Remove the stored {secretNoun}
          </label>
        </p>
      {/if}
      {#if form.kind === 's3'}
        <p>
          <label for="region">Region</label><br />
          <input
            id="region"
            name="region"
            autocomplete="off"
            spellcheck="false"
            bind:value={form.region}
            aria-describedby="region-hint"
          />
          <br />
          <small id="region-hint">The region signatures are made for; us-east-1 if unsure.</small>
        </p>
        <p>
          <label>
            <input type="checkbox" name="path_style" bind:checked={form.pathStyle} />
            Bucket in the path (host/bucket/key)
          </label>
          <br />
          <small
            >On for most self-hosted servers; off for AWS's newer buckets, which are reached as
            bucket.host. An endpoint given as an address needs it on.</small
          >
        </p>
        <p>
          <label for="buckets">Buckets operators may import from</label><br />
          <textarea
            id="buckets"
            name="buckets"
            rows="3"
            spellcheck="false"
            bind:value={form.buckets}
            required
            aria-describedby="buckets-hint"
          ></textarea>
          <br />
          <small id="buckets-hint"
            >One a line. Tangible never lists or writes them, and deletes only as chosen below.</small
          >
        </p>
      {:else if form.kind === 'romm_api'}
        <p>
          <label for="web-url">RomM address, as your browser reaches it</label><br />
          <input
            id="web-url"
            name="web_url"
            type="url"
            inputmode="url"
            autocomplete="off"
            spellcheck="false"
            placeholder="https://romm.example.net/"
            bind:value={form.webUrl}
            aria-describedby="web-url-hint"
          />
          <br />
          <small id="web-url-hint"
            >For links to games' pages in RomM. Leave empty when it is the address above. The server
            never fetches it.</small
          >
        </p>
        <p>
          <label>
            <input type="checkbox" name="scan_after_export" bind:checked={form.scanAfterExport} />
            Ask RomM to scan what the export changes
          </label>
          <br />
          <small
            >A quick scan of each platform folder the export wrote, renamed or took out (RomM 5.4 or
            later). Off, scanning is left to RomM's own schedule.</small
          >
        </p>
        <p role="note">
          Make the client token in RomM (Settings, then Client API Tokens) with exactly the scopes
          <code>platforms.read</code>, <code>roms.read</code> and <code>tasks.run</code>, then Test
          it here.
        </p>
      {:else}
        <p>
          <label for="category">Category</label><br />
          <input
            id="category"
            name="category"
            autocomplete="off"
            bind:value={form.category}
            aria-describedby="category-hint"
          />
          <br />
          <small id="category-hint">
            Tangible adds its jobs under this category and follows only them.
            {#if form.kind !== 'qbittorrent'}
              It must exist in {kindLabel(form.kind)} first, with its own folder.
            {/if}
          </small>
        </p>
        <p>
          <label for="client-path">Download folder, as {kindLabel(form.kind)} sees it</label><br />
          <input
            id="client-path"
            name="client_path"
            autocomplete="off"
            spellcheck="false"
            placeholder={wording.folderExample}
            bind:value={form.clientPath}
            required
            aria-describedby="client-path-hint"
          />
          <br />
          <small id="client-path-hint">
            Where the category's finished downloads go, as an absolute path inside {kindLabel(
              form.kind,
            )}'s own container.
          </small>
        </p>
        <p>
          <label for="local-root">The same folder, as mounted here</label><br />
          <select
            id="local-root"
            name="local_root"
            bind:value={form.localRoot}
            required
            aria-describedby="local-root-hint"
          >
            <option value="" disabled>Choose a watched root</option>
            {#each roots ?? [] as root (root)}
              <option value={root}>{root}</option>
            {/each}
          </select>
          <br />
          <small id="local-root-hint">
            The watched root where Tangible's container sees that folder, read-only unless it is
            listed in <code>TANGIBLE_REMOVABLE_ROOTS</code>.
          </small>
        </p>
      {/if}
      {#if removesAfterImport(form.kind)}
        <fieldset aria-describedby="after-import-hint">
          <legend>After an import is in the library</legend>
          <p>
            <label>
              <input type="radio" name="after_import" value="keep" bind:group={form.afterImport} />
              {after.keep}
            </label>
            <br />
            <label>
              <input
                type="radio"
                name="after_import"
                value="remove"
                bind:group={form.afterImport}
                disabled={blocked !== null}
              />
              {after.remove}
            </label>
          </p>
          <small id="after-import-hint">
            Removing happens only after an integrity check reads the library's copy back and finds
            it whole, and only for imports made while this is chosen. Anything moved or changed
            since it was imported is left, and the import says why. {after.needs}
          </small>
          {#if blocked !== null}
            <p role="note"><small>{blocked}</small></p>
          {/if}
        </fieldset>
      {/if}
      <p>
        <label>
          <input type="checkbox" name="enabled" bind:checked={form.enabled} />
          In use
        </label>
      </p>

      <div role="alert">
        {#if refusal}
          <p><strong>Not saved.</strong> {refusal}</p>
        {/if}
      </div>

      <button type="submit" disabled={submitting}>
        {#if submitting}Saving…{:else if editing}Save changes{:else}Add {kindLabel(form.kind)}{/if}
      </button>
      {#if editing}
        <button type="button" onclick={startAdding} disabled={submitting}>Cancel editing</button>
      {/if}
    </form>
  </section>
{/if}

<style>
  .notice {
    border: 2px solid currentColor;
    padding: 0 1rem;
  }
</style>
