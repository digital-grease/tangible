<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { onMount } from 'svelte';
  import { page } from '$app/state';
  import {
    formatBytes,
    formatLabel,
    loadArtifact,
    validationLabel,
    validationMark,
    type ArtifactDetail,
    type LoadState,
  } from '$lib/library';

  let view = $state<LoadState<ArtifactDetail>>({ kind: 'loading' });

  onMount(async () => {
    view = await loadArtifact(page.params.id ?? '');
  });
</script>

<svelte:head>
  <title>
    {view.kind === 'ready'
      ? `${view.data.source_filename ?? 'Artifact'} — Tangible`
      : 'Artifact — Tangible'}
  </title>
</svelte:head>

<p><a href="/library">← Library</a></p>

<div aria-live="polite" aria-busy={view.kind === 'loading'}>
  {#if view.kind === 'loading'}
    <p>Loading artifact…</p>
  {:else if view.kind === 'error'}
    <h1>Could not load this artifact</h1>
    <p>{view.problem.title}.</p>
    <details>
      <summary>Technical detail</summary>
      <p>{view.problem.detail}</p>
      <p>Error code: <code>{view.problem.code}</code></p>
    </details>
  {:else if view.kind === 'ready'}
    <h1>{view.data.source_filename ?? 'Artifact'}</h1>

    <section aria-labelledby="summary-heading">
      <h2 id="summary-heading">Summary</h2>
      <dl>
        <dt>Identifier</dt>
        <dd><code>{view.data.id}</code></dd>
        <dt>Format</dt>
        <dd>
          {formatLabel(view.data.format)}
          <!-- Confidence is diagnostic and labelled as such, so it is not
               mistaken for a guarantee. -->
          <small>(detection confidence {(view.data.format_confidence * 100).toFixed(0)}%)</small>
        </dd>
        <dt>Size</dt>
        <dd>{formatBytes(view.data.total_bytes)}</dd>
        <dt>Files</dt>
        <dd>{view.data.component_count}</dd>
        <dt>Source</dt>
        <dd>{view.data.source_kind}</dd>
        <dt>Validation</dt>
        <dd>
          <span aria-hidden="true">{validationMark(view.data.validation_state)}</span>
          {validationLabel(view.data.validation_state)}
        </dd>
      </dl>

      <!-- Every imported original is immutable, and saying so is the point of
           the tool rather than an implementation note. -->
      <p class="badge">
        <strong>Immutable original.</strong>
        These bytes are never rewritten, repaired or recompressed. Conversions create separate derivatives
        that record what produced them.
      </p>
    </section>

    {#if view.data.warnings.length > 0}
      <section aria-labelledby="findings-heading">
        <h2 id="findings-heading">Structural findings</h2>
        <p>
          The image was preserved exactly as received. These findings describe what was observed;
          nothing was altered to resolve them.
        </p>
        <ul>
          {#each view.data.warnings as warning, index (index)}
            <li>{warning}</li>
          {/each}
        </ul>
      </section>
    {/if}

    <section aria-labelledby="components-heading">
      <h2 id="components-heading">Components</h2>
      <table>
        <caption class="visually-hidden">Files making up this artifact</caption>
        <thead>
          <tr>
            <th scope="col">Path</th>
            <th scope="col">Role</th>
            <th scope="col">Size</th>
            <th scope="col">SHA-256</th>
          </tr>
        </thead>
        <tbody>
          {#each view.data.components as component (component.logical_path)}
            <tr>
              <th scope="row"><code>{component.logical_path}</code></th>
              <td>{component.role.replace(/_/g, ' ')}</td>
              <td>{formatBytes(component.length_bytes)}</td>
              <td>
                <code title={component.sha256}>{component.sha256.slice(0, 12)}…</code>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    </section>

    <section aria-labelledby="compatibility-heading">
      <h2 id="compatibility-heading">Burn compatibility</h2>
      <!-- Detection is not a burn plan. Saying "unknown" is accurate and
           saying anything stronger would be a promise nothing has checked. -->
      <p>
        Target compatibility unknown. Structural validity does not establish that a written disc
        will be accepted by any particular player, console or drive.
      </p>
      <p>
        <a href={`/burns/new?artifact=${view.data.id}`}>Create burn job</a>
        — queues a burn. Nothing is written until a worker claims it and its preflight passes.
      </p>
    </section>

    <section aria-labelledby="manifest-heading">
      <h2 id="manifest-heading">Manifest</h2>
      <p>
        <a href={`/api/v1/artifacts/${view.data.id}/manifest`} download> Download manifest </a>
        — the portable description of this artifact, including every component digest.
      </p>
    </section>
  {/if}
</div>

<style>
  dl {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 0.25rem 1rem;
  }
  dt {
    font-weight: 600;
  }
  dd {
    margin: 0;
  }
  table {
    border-collapse: collapse;
    width: 100%;
  }
  th,
  td {
    text-align: left;
    padding: 0.5rem;
    border-bottom: 1px solid currentColor;
  }
  .badge {
    border: 1px solid currentColor;
    padding: 0.75rem;
  }
  .visually-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
  }
</style>
