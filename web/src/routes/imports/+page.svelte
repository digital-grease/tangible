<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import Panel from '$lib/components/ui/Panel.svelte';
  import Progress from '$lib/components/ui/Progress.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import Table from '$lib/components/ui/Table.svelte';
  import { onDestroy, onMount } from 'svelte';
  import { watch } from '$lib/events';
  import {
    canCancel,
    clientOfSource,
    canRetry,
    cancelImport,
    importFromS3,
    importFromUrl,
    importFromWatchedRoot,
    importStateLabel,
    sourceRemovalLabel,
    importStateTone,
    loadImportSources,
    loadImports,
    retryImport,
    s3ImportRequest,
    sendTorrent,
    sourceLabel,
    jobOutcome,
    nzbRequest,
    torrentRequest,
    MAX_NZB_BYTES,
    MAX_TORRENT_BYTES,
    transferFraction,
    PARTS_ABOVE_BYTES,
    uploadImport,
    uploadInParts,
    urlImportRequest,
    type ImportPage,
    type ImportSources,
  } from '$lib/imports';
  import { formatBytes, type LoadState, type Problem } from '$lib/library';
  import { reportSettingsOnce } from '$lib/report';

  let view = $state<LoadState<ImportPage>>({ kind: 'loading' });
  let sources = $state<ImportSources | null>(null);
  let problem = $state<Problem | null>(null);
  let busy = $state(false);
  let reportsOn = $state(false);

  let files = $state<FileList | null>(null);
  let uploaded = $state<{ sent: number; total: number } | null>(null);
  let pathId = $state('');
  let relativePath = $state('');
  let url = $state('');
  let sha256 = $state('');
  let clientId = $state('');
  let magnet = $state('');
  let torrentFiles = $state<FileList | null>(null);
  let nzbFiles = $state<FileList | null>(null);
  let s3SourceId = $state('');
  let s3Bucket = $state('');
  let s3Key = $state('');
  let s3Sha256 = $state('');
  let handedOver = $state<string | null>(null);
  let torrentRefusal = $state<string | null>(null);

  /** The S3 integrations objects can come from, and the one chosen. */
  const s3Sources = $derived(sources?.s3_sources ?? []);
  const chosenS3 = $derived(s3Sources.find((source) => source.id === s3SourceId));

  /** The download clients a job can go to. */
  const clients = $derived(sources?.download_clients ?? []);
  const chosenClient = $derived(clients.find((client) => client.id === clientId));
  /** SABnzbd and NZBGet take an NZB; qBittorrent a torrent or magnet link. */
  const takesNzb = $derived(chosenClient !== undefined && chosenClient.kind !== 'qbittorrent');
  const nothingChosen = $derived(
    takesNzb ? !nzbFiles?.length : !torrentFiles?.length && magnet.trim() === '',
  );

  let stopWatching: (() => void) | undefined;

  /**
   * How often the list polls where the browser has no event stream. With
   * one, it refreshes as imports change and reconciles every minute.
   */
  const FALLBACK_MS = 2_000;

  async function refresh() {
    const next = await loadImports();
    // A failed poll leaves what is on screen alone. Replacing a readable list
    // with an error because one refresh missed would be worse than stale.
    if (next.kind === 'error' && view.kind !== 'loading') return;
    view = next;
  }

  onMount(async () => {
    void reportSettingsOnce().then((settings) => {
      reportsOn = settings?.enabled === true && !!settings.issues_url;
    });
    const found = await loadImportSources();
    if (found.kind === 'ready') {
      sources = found.data;
      pathId = found.data.watch_roots[0] ?? '';
      clientId = found.data.download_clients[0]?.id ?? '';
      s3SourceId = found.data.s3_sources[0]?.id ?? '';
      s3Bucket = found.data.s3_sources[0]?.buckets[0] ?? '';
    }
    await refresh();
    stopWatching = watch(['imports'], refresh, { fallbackMs: FALLBACK_MS });
  });

  onDestroy(() => stopWatching?.());

  async function upload(event: SubmitEvent) {
    event.preventDefault();
    const file = files?.[0];
    if (!file || busy) return;
    busy = true;
    problem = null;

    // A large file goes in parts, so a dropped connection costs one part and
    // choosing the same file again carries on.
    const result =
      file.size > PARTS_ABOVE_BYTES
        ? await uploadInParts(file, (sent, total) => {
            uploaded = { sent, total };
          })
        : await uploadImport(file);
    if (result.kind === 'error') {
      problem = result.problem;
    } else {
      files = null;
      uploaded = null;
      await refresh();
    }
    busy = false;
  }

  async function importWatched(event: SubmitEvent) {
    event.preventDefault();
    if (busy) return;
    busy = true;
    problem = null;

    const result = await importFromWatchedRoot({
      path_id: pathId,
      relative_path: relativePath.trim(),
    });
    if (result.kind === 'error') {
      problem = result.problem;
    } else {
      relativePath = '';
      await refresh();
    }
    busy = false;
  }

  async function importUrl(event: SubmitEvent) {
    event.preventDefault();
    if (busy) return;
    busy = true;
    problem = null;

    const result = await importFromUrl(urlImportRequest(url, sha256));
    if (result.kind === 'error') {
      problem = result.problem;
    } else {
      url = '';
      sha256 = '';
      await refresh();
    }
    busy = false;
  }

  /** A different source offers different buckets: start from its first. */
  function chooseS3Source(id: string) {
    s3SourceId = id;
    s3Bucket = s3Sources.find((source) => source.id === id)?.buckets[0] ?? '';
  }

  async function importS3(event: SubmitEvent) {
    event.preventDefault();
    if (busy || !chosenS3) return;
    busy = true;
    problem = null;
    const result = await importFromS3(s3ImportRequest(chosenS3.id, s3Bucket, s3Key, s3Sha256));
    if (result.kind === 'error') {
      problem = result.problem;
    } else {
      s3Key = '';
      s3Sha256 = '';
      await refresh();
    }
    busy = false;
  }

  async function sendToClient(event: SubmitEvent) {
    event.preventDefault();
    if (busy || !sources) return;
    problem = null;
    handedOver = null;
    torrentRefusal = null;
    const client = chosenClient;
    if (!client) return;
    const body = takesNzb
      ? await nzbRequest(nzbFiles?.[0])
      : await torrentRequest(magnet, torrentFiles?.[0]);
    if (!body) {
      if (takesNzb) {
        torrentRefusal = nzbFiles?.[0]
          ? `An NZB is at most ${formatBytes(MAX_NZB_BYTES)}.`
          : 'Choose an .nzb file.';
      } else {
        torrentRefusal = torrentFiles?.[0]
          ? `A .torrent file is at most ${formatBytes(MAX_TORRENT_BYTES)}.`
          : 'Choose a .torrent file or paste a magnet link.';
      }
      return;
    }
    busy = true;
    const result = await sendTorrent(client.id, body);
    if (result.kind === 'error') {
      problem = result.problem;
    } else {
      handedOver = jobOutcome(result.data, client.name);
      magnet = '';
      torrentFiles = null;
      nzbFiles = null;
      await refresh();
    }
    busy = false;
  }

  async function cancel(id: string) {
    busy = true;
    problem = null;
    const result = await cancelImport(id);
    if (result.kind === 'error') problem = result.problem;
    await refresh();
    busy = false;
  }

  async function retry(id: string) {
    busy = true;
    problem = null;
    const result = await retryImport(id);
    if (result.kind === 'error') problem = result.problem;
    await refresh();
    busy = false;
  }
