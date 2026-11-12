<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // One artifact (UX.md 5; docs/plans/ui-overhaul.md, "Pages"): a header
  // with its name, what it is and what can be done with it, then tabs in
  // UX.md 5's order. Its deletion, when asked for, comes first, because
  // everything below reads differently; otherwise it is the last thing on
  // the page.
  import Callout from '$lib/components/ui/Callout.svelte';
  import { shortDigest } from '$lib/ids';
  import Deletion from '$lib/components/Deletion.svelte';
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import FormatReport from '$lib/components/FormatReport.svelte';
  import Identification from '$lib/components/Identification.svelte';
  import Integrity from '$lib/components/Integrity.svelte';
  import Lineage from '$lib/components/Lineage.svelte';
  import Badge from '$lib/components/ui/Badge.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import Disc from '$lib/components/ui/Disc.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import Table from '$lib/components/ui/Table.svelte';
  import Tabs from '$lib/components/ui/Tabs.svelte';
  import { onMount } from 'svelte';
  import { page } from '$app/state';
  import { familyLabel } from '$lib/disc';
  import {
    componentUrl,
    discSummary,
    formatBytes,
    archiveLabel,
    formatLabel,
    integrityShort,
    integrityTone,
    isOriginal,
    loadArtifact,
    msf,
    notBurnedAsItIs,
    originLabel,
    trackDetails,
    trackModeLabel,
    validationLabel,
    validationTone,
    type ArtifactDetail,
    type LoadState,
  } from '$lib/library';
  import {
    decryptionSummary,
    encryptionLabel,
    genericDriveLabel,
    readOffsetLabel,
    retriedLabel,
    rippedDrive,
    writeOffsetLabel,
  } from '$lib/rips';

  let view = $state<LoadState<ArtifactDetail>>({ kind: 'loading' });
  let tab = $state('summary');

  /** The tabs, in UX.md 5's order; `#topology` and the like open one directly. */
  const TAB_IDS = ['summary', 'components', 'topology', 'validation', 'lineage', 'burning'];

  onMount(async () => {
    const asked = location.hash.slice(1);
    if (TAB_IDS.includes(asked)) tab = asked;
    view = await loadArtifact(page.params.id ?? '');
  });

  /** After a check ends, show what it found; keep the page if the reload fails. */
  async function reload() {
    const next = await loadArtifact(page.params.id ?? '');
    if (next.kind === 'ready') view = next;
  }

  function tabsFor(artifact: ArtifactDetail) {
    return [
      { id: 'summary', label: 'Summary' },
      { id: 'components', label: 'Components', count: artifact.component_count },
      { id: 'topology', label: 'Topology' },
      { id: 'validation', label: 'Validation', count: artifact.warnings.length || undefined },
      { id: 'lineage', label: 'Lineage' },
      { id: 'burning', label: 'Burning' },
    ];
  }
</script>

<svelte:head>
  <title>
    {view.kind === 'ready'
      ? `${view.data.source_filename ?? 'Artifact'} — Tangible`
      : 'Artifact — Tangible'}
  </title>
</svelte:head>

