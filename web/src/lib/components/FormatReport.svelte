<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // "Ask for support" for an image Tangible did not recognise, or read with
  // doubts. The server describes the image's structure, never its contents,
  // with names replaced; the review dialog shows all of it; the operator's
  // own browser opens the issue form. Nothing is shown with reports off, or
  // for an image recognised cleanly.
  import {
    draftFromReport,
    formatReportOffered,
    loadFormatReport,
    reportSettingsOnce,
    type ReportDraft,
    type ReportSettings,
  } from '$lib/report';
  import type { Problem } from '$lib/session';
  import ErrorPanel from './ErrorPanel.svelte';
  import ReportPreview from './ReportPreview.svelte';

  let {
    artifact,
  }: {
    artifact: { id: string; format: string; format_confidence: number; validation_state: string };
  } = $props();

  let settings = $state<ReportSettings | null>(null);
  let draft = $state<ReportDraft | null>(null);
  let open = $state(false);
  let preparing = $state(false);
  let problem = $state<Problem | null>(null);

  const unrecognised = $derived(artifact.format === 'unknown');

  $effect(() => {
    let current = true;
    void reportSettingsOnce().then((loaded) => {
      if (current) settings = loaded;
    });
    return () => {
      current = false;
    };
  });

  async function prepare() {
    preparing = true;
    problem = null;
    const result = await loadFormatReport(artifact.id);
    preparing = false;
    if (result.kind === 'error') {
      problem = result.problem;
      return;
    }
    draft = draftFromReport(result.data);
    open = true;
  }
</script>

{#if settings?.enabled && settings.issues_url && formatReportOffered(artifact)}
  <section aria-labelledby="format-report-heading" class="format-report">
    <h2 id="format-report-heading">
      {unrecognised ? 'Tangible did not recognise this image' : 'Ask about this format'}
    </h2>
    <p>
      {unrecognised
        ? 'It was imported and kept exactly as received, but no detector claimed it.'
        : 'It was read with doubts or warnings.'}
      Tangible can describe its structure for the developers: what detection saw, each file by extension
      and size, and the first bytes of each binary file. File names and paths are replaced, and nothing
      leaves this page until you have read it.
    </p>
    <button type="button" onclick={prepare} disabled={preparing}>
      {preparing ? 'Preparing…' : 'Ask for support…'}
    </button>
    {#if problem}
      <ErrorPanel
        {problem}
        area="import"
        endpoint="GET /api/v1/artifacts/{'{id}'}/format-report"
        heading="The report could not be prepared"
        level={3}
        refusal
      />
    {/if}
    {#if draft}
      <ReportPreview
        {draft}
        issuesUrl={settings.issues_url}
        bind:open
        heading="Review this format report"
      />
    {/if}
  </section>
{/if}

<style>
  .format-report {
    border: 1px solid currentColor;
    padding: 0.75rem 1rem;
    margin: 1rem 0;
  }
  button {
    padding: 0.5rem 1rem;
  }
</style>
