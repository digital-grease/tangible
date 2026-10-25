<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Integrations, for administrators (ADR 0016). A download client is
  // configured here; a password is typed, sent, and never shown again. The
  // RomM export is listed, and kept by its own setting.
  import { onMount } from 'svelte';
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import { loadImportSources } from '$lib/imports';
  import {
    createIntegration,
    createQbittorrentRequest,
    deleteIntegration,
    emptyQbittorrentForm,
    integrationRefusal,
    integrationStatusLabel,
    kindLabel,
    loadIntegrations,
    qbittorrentFormFrom,
    secretsLabel,
    testIntegration,
    testOutcome,
    updateIntegration,
    updateQbittorrentRequest,
    type IntegrationView,
    type QbittorrentForm,
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

  /** The integration being edited, or `null` while adding one. */
  let editing = $state<IntegrationView | null>(null);
  let form = $state<QbittorrentForm>(emptyQbittorrentForm([]));
  let submitting = $state(false);
  let refusal = $state<string | null>(null);
  let done = $state<string | null>(null);

  /** The integration being tried, and what trying said. */
  let testing = $state<string | null>(null);
  let tested = $state<{ id: string; text: string; ok: boolean } | null>(null);

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
    if (!editing) form = emptyQbittorrentForm(roots ?? []);
  });

  function startEditing(item: IntegrationView) {
    editing = item;
    form = qbittorrentFormFrom(item);
    refusal = null;
    done = null;
  }

  function startAdding() {
    editing = null;
    form = emptyQbittorrentForm(roots ?? []);
    refusal = null;
  }

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (submitting) return;
    submitting = true;
    refusal = null;
    done = null;
    const result = editing
      ? await updateIntegration(editing.id, updateQbittorrentRequest(form))
      : await createIntegration(createQbittorrentRequest(form));
    submitting = false;
    // Whatever happened, the password typed is not kept in the page.
    form.password = '';
    form.clearPassword = false;
    if (result.kind === 'error') {
      refusal = integrationRefusal(result.problem);
      return;
    }
    done = editing ? `Saved ${result.data.name}.` : `Added ${result.data.name}.`;
    editing = null;
    form = emptyQbittorrentForm(roots ?? []);
    await refresh();
  }

  async function test(item: IntegrationView) {
    testing = item.id;
    tested = null;
    const result = await testIntegration(item.id);
    testing = null;
    tested =
      result.kind === 'ok'
        ? { id: item.id, text: testOutcome(item.name, result.data), ok: result.data.ok }
        : {
            id: item.id,
            text: `${item.name} could not be tried: ${result.problem.detail}`,
            ok: false,
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
  Download clients you already run, which Tangible will hand a torrent to and import from when it
  finishes. Tangible never searches for anything, and never runs a client's scripts.
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
          <strong>No secret key.</strong> This server cannot store a password until an administrator
          sets <code>TANGIBLE_SECRET_KEY_FILE</code> and restarts it (deploy/README.md, "Integration secrets").
          Integrations can still be added without one.
        </p>
      </div>
    {/if}
    {#if view.items.length === 0}
      <p>No integrations yet. Add a qBittorrent below.</p>
    {:else}
      <table>
        <caption>Configured integrations</caption>
        <thead>
          <tr>
            <th scope="col">Name</th>
            <th scope="col">Kind</th>
            <th scope="col">Status</th>
            <th scope="col">Password</th>
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
                {#if item.client_version}<br /><small>WebUI API {item.client_version}</small>{/if}
              </td>
              <td>{secretsLabel(item)}</td>
              <td>{when(item.last_success_at)}</td>
              <td>
                {#if item.managed}
                  <small>Kept by <code>TANGIBLE_ROMM_EXPORT_ROOT</code></small>
                {:else if confirmingRemove === item.id}
                  <p>
                    <strong>Remove {item.name}?</strong> Its settings and password are deleted. Nothing
                    in the client or the library is touched.
                  </p>
                  <button type="button" onclick={() => remove(item)}>Yes, remove {item.name}</button
                  >
                  <button type="button" onclick={() => (confirmingRemove = null)}>Keep it</button>
                  {#if removeProblem}
                    <ErrorPanel problem={removeProblem} area="import" refusal />
                  {/if}
                {:else}
                  {#if item.kind === 'qbittorrent'}
                    <button
                      type="button"
                      onclick={() => test(item)}
                      disabled={testing !== null || !item.enabled}
                      aria-label={`Test ${item.name}`}
                      >{testing === item.id ? 'Testing…' : 'Test'}</button
                    >
                    <!-- The form edits a qBittorrent; others are changed through
                         the API until the page has a form for them. -->
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
  {/if}
</div>

{#if view.kind === 'ready'}
  <section aria-labelledby="form-heading">
    <h2 id="form-heading">{editing ? `Edit ${editing.name}` : 'Add a qBittorrent'}</h2>
    {#if roots !== null && roots.length === 0}
      <p role="note">
        No watched roots are configured. Tangible reads a client's finished downloads from a folder
        mounted into its container and listed in <code>TANGIBLE_WATCH_ROOTS</code>; add one first.
      </p>
    {/if}
    <form onsubmit={submit}>
      <p>
        <label for="name">Name</label><br />
        <input id="name" name="name" autocomplete="off" bind:value={form.name} required />
      </p>
      <p>
        <label for="base-url">WebUI address</label><br />
        <input
          id="base-url"
          name="base_url"
          type="url"
          inputmode="url"
          autocomplete="off"
          spellcheck="false"
          placeholder="http://192.168.1.13:8080/"
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
      <p>
        <label for="username">WebUI user</label><br />
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
      <p>
        <label for="password">WebUI password</label><br />
        <input
          id="password"
          name="password"
          type="password"
          autocomplete="new-password"
          bind:value={form.password}
          disabled={form.clearPassword}
          aria-describedby="password-hint"
        />
        <br />
        <small id="password-hint">
          {#if editing}
            Leave empty to keep the stored password. It is never shown again, here or anywhere.
          {:else}
            Stored sealed and never shown again, here or anywhere.
          {/if}
        </small>
      </p>
      {#if editing && editing.secrets.some((secret) => secret.field === 'password' && secret.set)}
        <p>
          <label>
            <input type="checkbox" name="clear_password" bind:checked={form.clearPassword} />
            Remove the stored password
          </label>
        </p>
      {/if}
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
        <small id="category-hint"
          >Tangible adds torrents under this category and watches only it.</small
        >
      </p>
      <p>
        <label for="client-path">Download folder, as qBittorrent sees it</label><br />
        <input
          id="client-path"
          name="client_path"
          autocomplete="off"
          spellcheck="false"
          placeholder="/downloads/tangible"
          bind:value={form.clientPath}
          required
          aria-describedby="client-path-hint"
        />
        <br />
        <small id="client-path-hint">
          The category's save path, as an absolute path inside qBittorrent's own container.
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
          The watched root where Tangible's container sees that folder, read-only.
        </small>
      </p>
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
        {#if submitting}Saving…{:else if editing}Save changes{:else}Add qBittorrent{/if}
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
