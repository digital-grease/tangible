<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // The review step every issue report goes through. It shows everything the
  // issue will contain, already scrubbed, and says what was replaced. Nothing
  // is sent from here: "Open the issue form" opens a prefilled form in a new
  // tab, which the operator can still edit or discard.
  import {
    copyText,
    issueHost,
    issueLink,
    removedSummary,
    reportMarkdown,
    type ReportDraft,
  } from '$lib/report';

  let {
    draft,
    issuesUrl,
    open = $bindable(false),
    heading = 'Review this report',
  }: {
    draft: ReportDraft;
    issuesUrl: string;
    open?: boolean;
    heading?: string;
  } = $props();

  let dialog: HTMLDialogElement | undefined = $state();
  let status = $state('');
  let showText = $state(false);
  let opener: HTMLElement | null = null;

  const link = $derived(issueLink(issuesUrl, draft));
  const host = $derived(issueHost(issuesUrl));
  const markdown = $derived(reportMarkdown(draft));

  $effect(() => {
    if (!dialog) return;
    if (open && !dialog.open) {
      opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
      status = '';
      showText = false;
      dialog.showModal();
    } else if (!open && dialog.open) {
      dialog.close();
    }
  });

  function closed() {
    open = false;
    opener?.focus();
  }

  async function openIssue() {
    // Both copying and opening a tab need the click that started this, so
    // the copy starts first and the tab opens without waiting for it.
    const copying = link.kind === 'shortened' ? copyText(markdown) : null;
    window.open(link.url, '_blank', 'noopener,noreferrer');
    if (copying === null) {
      status = `The issue form opened in a new tab on ${host}. Nothing is sent until you submit it there.`;
      return;
    }
    const field = link.kind === 'shortened' ? link.field.label : 'log';
    if (await copying) {
      status = `The issue form opened in a new tab. The ${field} was too long for the link, so its start was left out; the full report is on your clipboard. Paste the log into the ${field} field there.`;
    } else {
      status = `The issue form opened in a new tab, but the ${field} was too long for the link and could not be copied for you. Use "Show as text" below and copy the log from there.`;
      showText = true;
    }
  }

  async function copyReport() {
    const copied = await copyText(markdown);
    status = copied
      ? 'The report is on your clipboard, as Markdown.'
      : 'It could not be copied automatically. Use "Show as text" below and copy it from there.';
    if (!copied) showText = true;
  }
</script>

<dialog bind:this={dialog} onclose={closed} aria-labelledby="report-heading">
  <h2 id="report-heading">{heading}</h2>
  <p>
    Nothing has been sent. This is everything the issue will contain. Read it, then open the issue
    form; you can still change or discard it there.
  </p>
  <p>
    {removedSummary(draft.removed)}
    {#if draft.truncated}
      The text was very long and was cut short.
    {/if}
  </p>

  <dl>
    {#each draft.fields as field (field.id)}
      <dt>{field.label}</dt>
      <dd>
        {#if !field.value}
          <em>Left for you to fill in.</em>
        {:else if field.id === draft.longField || field.value.includes('\n')}
          <textarea readonly rows="8" aria-label={field.label} value={field.value}></textarea>
        {:else}
          {field.value}
        {/if}
      </dd>
    {/each}
  </dl>

  {#if link.kind === 'shortened'}
    <p>
      The {link.field.label} is too long for a link. The form will get its end, and the full report will
      be copied to your clipboard so you can paste the rest.
    </p>
  {/if}

  <div class="actions">
    <button type="button" onclick={openIssue} aria-describedby="report-opens">
      Open the issue form on {host}
    </button>
    <span id="report-opens" class="hint">Opens in a new tab</span>
    <button type="button" onclick={copyReport}>Copy report</button>
    <button type="button" onclick={() => (showText = !showText)} aria-expanded={showText}>
      {showText ? 'Hide text' : 'Show as text'}
    </button>
    <button type="button" onclick={() => (open = false)}>Close</button>
  </div>

  {#if showText}
    <label>
      The report as Markdown
      <textarea readonly rows="12" value={markdown}></textarea>
    </label>
  {/if}

  <p role="status" aria-live="polite">{status}</p>
</dialog>

<style>
  dialog {
    max-width: 50rem;
    width: calc(100% - 2rem);
    max-height: calc(100% - 2rem);
    padding: 1.25rem;
  }
  dialog::backdrop {
    background: rgb(0 0 0 / 0.4);
  }
  dl {
    display: grid;
    grid-template-columns: minmax(8rem, max-content) 1fr;
    gap: 0.5rem 1rem;
  }
  dt {
    font-weight: 600;
  }
  dd {
    margin: 0;
    min-width: 0;
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
    align-items: center;
  }
  .hint {
    font-size: 0.875rem;
    margin-right: auto;
  }
  label {
    display: block;
    margin-top: 1rem;
  }
  textarea {
    display: block;
    width: 100%;
    font-family: monospace;
  }
  @media (max-width: 30rem) {
    dl {
      grid-template-columns: 1fr;
    }
  }
</style>