<div aria-live="polite" aria-busy={view.kind === 'loading'}>
  {#if view.kind === 'loading'}
    <p class="quiet">Loading artifact…</p>
  {:else if view.kind === 'error'}
    <p><a href="/library">Library</a></p>
    <ErrorPanel
      problem={view.problem}
      area="library"
      endpoint="GET /api/v1/artifacts/{'{id}'}"
      heading="Could not load this artifact"
      level={1}
    />
  {:else if view.kind === 'ready'}
    {@const artifact = view.data}
    {@const deleted = artifact.deletion?.state === 'deleted'}
    {@const pending = artifact.deletion?.state === 'pending'}
    {@const refusal = notBurnedAsItIs(artifact)}
    {@const burnable = !deleted && !pending && !refusal}

    <PageHeader
      title={artifact.source_filename ?? 'Artifact'}
      crumbs={[
        { label: 'Library', href: '/library' },
        { label: artifact.source_filename ?? 'Artifact' },
      ]}
    >
      {#snippet badges()}
        {#if isOriginal(artifact.origin) && !deleted}
          <Badge icon="lock" strong>Original, immutable</Badge>
        {:else}
          <Badge>{originLabel(artifact.origin)}</Badge>
        {/if}
        <Badge>{formatLabel(artifact.format)}</Badge>
        <StatusMark
          tone={validationTone(artifact.validation_state)}
          label={validationLabel(artifact.validation_state)}
        />
        <StatusMark
          tone={integrityTone(artifact.integrity_state)}
          label={integrityShort(artifact.integrity_state)}
        />
      {/snippet}
      {#snippet actions()}
        {#if burnable}
          <Button variant="primary" icon="burn" href={`/burns/new?artifact=${artifact.id}`}
            >Create burn job</Button
          >
        {/if}
        <Button href={`/api/v1/artifacts/${artifact.id}/manifest`} download="">
          Download manifest
        </Button>
      {/snippet}
    </PageHeader>

    {#if artifact.deletion}
      <!-- First, when it is going or gone: everything below reads differently. -->
      <Deletion
        artifactId={artifact.id}
        name={artifact.source_filename ?? 'this artifact'}
        origin={artifact.origin}
        deletion={artifact.deletion}
        onchanged={reload}
      />
    {/if}

    <Tabs label="About this artifact" tabs={tabsFor(artifact)} bind:selected={tab}>
      {#snippet panel(id)}
        {#if id === 'summary'}
          <section aria-labelledby="summary-heading">
            <h2 id="summary-heading">Summary</h2>
            <dl class="facts">
              <div>
                <dt>Identifier</dt>
                <dd><code>{artifact.id}</code></dd>
              </div>
              <div>
                <dt>Format</dt>
                <dd>
                  {formatLabel(artifact.format)}
                  <!-- Confidence is diagnostic and labelled as such, so it is
                       not mistaken for a guarantee. -->
                  <small class="quiet"
                    >(detection confidence {(artifact.format_confidence * 100).toFixed(0)}%)</small
                  >
                </dd>
              </div>
              <div>
                <dt>Size</dt>
                <dd class="num">{formatBytes(artifact.total_bytes)}</dd>
              </div>
              <div>
                <dt>Files</dt>
                <dd class="num">{artifact.component_count}</dd>
              </div>
              <div>
                <dt>Source</dt>
                <dd>{artifact.source_kind}</dd>
              </div>
              <div>
                <dt>Validation</dt>
                <dd>
                  <StatusMark
                    tone={validationTone(artifact.validation_state)}
                    label={validationLabel(artifact.validation_state)}
                  />
                </dd>
              </div>
              {#if artifact.unpacked_from}
                {@const from = artifact.unpacked_from}
                <div class="wide">
                  <dt>Unpacked from</dt>
                  <dd>
                    A {archiveLabel(from.format)} archive of {formatBytes(from.length_bytes)},
                    holding
                    {from.files}
                    {from.files === 1 ? 'file' : 'files'}. The archive itself is not kept.
                    <br /><small
                      >SHA-256 <code title={from.sha256}>{shortDigest(from.sha256)}</code></small
                    >
                    {#each from.notes as note (note)}
                      <br /><small>{note}</small>
                    {/each}
                  </dd>
                </div>
              {/if}
            </dl>

            <!-- Every imported original is immutable, and saying so is the
                 point of the tool rather than an implementation note. -->
            {#if isOriginal(artifact.origin) && !deleted}
              <Callout
                ><p>
                  <strong>Immutable original.</strong>
                  These bytes are never rewritten, repaired or recompressed. Conversions create separate
                  derivatives that record what produced them.
                </p></Callout
              >
            {/if}
          </section>

          {#if !deleted}
            <Integrity
              artifactId={artifact.id}
              integrityState={artifact.integrity_state}
              checkedAt={artifact.integrity_checked_at}
              onchecked={reload}
            />
          {/if}

          <Identification artifactId={artifact.id} />

          {#if artifact.ripped}
            {@const ripped = artifact.ripped}
            {@const readOffset = readOffsetLabel(ripped.engine, ripped.cd_read)}
            <section aria-labelledby="ripped-heading">
              <h2 id="ripped-heading">Ripped from a disc</h2>
              {#if ripped.decrypted_at_acquisition}
                <!-- Said plainly: these bytes are not what is on the disc. -->
                <Callout tone="warn"
                  ><p>
                    <strong>Decrypted when ripped.</strong>
                    This is not a faithful copy of the disc, and it will not match a preservation database.
                  </p></Callout
                >
              {/if}
              <dl class="facts">
                <div>
                  <dt>Drive</dt>
                  <dd>{rippedDrive(ripped)}</dd>
                </div>
                <div>
                  <dt>Read with</dt>
                  <dd>{ripped.engine} {ripped.engine_version}</dd>
                </div>
                {#if readOffset}
                  <div>
                    <dt>Read offset</dt>
                    <dd>{readOffset}</dd>
                  </div>
                {/if}
                {#if ripped.cd_read}
                  <div>
                    <dt>Write offset</dt>
                    <dd>{writeOffsetLabel(ripped.cd_read)}</dd>
                  </div>
                  <div>
                    <dt>Generic drive</dt>
                    <dd>{genericDriveLabel(ripped.cd_read.generic_drive)}</dd>
                  </div>
                {/if}
                <div>
                  <dt>Read errors</dt>
                  <dd>
                    {#if ripped.read_errors}
                      {ripped.read_errors.unreadable_sectors} unreadable, {ripped.read_errors
                        .retried_sectors} retried
                    {:else if ripped.cd_read}
                      {retriedLabel(ripped.cd_read)}
                    {:else}
                      Not counted by this tool
                    {/if}
                  </dd>
                </div>
                <div>
                  <dt>Encryption</dt>
                  <dd>{encryptionLabel(ripped.encryption)}</dd>
                </div>
                {#if ripped.decryption}
                  <div>
                    <dt>Decrypted with</dt>
                    <dd>{decryptionSummary(ripped.decryption)}</dd>
                  </div>
                {/if}
              </dl>
              <p><a href={`/rips/${ripped.rip_id}`}>The rip that read it</a></p>
            </section>
          {/if}
        {:else if id === 'components'}
          <section aria-labelledby="components-heading">
            <h2 id="components-heading">Components</h2>
            <Table caption="Files making up this artifact">
              <thead>
                <tr>
                  <th scope="col">Path</th>
                  <th scope="col">Role</th>
                  <th scope="col" class="num">Size</th>
                  <th scope="col">SHA-256</th>
                  <th scope="col"><span class="visually-hidden">Download</span></th>
                </tr>
              </thead>
              <tbody>
                {#each artifact.components as component (component.logical_path)}
                  <tr>
                    <th scope="row"><code>{component.logical_path}</code></th>
                    <td>{component.role.replace(/_/g, ' ')}</td>
                    <td class="num">{formatBytes(component.length_bytes)}</td>
                    <td>
                      <code title={component.sha256}>{shortDigest(component.sha256)}</code>
                    </td>
                    <td>
                      {#if deleted}
                        <span class="quiet">Removed</span>
                      {:else}
                        <a href={componentUrl(artifact.id, component.id)} download
                          >Download <span class="visually-hidden">{component.logical_path}</span></a
                        >
                      {/if}
                    </td>
                  </tr>
                {/each}
              </tbody>
            </Table>
          </section>
        {:else if id === 'topology'}
          {@const shape = artifact.disc_shape}
          <section aria-labelledby="topology-heading">
            <h2 id="topology-heading">Topology</h2>
            <div class="topology">
              <figure class="drawing">
                <Disc {shape} size={176} hole="var(--bg)" />
                <figcaption>
                  {familyLabel(shape?.media_family)}{#if shape?.sector_count}, {shape.sector_count.toLocaleString()}
                    sectors{/if}
                </figcaption>
              </figure>
              <div class="layout">
                {#if artifact.disc}
                  <p>
                    {discSummary(artifact.disc)} Times are minutes, seconds and frames, 75 frames to the
                    second, as a CD counts them.
                  </p>
                {:else if shape?.sector_count}
                  <p>
                    One image of {shape.sector_count.toLocaleString()} sectors, with no tracks of its
                    own.
                  </p>
                {:else}
                  <p class="quiet">The manifest does not say how this disc is laid out.</p>
                {/if}
                <p class="quiet note">
                  The drawing is to scale: each track's ring takes the area its sectors take on the
                  disc, audio tracks set apart as on a pressed CD.
                </p>
              </div>
            </div>
            {#if artifact.disc}
              <Table caption="Tracks on this disc">
                <thead>
                  <tr>
                    <th scope="col" class="num">Track</th>
                    <th scope="col">Kind</th>
                    <th scope="col">Starts</th>
                    <th scope="col">Length</th>
                    <th scope="col">Pregap</th>
                    <th scope="col">Details</th>
                  </tr>
                </thead>
                <tbody>
                  {#each artifact.disc.tracks as track (track.number)}
                    <tr>
                      <th scope="row" class="num">{track.number}</th>
                      <td>{trackModeLabel(track.mode)}</td>
                      <td><code>{msf(track.start_lba)}</code></td>
                      <td><code>{msf(track.sector_count)}</code></td>
                      <td>{track.pregap_sectors > 0 ? msf(track.pregap_sectors) : 'None'}</td>
                      <td>
                        <div class="details">
                          {#if trackDetails(track)}<span>{trackDetails(track)}</span>{/if}
                          {#if track.indexes.length > 1 + (track.pregap_sectors > 0 ? 1 : 0)}
                            <small>{track.indexes.length} index points</small>
                          {/if}
                          <small>from <code>{track.component_path}</code></small>
                        </div>
                      </td>
                    </tr>
                  {/each}
                </tbody>
              </Table>
            {/if}
          </section>
        {:else if id === 'validation'}
          <section aria-labelledby="findings-heading">
            <h2 id="findings-heading">Structural findings</h2>
            {#if artifact.warnings.length > 0}
              <p>
                The image was preserved exactly as received. These findings describe what was
                observed; nothing was altered to resolve them.
              </p>
              <ul class="findings">
                {#each artifact.warnings as warning, index (index)}
                  <li><StatusMark tone="warn" label={warning} /></li>
                {/each}
              </ul>
            {:else}
              <p class="quiet">None.</p>
            {/if}
          </section>
          <FormatReport {artifact} />
        {:else if id === 'lineage'}
          <Lineage artifactId={artifact.id} />
        {:else if id === 'burning'}
          <section aria-labelledby="compatibility-heading">
            <h2 id="compatibility-heading">Burn compatibility</h2>
            {#if deleted}
              <p class="quiet">Deleted: there is nothing left to burn.</p>
            {:else}
              <!-- Detection is not a burn plan. Saying "unknown" is accurate
                   and saying anything stronger would be a promise nothing has
                   checked. -->
              <p>
                <StatusMark tone="idle" label="Target compatibility unknown" />
              </p>
              <p>
                Structural validity does not establish that a written disc will be accepted by any
                particular player, console or drive.
              </p>
              {#if pending}
                <Callout tone="warn" role="note"
                  ><p>
                    <strong>Not burned while it is pending deletion.</strong> Keep it to burn it.
                  </p></Callout
                >
              {:else if refusal}
                <Callout tone="warn" role="note"
                  ><p>
                    <strong>Not burned as it is.</strong>
                    {refusal}
                  </p></Callout
                >
              {:else}
                <p>
                  <Button variant="primary" icon="burn" href={`/burns/new?artifact=${artifact.id}`}
                    >Create burn job</Button
                  >
                </p>
                <p class="quiet">
                  It queues a burn. Nothing is written until a worker claims it and its preflight
                  passes.
                </p>
              {/if}
            {/if}
          </section>
        {/if}
      {/snippet}
    </Tabs>

    {#if !artifact.deletion}
      <div class="danger">
        <Deletion
          artifactId={artifact.id}
          name={artifact.source_filename ?? 'this artifact'}
          origin={artifact.origin}
          deletion={artifact.deletion}
          onchanged={reload}
        />
      </div>
    {/if}
  {/if}
</div>

<style>
  .facts {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(14rem, 1fr));
    gap: var(--space-3) var(--space-6);
    margin: 0 0 var(--space-4);
  }
  .facts > div {
    min-width: 0;
  }
  .facts .wide {
    grid-column: 1 / -1;
  }
  dt {
    color: var(--text-muted);
    font-size: var(--text-sm);
  }
  dd {
    margin: 0;
    font-weight: var(--weight-medium);
    overflow-wrap: anywhere;
  }
  section + :global(section),
  :global(section) + section {
    margin-top: var(--space-6);
  }
  h2 {
    font-size: var(--text-md);
    margin-top: 0;
  }
  .topology {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-6);
    align-items: center;
    margin-bottom: var(--space-4);
  }
  .drawing {
    margin: 0;
    display: grid;
    justify-items: center;
    gap: var(--space-2);
  }
  .drawing figcaption {
    color: var(--text-muted);
    font-size: var(--text-sm);
  }
  .layout {
    flex: 1 1 20rem;
    max-width: 65ch;
  }
  .note {
    font-size: var(--text-sm);
  }
  .findings {
    list-style: none;
    padding: 0;
    display: grid;
    gap: var(--space-2);
  }
  .findings :global(.mark) {
    white-space: normal;
    font-weight: var(--weight-medium);
  }
  .details {
    display: grid;
    gap: 0.125rem;
  }
  .danger {
    margin-top: var(--space-8);
    padding-top: var(--space-4);
    border-top: 1px solid var(--line);
  }
</style>
