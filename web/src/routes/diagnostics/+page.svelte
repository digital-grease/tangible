<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // The deployment's state as a checklist, for administrators: the same
  // report `tangible doctor --report` prints. It carries no paths, library
  // file names, or worker and drive names, so it can be copied into an issue
  // as it is; "Report a problem" prepares it as one, for review. In the new
  // design (docs/plans/ui-overhaul.md, U8): the overall state and the
  // actions in one panel, then a panel per section, each check's status a
  // mark.
  import { onMount } from 'svelte';
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import ReportPreview from '$lib/components/ReportPreview.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import EmptyState from '$lib/components/ui/EmptyState.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import Panel from '$lib/components/ui/Panel.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import Table from '$lib/components/ui/Table.svelte';
  import { when } from '$lib/overview';
  import {
    downloadName,
    loadDiagnostics,
    loadDiagnosticsReport,
    statusCounts,
    statusLabel,
    statusTone,
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

  /** The full date and time, for a time's tooltip. */
  function full(value: string): string {
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
  }
</script>

<svelte:head><title>Diagnostics — Tangible</title></svelte:head>

<PageHeader
  title="Diagnostics"
  description="The state of this deployment as a checklist: the database, storage, tools, workers and drives, recent failures and the settings that matter for support."
/>

<p class="lede">
  It names no paths, library file names, workers or drives, so it can be shared as it is. The same
  report is printed by <code>docker compose run --rm server doctor --report</code>, which works when
  this page does not.
</p>

<div aria-live="polite" aria-busy={view.kind === 'loading' || refreshing}>
  {#if view.kind === 'loading'}
    <p class="quiet">Collecting diagnostics…</p>
  {:else if view.kind === 'forbidden'}
    <EmptyState title="Administrators only" icon="lock">
      <p>Only an administrator can see diagnostics.</p>
    </EmptyState>
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
    <div class="stack">
      <section class="overall {statusTone(data.status)}" aria-labelledby="overall-heading">
        <div class="summary">
          <h2 id="overall-heading">Overall: {statusLabel(data.status)}</h2>
          <p>
            {counts.problem} problem{counts.problem === 1 ? '' : 's'}, {counts.warn}
            warning{counts.warn === 1 ? '' : 's'}, {counts.ok} OK. Collected
            <time datetime={data.generated_at} title={full(data.generated_at)}
              >{when(data.generated_at)}</time
            >
            from <code>{data.build}</code>.
          </p>
        </div>
        <div class="actions">
          <Button icon="refresh" onclick={refresh} disabled={refreshing}>
            {refreshing ? 'Collecting…' : 'Collect again'}
          </Button>
          <Button onclick={() => copy(data.markdown)}>Copy as Markdown</Button>
          <Button onclick={() => download(data)}>Download .md</Button>
          {#if settings?.enabled && settings.issues_url}
            <Button variant="primary" onclick={report} disabled={preparing}>
              {preparing ? 'Preparing…' : 'Report a problem…'}
            </Button>
          {/if}
        </div>
        <p role="status" class="status" class:shown={status !== ''}>{status}</p>
      </section>

      {#if refusal}
        <ErrorPanel problem={refusal} area="deployment" refusal />
      {/if}

      {#each data.sections as section (section.title)}
        <Panel title={section.title} flush>
          <Table caption={section.title}>
            <!-- The same widths in every section, so the columns line up
                 down the page. -->
            <colgroup>
              <col class="name" />
              <col class="state" />
              <col />
            </colgroup>
            <thead>
              <tr>
                <th scope="col">Check</th>
                <th scope="col">Status</th>
                <th scope="col">Detail</th>
              </tr>
            </thead>
            <tbody>
              {#each section.checks as check (check.name)}
                <tr>
                  <th scope="row">{check.name}</th>
                  <td>
                    <StatusMark tone={statusTone(check.status)} label={statusLabel(check.status)} />
                  </td>
                  <td class="detail">{check.detail}</td>
                </tr>
              {/each}
            </tbody>
          </Table>
        </Panel>
      {/each}
    </div>

    {#if draft && settings?.issues_url}
      <ReportPreview
        {draft}
        issuesUrl={settings.issues_url}
        bind:open
        heading="Review this diagnostics report"
      />
    {/if}
  {/if}
</div>

<style>
  .lede {
    margin: 0 0 var(--space-5);
    max-width: 85ch;
    color: var(--text-muted);
  }
  .stack {
    display: grid;
    gap: var(--space-5);
  }
  /* The overall state is said in its heading; the edge's colour repeats it. */
  .overall {
    padding: var(--space-4);
    background: var(--surface);
    border: 1px solid var(--line);
    border-left: 4px solid var(--tone);
    border-radius: var(--radius-lg);
  }
  .overall.ok {
    --tone: var(--ok);
  }
  .overall.warn {
    --tone: var(--warn);
  }
  .overall.bad {
    --tone: var(--bad);
  }
  .summary h2 {
    margin: 0 0 var(--space-1);
    font-size: var(--text-lg);
  }
  .summary p {
    margin: 0;
  }
  .summary {
    margin-bottom: var(--space-3);
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
  }
  .status {
    margin: 0;
    font-size: var(--text-sm);
  }
  .status.shown {
    margin-top: var(--space-2);
  }
  .detail {
    overflow-wrap: anywhere;
  }
  .stack :global(table) {
    table-layout: fixed;
  }
  .name {
    width: 16rem;
  }
  .state {
    width: 9rem;
  }
  @media (max-width: 700px) {
    .name {
      width: 9rem;
    }
    .state {
      width: 7rem;
    }
  }
  .stack :global(tbody th) {
    overflow-wrap: anywhere;
  }
</style>
