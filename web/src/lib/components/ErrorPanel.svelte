<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // How every page shows an error from the API: what went wrong in plain
  // words, then the detail, code and request id for whoever looks into it.
  //
  // Two shapes. A page that could not load shows its own heading, the
  // problem's title, and the technical detail folded away. A refusal (an
  // action the server turned down) leads with the title and shows the detail,
  // and is announced as soon as it appears.
  //
  // Where the code says the software itself went wrong, and the server offers
  // reports, "Report this…" asks the server to prepare the error as a bug
  // report, scrubbed, and opens the review dialog. Nothing is sent from here.
  import {
    draftFromReport,
    errorReportRequest,
    prepareErrorReport,
    reportSettingsOnce,
    suggestsDefect,
    type ReportArea,
    type ReportDraft,
    type ReportSettings,
  } from '$lib/report';
  import type { Problem } from '$lib/session';
  import ReportPreview from './ReportPreview.svelte';

  let {
    problem,
    area,
    endpoint,
    heading,
    level = 2,
    refusal = false,
  }: {
    problem: Problem;
    /** The part of Tangible, as the bug form's Area names it. */
    area: ReportArea;
    /** The failed call as a route, such as `GET /api/v1/artifacts/{id}`. */
    endpoint?: string;
    /** The page's own words for what failed; a refusal leads with the title instead. */
    heading?: string;
    level?: 1 | 2 | 3;
    refusal?: boolean;
  } = $props();

  const uid = $props.id();
  let settings = $state<ReportSettings | null>(null);
  let draft = $state<ReportDraft | null>(null);
  let open = $state(false);
  let preparing = $state(false);
  let status = $state('');

  $effect(() => {
    let current = true;
    void reportSettingsOnce().then((loaded) => {
      if (current) settings = loaded;
    });
    return () => {
      current = false;
    };
  });

  const offered = $derived(
    settings?.enabled === true && !!settings.issues_url && suggestsDefect(problem.code),
  );
  const tag = $derived(`h${level}`);

  async function prepare() {
    preparing = true;
    status = '';
    const result = await prepareErrorReport(errorReportRequest(problem, area, endpoint));
    preparing = false;
    if (result.kind === 'error') {
      status = `The report could not be prepared: ${result.problem.detail}`;
      return;
    }
    draft = draftFromReport(result.data);
    open = true;
  }
</script>

{#snippet identifiers()}
  <p>Error code: <code>{problem.code}</code></p>
  {#if problem.request_id}
    <p>Request id: <code>{problem.request_id}</code></p>
  {/if}
{/snippet}

{#snippet report()}
  {#if offered}
    <p>
      <button type="button" onclick={prepare} disabled={preparing}>
        {preparing ? 'Preparing…' : 'Report this…'}
      </button>
    </p>
    <p role="status">{status}</p>
    {#if draft && settings?.issues_url}
      <ReportPreview
        {draft}
        issuesUrl={settings.issues_url}
        bind:open
        heading="Review this bug report"
      />
    {/if}
  {/if}
{/snippet}

{#if refusal}
  <section class="refusal" aria-live="assertive" aria-labelledby="{uid}-heading">
    <svelte:element this={tag} id="{uid}-heading">{heading ?? problem.title}</svelte:element>
    {#if heading}
      <p>{problem.title}.</p>
    {/if}
    <p>{problem.detail}</p>
    {@render identifiers()}
    {@render report()}
  </section>
{:else}
  <div class="error">
    {#if heading}
      <svelte:element this={tag}>{heading}</svelte:element>
    {/if}
    <p>{problem.title}.</p>
    <details>
      <summary>Technical detail</summary>
      <p>{problem.detail}</p>
      {@render identifiers()}
    </details>
    {@render report()}
  </div>
{/if}

<style>
  .refusal {
    border: 2px solid currentColor;
    padding: 0.75rem 1rem;
    margin: 1rem 0;
  }
  p {
    overflow-wrap: anywhere;
  }
</style>
