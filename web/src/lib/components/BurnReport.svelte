<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // "Report this burn" and "Worked on my drive" for one attempt. The server
  // prepares the report, scrubbed; the review dialog shows all of it; the
  // operator's own browser opens the issue form. Nothing is shown at all on
  // a server with reports turned off.
  import { reportOptions, type BurnAttemptView } from '$lib/burns';
  import {
    draftFromReport,
    loadBurnReport,
    type BurnReportKind,
    type ReportDraft,
    type ReportSettings,
  } from '$lib/report';
  import type { Problem } from '$lib/session';
  import ReportPreview from './ReportPreview.svelte';

  let {
    attempt,
    settings,
  }: {
    attempt: Pick<BurnAttemptView, 'id' | 'state' | 'engine' | 'attempt_number'>;
    settings: ReportSettings | null;
  } = $props();

  let draft = $state<ReportDraft | null>(null);
  let heading = $state('Review this report');
  let open = $state(false);
  let preparing = $state<BurnReportKind | null>(null);
  let problem = $state<Problem | null>(null);

  const options = $derived(reportOptions(attempt));

  async function prepare(kind: BurnReportKind) {
    preparing = kind;
    problem = null;
    const result = await loadBurnReport(attempt.id, kind);
    preparing = null;
    if (result.kind === 'error') {
      problem = result.problem;
      return;
    }
    draft = draftFromReport(result.data);
    heading = kind === 'worked' ? 'Review this hardware report' : 'Review this burn report';
    open = true;
  }
</script>

{#if settings?.enabled && settings.issues_url}
  <section aria-labelledby="report-heading-{attempt.id}" class="report">
    <h2 id="report-heading-{attempt.id}">Tell the developers</h2>
    <p>
      Tangible can fill in an issue for attempt {attempt.attempt_number}. It removes paths, file
      names, serial numbers and addresses, and shows you everything before anything leaves this
      page.
    </p>
    <div class="buttons">
      {#if options.problem}
        <button type="button" onclick={() => prepare('problem')} disabled={preparing !== null}>
          {preparing === 'problem' ? 'Preparing…' : 'Report a problem with this burn…'}
        </button>
      {/if}
      {#if options.worked}
        <button type="button" onclick={() => prepare('worked')} disabled={preparing !== null}>
          {preparing === 'worked' ? 'Preparing…' : 'Report this drive and disc as working…'}
        </button>
      {/if}
    </div>
    {#if problem}
      <div role="alert">
        <p>The report could not be prepared: {problem.detail}</p>
        <p>Error code: <code>{problem.code}</code></p>
      </div>
    {/if}
  </section>

  {#if draft}
    <ReportPreview {draft} issuesUrl={settings.issues_url} {heading} bind:open />
  {/if}
{/if}

<style>
  .report {
    border-top: 1px solid currentColor;
    margin-top: 1.5rem;
    padding-top: 0.5rem;
  }
  .buttons {
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
  }
  button {
    padding: 0.5rem 1rem;
  }
</style>
