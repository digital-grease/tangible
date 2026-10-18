<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // The deployment's state as a checklist, for administrators: the same
  // report `tangible doctor --report` prints. It carries no paths, library
  // file names, or worker and drive names, so it can be copied into an issue
  // as it is; "Report a problem" prepares it as one, for review.
  import { onMount } from 'svelte';
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import ReportPreview from '$lib/components/ReportPreview.svelte';
  import {
    downloadName,
    loadDiagnostics,
    loadDiagnosticsReport,
    statusCounts,
    statusLabel,
    type DiagnosticsView,
  } from '$lib/diagnostics';
  import {
    copyText,
    draftFromReport,
    reportSettingsOnce,
    type ReportDraft,
    type ReportSettings,
  } from '$lib/report';
  import type { Problem } from '$lib/session';

  type View =
    | { kind: 'loading' }
    | { kind: 'forbidden' }
    | { kind: 'error'; problem: Problem }
    | { kind: 'ready'; data: DiagnosticsView };

  let view = $state<View>({ kind: 'loading' });
  let refreshing = $state(false);
  let status = $state('');
  let settings = $state<ReportSettings | null>(null);
  let draft = $state<ReportDraft | null>(null);
  let open = $state(false);
  let preparing = $state(false);
  let refusal = $state<Problem | null>(null);

  async function refresh() {
    refreshing = true;
    status = '';
    const result = await loadDiagnostics();
    refreshing = false;
    if (result.kind === 'ok') {
      view = { kind: 'ready', data: result.data };
    } else if (result.problem.code === 'FORBIDDEN') {
      view = { kind: 'forbidden' };
    } else if (view.kind === 'ready') {
      // Keep what was there, and say the refresh failed.
      refusal = result.problem;
    } else {
      view = { kind: 'error', problem: result.problem };
    }
  }

  onMount(() => {
    void refresh();
    void reportSettingsOnce().then((loaded) => (settings = loaded));
  });

  async function copy(markdown: string) {
    status = (await copyText(markdown))
      ? 'The report is on your clipboard, as Markdown.'
      : 'It could not be copied automatically. Download it instead.';
  }

  function download(data: DiagnosticsView) {
    const blob = new Blob([data.markdown], { type: 'text/markdown' });
    const url = URL.createObjectURL(blob);
    const link = document.createElement('a');
    link.href = url;
    link.download = downloadName(data.generated_at);
    link.click();
    URL.revokeObjectURL(url);
    status = `Saved as ${link.download}.`;
  }

  async function report() {
    preparing = true;
    refusal = null;
    const result = await loadDiagnosticsReport();
    preparing = false;
    if (result.kind === 'error') {
      refusal = result.problem;
      return;
    }
    draft = draftFromReport(result.data);
    open = true;
  }

  function when(value: string): string {
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
  }
</script>

<svelte:head><title>Diagnostics — Tangible</title></svelte:head>

<h1>Diagnostics</h1>

<p>
  The state of this deployment as a checklist: the database, storage, tools, workers and drives,
  recent failures and the settings that matter for support. It names no paths, library file names,
  workers or drives, so it can be shared as it is. The same report is printed by
  <code>docker compose run --rm server doctor --report</code>, which works when this page does not.
</p>

<section aria-live="polite" aria-busy={view.kind === 'loading' || refreshing}>
  {#if view.kind === 'loading'}
    <p>Collecting diagnostics…</p>
  {:else if view.kind === 'forbidden'}
    <p>Only an administrator can see diagnostics.</p>
  {:else if view.kind === 'error'}
    <ErrorPanel
      problem={view.problem}
      area="deployment"
      endpoint="GET /api/v1/diagnostics"
      heading="Could not collect diagnostics"
    />
  {:else}
    {@const data = view.data}
    {@const counts = statusCounts(data)}
    <h2>Overall: {statusLabel(data.status)}</h2>
    <p>
      {counts.problem} problem{counts.problem === 1 ? '' : 's'}, {counts.warn}
      warning{counts.warn === 1 ? '' : 's'}, {counts.ok} OK. Collected {when(data.generated_at)}
      from {data.build}.
    </p>

    <div class="actions">
      <button type="button" onclick={refresh} disabled={refreshing}>
        {refreshing ? 'Collecting…' : 'Collect again'}
      </button>
      <button type="button" onclick={() => copy(data.markdown)}>Copy as Markdown</button>
      <button type="button" onclick={() => download(data)}>Download .md</button>
      {#if settings?.enabled && settings.issues_url}
        <button type="button" onclick={report} disabled={preparing}>
          {preparing ? 'Preparing…' : 'Report a problem…'}
        </button>
      {/if}
    </div>
    <p role="status">{status}</p>

    {#if refusal}
      <ErrorPanel problem={refusal} area="deployment" refusal />
    {/if}

    {#each data.sections as section (section.title)}
      <table>
        <caption>{section.title}</caption>
        <thead>
          <tr>
            <th scope="col">Check</th>
            <th scope="col">Status</th>
            <th scope="col">Detail</th>
          </tr>
        </thead>
        <tbody>
          {#each section.checks as check (check.name)}
            <tr class={check.status}>
              <th scope="row">{check.name}</th>
              <td><strong>{statusLabel(check.status)}</strong></td>
              <td>{check.detail}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/each}

    {#if draft && settings?.issues_url}
      <ReportPreview
        {draft}
        issuesUrl={settings.issues_url}
        bind:open
        heading="Review this diagnostics report"
      />
    {/if}
  {/if}
</section>

<style>
  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
  }
  button {
    padding: 0.5rem 1rem;
  }
  table {
    border-collapse: collapse;
    width: 100%;
    margin-top: 1.5rem;
  }
  caption {
    text-align: left;
    font-weight: 600;
    font-size: 1.15em;
    padding-bottom: 0.25rem;
  }
  th,
  td {
    text-align: left;
    padding: 0.5rem;
    border-bottom: 1px solid currentColor;
    vertical-align: top;
  }
  td:last-child {
    overflow-wrap: anywhere;
  }
  /* The status is in words; the border only repeats it. */
  tr.problem th {
    border-left: 4px solid currentColor;
  }
  tr.warn th {
    border-left: 4px dashed currentColor;
  }
</style>