</script>

<svelte:head><title>Imports — Tangible</title></svelte:head>

<PageHeader
  title="Imports"
  description="Importing preserves what you give it. The bytes are stored exactly as received, hashed, and described; nothing is repaired, recompressed or renamed."
/>

{#if problem}
  <ErrorPanel {problem} area="import" refusal />
{/if}

<!-- Where an import can come from, each its own panel; those this server is
     not set up for say how to set them up rather than showing a form. -->
<div class="sources">
  <Panel title="Upload a file">
    <form class="form" onsubmit={upload}>
      <label for="file">Disc image</label>
      <input id="file" type="file" bind:files />
      {#if sources}
        <p class="hint">
          This server accepts uploads up to {formatBytes(sources.max_upload_bytes)}. Files over {formatBytes(
            PARTS_ABOVE_BYTES,
          )} are sent in parts: if the connection drops or the page is closed, choose the same file again
          and the upload carries on from where it stopped.
        </p>
      {/if}
      {#if uploaded}
        <div class="sent" aria-live="polite">
          <Progress value={uploaded.sent / Math.max(1, uploaded.total)} label="Upload progress" />
          <span class="num">Sent {formatBytes(uploaded.sent)} of {formatBytes(uploaded.total)}</span
          >
        </div>
      {/if}
      <div>
        <Button type="submit" variant="primary" icon="import" disabled={busy || !files?.length}>
          {busy ? 'Working…' : 'Upload and import'}
        </Button>
      </div>
    </form>
  </Panel>

  <Panel title="Import from a watched folder">
    {#if !sources || sources.watch_roots.length === 0}
      <!-- Configuration, not a failure: an operator who has not set any roots
           should be told how, not shown a broken form. -->
      <p class="quiet">
        No watched folders are configured. An administrator can add them with
        <code>TANGIBLE_WATCH_ROOTS</code>, as <code>id=/path,id=/path</code>.
      </p>
    {:else}
      <form class="form" onsubmit={importWatched}>
        <label for="root">Folder</label>
        <select id="root" bind:value={pathId}>
          {#each sources.watch_roots as root (root)}
            <option value={root}>{root}</option>
          {/each}
        </select>
        <label for="relative">Path within the folder</label>
        <input
          id="relative"
          bind:value={relativePath}
          placeholder="Example Disc"
          aria-describedby="relative-hint"
        />
        <!-- The server never accepts a host path, and saying so here means an
             operator does not try one. -->
        <p class="hint" id="relative-hint">
          Relative to the folder above: a file, or a folder holding one disc, such as a CUE or TOC
          sheet and the files it names, which become one artifact. Everything is copied; your
          originals are left alone.
        </p>
        <div>
          <Button type="submit" disabled={busy || relativePath.trim() === ''}>
            {busy ? 'Working…' : 'Import'}
          </Button>
        </div>
      </form>
    {/if}
  </Panel>

  <Panel title="Import from a URL">
    {#if !sources?.url_imports}
      <p class="quiet">
        Importing from a URL is turned off on this server. An administrator can turn it on with
        <code>TANGIBLE_URL_IMPORTS=true</code>.
      </p>
    {:else}
      <form class="form" onsubmit={importUrl}>
        <label for="url">Address of the file</label>
        <input
          id="url"
          type="url"
          bind:value={url}
          placeholder="https://example.org/disc.iso"
          aria-describedby="url-hint"
          required
        />
        <p class="hint" id="url-hint">
          The server fetches it {sources.url_imports_allow_http
            ? 'over https or http'
            : 'over https'}, from public addresses unless an administrator allowed more, and picks
          up where it left off if the connection drops. It is recorded without anything after a
          <code>?</code>, which is where signed links keep their secret.
        </p>
        <label for="sha256">SHA-256, if you know it</label>
        <input
          id="sha256"
          bind:value={sha256}
          spellcheck="false"
          autocomplete="off"
          aria-describedby="sha256-hint"
        />
        <p class="hint" id="sha256-hint">
          Checked after the download; a file that does not match is not imported.
        </p>
        <div>
          <Button type="submit" disabled={busy || url.trim() === ''}>
            {busy ? 'Working…' : 'Fetch and import'}
          </Button>
        </div>
      </form>
    {/if}
  </Panel>

  <Panel title="Import from S3">
    {#if s3Sources.length === 0}
      <p class="quiet">
        No S3-compatible storage is set up. An administrator can add it on the
        <a href="/integrations">Integrations</a> page, with the buckets you may import from.
      </p>
    {:else}
      <form class="form" onsubmit={importS3}>
        {#if s3Sources.length > 1}
          <label for="s3-source">Storage</label>
          <select
            id="s3-source"
            value={s3SourceId}
            onchange={(event) => chooseS3Source(event.currentTarget.value)}
          >
            {#each s3Sources as source (source.id)}
              <option value={source.id}>{source.name}</option>
            {/each}
          </select>
        {/if}
        <label for="s3-bucket">Bucket</label>
        <select id="s3-bucket" bind:value={s3Bucket}>
          {#each chosenS3?.buckets ?? [] as bucket (bucket)}
            <option value={bucket}>{bucket}</option>
          {/each}
        </select>
        <label for="s3-key">Object key</label>
        <input
          id="s3-key"
          bind:value={s3Key}
          placeholder="discs/Example Disc.iso"
          spellcheck="false"
          autocomplete="off"
          aria-describedby="s3-key-hint"
          required
        />
        <p class="hint" id="s3-key-hint">
          The object's full name in the bucket. The server looks at it first, and the import is
          pinned to the version and size it has now.
        </p>
        <label for="s3-sha256">SHA-256, if you know it</label>
        <input id="s3-sha256" bind:value={s3Sha256} spellcheck="false" autocomplete="off" />
        <div>
          <Button type="submit" disabled={busy || s3Key.trim() === '' || s3Bucket === ''}>
            {busy ? 'Working…' : 'Import from S3'}
          </Button>
        </div>
      </form>
    {/if}
  </Panel>

  <Panel title="Send to a download client">
    {#if clients.length === 0}
      <p class="quiet">
        No download client is set up. An administrator can add qBittorrent, SABnzbd or NZBGet on the
        <a href="/integrations">Integrations</a> page.
      </p>
    {:else}
      <form class="form" onsubmit={sendToClient}>
        <p class="hint">
          For a torrent or an NZB you already have. It is added to the client's Tangible category
          and imported here when it finishes; seeding, cleaning up and anything else stay the
          client's business. Tangible never searches for anything.
        </p>
        {#if clients.length > 1}
          <label for="client">Client</label>
          <select id="client" bind:value={clientId}>
            {#each clients as client (client.id)}
              <option value={client.id}>{client.name}</option>
            {/each}
          </select>
        {/if}
        {#if takesNzb}
          <label for="nzb-file">.nzb file</label>
          <input id="nzb-file" type="file" accept=".nzb,application/x-nzb" bind:files={nzbFiles} />
        {:else}
          <label for="torrent-file">.torrent file</label>
          <input
            id="torrent-file"
            type="file"
            accept=".torrent,application/x-bittorrent"
            bind:files={torrentFiles}
          />
          <label for="magnet">or a magnet link</label>
          <input
            id="magnet"
            bind:value={magnet}
            placeholder="magnet:?xt=urn:btih:…"
            spellcheck="false"
            autocomplete="off"
            disabled={!!torrentFiles?.length}
          />
        {/if}
        <div role="alert">
          {#if torrentRefusal}<p class="refused">{torrentRefusal}</p>{/if}
        </div>
        <div aria-live="polite">
          {#if handedOver}<p>{handedOver}</p>{/if}
        </div>
        <div>
          <Button type="submit" disabled={busy || nothingChosen}>
            {busy ? 'Working…' : `Send to ${chosenClient?.name ?? 'the client'}`}
          </Button>
        </div>
      </form>
    {/if}
  </Panel>
</div>

<div class="queue">
  <Panel title="Recent imports" flush>
    <div aria-live="polite" aria-busy={view.kind === 'loading'}>
      {#if view.kind === 'loading'}
        <p class="quiet padded">Loading imports…</p>
      {:else if view.kind === 'empty'}
        <p class="quiet padded">Nothing has been imported yet.</p>
      {:else if view.kind === 'error'}
        <div class="padded">
          <ErrorPanel problem={view.problem} area="import" endpoint="GET /api/v1/imports" />
        </div>
      {:else}
        <Table caption="Imports, newest first">
          <thead>
            <tr>
              <th scope="col">File</th>
              <th scope="col">Source</th>
              <th scope="col">State</th>
              <th scope="col" class="num">Size</th>
              <th scope="col">Result</th>
              <th scope="col"><span class="visually-hidden">Actions</span></th>
            </tr>
          </thead>
          <tbody>
            {#each view.data.items as job (job.id)}
              {@const fraction = transferFraction(job)}
              <tr>
                <th scope="row">{job.source_filename ?? job.id.slice(0, 8)}</th>
                <td>
                  <span>{sourceLabel(job.source_kind)}</span>
                  {#if (job.source_kind === 'http_url' || job.source_kind === 's3_object' || clientOfSource(job.source_kind)) && job.source_reference}
                    <small class="reference">{job.source_reference}</small>
                  {/if}
                </td>
                <td>
                  <div class="stack">
                    <StatusMark
                      tone={importStateTone(job.state)}
                      label={importStateLabel(job.state, job.source_kind, job.error_code)}
                      pill
                    />
                    {#if fraction !== null}
                      <Progress
                        value={fraction}
                        label="Transfer of {job.source_filename ?? 'the file'}"
                      />
                    {/if}
                    {#if job.error_code}
                      <small><code>{job.error_code}</code></small>
                    {/if}
                  </div>
                </td>
                <td class="num">{formatBytes(job.bytes_received || (job.bytes_expected ?? 0))}</td>
                <td>
                  <div class="stack">
                    {#if job.artifact_id}
                      <a href={`/library/${job.artifact_id}`}>Open in library</a>
                    {:else if job.error_detail}
                      <span>{job.error_detail}</span>
                    {:else}
                      <span class="quiet">·</span>
                    {/if}
                    {#if sourceRemovalLabel(job)}
                      {@const removal = sourceRemovalLabel(job)}
                      <p class="removal">
                        <small
                          ><strong>{removal?.headline}.</strong>
                          {#if removal?.detail}{removal.detail}.{/if}</small
                        >
                      </p>
                    {/if}
                    {#if job.warnings.length > 0}
                      <details>
                        <summary
                          >{job.warnings.length} finding{job.warnings.length === 1
                            ? ''
                            : 's'}</summary
                        >
                        <ul>
                          {#each job.warnings as warning, index (index)}
                            <li>{warning}</li>
                          {/each}
                        </ul>
                        {#if job.artifact_id && reportsOn}
                          <!-- The artifact's page offers a format report when
                             reports are on; the row does not know the format. -->
                          <a href={`/library/${job.artifact_id}#validation`}
                            >Ask about these findings</a
                          >
                        {/if}
                      </details>
                    {/if}
                  </div>
                </td>
                <td class="actions">
                  {#if canCancel(job)}
                    <Button size="sm" onclick={() => cancel(job.id)} disabled={busy}>Cancel</Button>
                  {/if}
                  {#if canRetry(job)}
                    <Button size="sm" onclick={() => retry(job.id)} disabled={busy}>
                      {job.resumes_from ? `Retry from ${job.resumes_from}` : 'Retry'}
                    </Button>
                  {/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </Table>
      {/if}
    </div>
  </Panel>
</div>

<style>
  .sources {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(22rem, 1fr));
    gap: var(--space-4);
    align-items: start;
  }
  .form {
    display: grid;
    gap: var(--space-1);
  }
  .form label {
    margin-top: var(--space-2);
    font-size: var(--text-sm);
  }
  .form label:first-child {
    margin-top: 0;
  }
  .form > div:last-child {
    margin-top: var(--space-3);
  }
  .hint {
    margin: 0;
    font-size: var(--text-sm);
    color: var(--text-muted);
    max-width: 60ch;
  }
  .sent {
    display: grid;
    gap: var(--space-1);
    margin-top: var(--space-2);
    font-size: var(--text-sm);
  }
  .refused {
    color: var(--bad);
    font-weight: var(--weight-medium);
  }
  .queue {
    margin-top: var(--space-5);
  }
  .stack {
    display: grid;
    gap: var(--space-1);
    justify-items: start;
  }
  .stack :global(.progress) {
    width: 8rem;
  }
  .reference {
    display: block;
    color: var(--text-muted);
    overflow-wrap: anywhere;
  }
  .removal {
    margin: 0;
  }
  .actions {
    white-space: nowrap;
  }
  .padded {
    padding: var(--space-3) var(--space-4);
  }
  .quiet {
    color: var(--text-muted);
  }
</style>
