<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // The Overview (UX.md 3; docs/plans/ui-overhaul.md, "Pages"): what needs
  // somebody, what is under way, what just happened, and the server's
  // health. Actionable state, not decorative figures.
  //
  // The tiles and tables come from GET /api/v1/dashboard, read again when the
  // event stream says something changed and on a slow timer whatever it
  // says. The events panel lists what the stream has said since the page
  // opened, in words built from each event's type and state: events carry
  // no names, so a burn's title is shown only where the page already has it.
  import { onMount } from 'svelte';
  import Icon from '$lib/components/ui/Icon.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import Panel from '$lib/components/ui/Panel.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import Table from '$lib/components/ui/Table.svelte';
  import { attemptOutcomeLabel, jobStateLabel } from '$lib/burns';
  import { loadDashboard, needsSomebody, type Dashboard } from '$lib/dashboard';
  import { EVENT_TYPES, RESET, TOPICS, sharedStream, watch } from '$lib/events';
  import { loadHealth, type HealthState } from '$lib/health';
  import {
    burnNames,
    eventLine,
    jobTone,
    outcomeTone,
    tiles,
    when,
    type EventLine,
  } from '$lib/overview';
  import { sessionRole } from '$lib/session';

  /** How many events the panel keeps, newest first. */
  const KEPT = 40;

  let health = $state<HealthState>({ kind: 'loading' });
  let board = $state<Dashboard | null>(null);
  let boardProblem = $state<string | null>(null);
  let lines = $state<(EventLine & { at: Date; key: number })[]>([]);
  let live = $state(false);
  let counter = 0;

  async function refreshBoard() {
    const dashboard = await loadDashboard();
    // Signed out, the page is the health check alone; anything else is
    // said rather than hidden.
    if (dashboard.kind === 'ok') {
      board = dashboard.data;
      boardProblem = null;
    } else if (dashboard.problem.code !== 'UNAUTHENTICATED') {
      boardProblem = dashboard.problem.detail;
    }
  }

  onMount(() => {
    void loadHealth().then((answer) => (health = answer));
    void refreshBoard();
    const stopWatching = watch(['burns', 'imports', 'workers', 'library'], refreshBoard);

    const stream = sharedStream();
    live = stream.available;
    const types = [...TOPICS.flatMap((topic) => EVENT_TYPES[topic]), RESET];
    const stopListening = stream.available
      ? stream.subscribe(types, (event) => {
          const line = eventLine(event, burnNames(board));
          if (!line) return;
          lines = [{ ...line, at: new Date(), key: counter++ }, ...lines].slice(0, KEPT);
        })
      : () => {};
    return () => {
      stopWatching();
      stopListening();
    };
  });

  const administrator = $derived(sessionRole() === 'administrator');
  const healthTone = $derived(
    health.kind === 'ready'
      ? 'ok'
      : health.kind === 'degraded'
        ? 'warn'
        : health.kind === 'unreachable'
          ? 'bad'
          : 'idle',
  );
  const healthWords = $derived(
    health.kind === 'loading'
      ? 'Checking the server…'
      : health.kind === 'unreachable'
        ? 'The web UI could not reach the API'
        : `${health.kind === 'ready' ? 'Ready' : 'Degraded'}${
            health.liveness ? `, version ${health.liveness.version}` : ''
          }`,
  );
</script>

<svelte:head><title>Overview — Tangible</title></svelte:head>

<PageHeader title="Overview">
  {#snippet badges()}
    {#if board}
      {#if needsSomebody(board)}
        <StatusMark tone="warn" label="Needs somebody" />
      {:else}
        <StatusMark tone="ok" label="Nothing needs you" />
      {/if}
    {/if}
  {/snippet}
</PageHeader>

{#if boardProblem}
  <div class="problem" role="alert">
    <StatusMark tone="bad" label="The overview could not be loaded" />
    <p>{boardProblem}</p>
  </div>
{/if}

{#if board}
  <ul class="tiles" aria-label="At a glance">
    {#each tiles(board) as tile (tile.key)}
      <li class="tile">
        <a class="tile-label" href={tile.href}>
          <Icon name={tile.icon} size={14} />{tile.label}
        </a>
        <p class="figure num">
          {tile.figure}{#if tile.unit}<span class="unit">{tile.unit}</span>{/if}
        </p>
        <p class="summary">{tile.summary}</p>
        {#if tile.fill !== undefined}
          <!-- Drawn only; the sentence above says the same in words. -->
          <div class="meter" aria-hidden="true">
            <span style:width="{Math.round(tile.fill * 100)}%"></span>
          </div>
        {/if}
        {#if tile.mark}<StatusMark tone={tile.mark.tone} label={tile.mark.label} />{/if}
      </li>
    {/each}
  </ul>

  <div class="columns">
    <div class="stack">
      <Panel title="Needs attention" aside={String(board.attention.length)} flush>
        {#if board.attention.length === 0}
          <p class="nothing">Nothing needs attention.</p>
        {:else}
          <Table caption="Burns needing attention">
            <thead>
              <tr>
                <th scope="col">Disc</th>
                <th scope="col">State</th>
                <th scope="col">Code</th>
                <th scope="col">Since</th>
              </tr>
            </thead>
            <tbody>
              {#each board.attention as burn (burn.burn_job_id)}
                <tr>
                  <th scope="row">
                    <a href={`/burns/${burn.burn_job_id}`}>{burn.title}, disc {burn.disc_number}</a>
                  </th>
                  <td>
                    <StatusMark tone={jobTone(burn.state)} label={jobStateLabel(burn.state)} pill />
                  </td>
                  <td><code>{burn.error_code ?? '·'}</code></td>
                  <td><time datetime={burn.at}>{when(burn.at)}</time></td>
                </tr>
              {/each}
            </tbody>
          </Table>
        {/if}
      </Panel>

      <Panel title="Latest burns" flush>
        {#snippet actions()}<a class="more" href="/burns">All burns</a>{/snippet}
        {#if board.recent_burns.length === 0}
          <p class="nothing">No burn has ended yet.</p>
        {:else}
          <Table caption="Latest burns">
            <thead>
              <tr>
                <th scope="col">When</th>
                <th scope="col">Disc</th>
                <th scope="col">How it ended</th>
              </tr>
            </thead>
            <tbody>
              {#each board.recent_burns as burn (burn.burn_job_id + burn.at)}
                <tr>
                  <td><time datetime={burn.at}>{when(burn.at)}</time></td>
                  <th scope="row">
                    <a href={`/burns/${burn.burn_job_id}`}>{burn.title}, disc {burn.disc_number}</a>
                  </th>
                  <td>
                    <StatusMark
                      tone={outcomeTone(burn.state)}
                      label={attemptOutcomeLabel(burn.state)}
                      pill
                    />
                  </td>
                </tr>
              {/each}
            </tbody>
          </Table>
        {/if}
      </Panel>
    </div>

    <Panel title="Events" aside={live ? 'Live' : 'Every minute'}>
      {#if !live}
        <p class="nothing">
          This browser has no event stream, so the page reads everything again every minute.
        </p>
      {:else if lines.length === 0}
        <p class="nothing">Nothing has happened since this page opened.</p>
      {:else}
        <ol class="log" aria-label="Events since this page opened, newest first">
          {#each lines as line (line.key)}
            <li>
              <time class="num" datetime={line.at.toISOString()}>{when(line.at)}</time>
              <span class="topic">{line.topic}</span>
              <span class="words">
                <StatusMark tone={line.tone} label={line.words} />
                {#if line.href}<a href={line.href} class="open">Open</a>{/if}
              </span>
            </li>
          {/each}
        </ol>
      {/if}
    </Panel>
  </div>
{/if}

<section class="health" aria-labelledby="health-heading">
  <h2 id="health-heading" class="visually-hidden">Health</h2>
  <details>
    <summary>
      <span aria-live="polite" aria-busy={health.kind === 'loading'}>
        <StatusMark tone={healthTone} label={healthWords} />
      </span>
      {#if health.kind === 'unreachable'}<span class="reason">{health.reason}</span>{/if}
    </summary>
    {#if health.kind === 'ready' || health.kind === 'degraded'}
      {#if health.readiness.checks.length === 0}
        <p>No dependencies reported.</p>
      {:else}
        <Table caption="Dependency status" showCaption>
          <thead>
            <tr
              ><th scope="col">Dependency</th><th scope="col">Status</th><th scope="col">Detail</th
              ></tr
            >
          </thead>
          <tbody>
            {#each health.readiness.checks as check (check.name)}
              <tr>
                <th scope="row">{check.name}</th>
                <td>
                  <StatusMark
                    tone={check.status === 'up' ? 'ok' : 'bad'}
                    label={check.status === 'up' ? 'Up' : 'Down'}
                  />
                </td>
                <td>{check.detail ?? '·'}</td>
              </tr>
            {/each}
          </tbody>
        </Table>
      {/if}
    {/if}
    {#if administrator}
      <p class="more-health">
        <a href="/diagnostics">Diagnostics</a> checks storage, tools, workers, drives and recent failures
        as well.
      </p>
    {/if}
  </details>
</section>

<style>
  .tiles {
    list-style: none;
    margin: 0 0 var(--space-5);
    padding: 0;
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(12rem, 1fr));
    gap: var(--space-3);
  }
  .tile {
    display: grid;
    gap: var(--space-1);
    align-content: start;
    padding: var(--space-3) var(--space-4);
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--radius-lg);
  }
  .tile-label {
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--text-xs);
    font-weight: var(--weight-strong);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--text-muted);
    text-decoration: none;
  }
  .tile-label:hover {
    color: var(--text);
    text-decoration: underline;
  }
  .figure {
    margin: var(--space-1) 0 0;
    font-family: var(--font-mono);
    font-size: var(--text-xl);
    font-weight: var(--weight-strong);
    line-height: 1.1;
  }
  .unit {
    margin-left: var(--space-2);
    font-family: var(--font-sans);
    font-size: var(--text-sm);
    font-weight: var(--weight-medium);
    color: var(--text-muted);
  }
  .summary {
    margin: 0;
    font-size: var(--text-sm);
    color: var(--text-muted);
  }
  .tile :global(.mark) {
    font-size: var(--text-sm);
    margin-top: var(--space-1);
  }
  .meter {
    height: 0.25rem;
    border-radius: var(--radius-full);
    background: var(--line-2);
    overflow: hidden;
    margin-top: var(--space-1);
  }
  .meter span {
    display: block;
    height: 100%;
    background: var(--accent);
  }
  .columns {
    display: grid;
    grid-template-columns: minmax(0, 1.5fr) minmax(0, 1fr);
    gap: var(--space-4);
    align-items: start;
  }
  .stack {
    display: grid;
    gap: var(--space-4);
  }
  .nothing {
    margin: 0;
    padding: var(--space-3) var(--space-4);
    color: var(--text-muted);
  }
  .more {
    font-size: var(--text-sm);
  }
  .log {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    font-size: var(--text-sm);
  }
  .log li {
    display: grid;
    grid-template-columns: 4.5rem 5.5rem minmax(0, 1fr);
    gap: var(--space-2);
    padding: 0.3125rem 0;
    border-bottom: 1px solid var(--line-2);
    align-items: baseline;
  }
  .log li:last-child {
    border-bottom: 0;
  }
  .log time {
    color: var(--text-muted);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
  }
  .topic {
    color: var(--accent);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
  }
  .words {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    align-items: baseline;
    min-width: 0;
  }
  .words :global(.mark) {
    font-weight: var(--weight-medium);
    white-space: normal;
  }
  .open {
    font-size: var(--text-xs);
  }
  .problem {
    padding: var(--space-3) var(--space-4);
    margin-bottom: var(--space-4);
    background: var(--surface);
    border: 1px solid var(--line);
    border-left: 4px solid var(--bad);
    border-radius: var(--radius);
  }
  .problem p {
    margin: var(--space-1) 0 0;
  }
  .health {
    margin-top: var(--space-6);
    padding-top: var(--space-3);
    border-top: 1px solid var(--line);
  }
  summary {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-3);
    align-items: center;
    cursor: pointer;
    width: fit-content;
  }
  .reason {
    color: var(--text-muted);
  }
  .more-health {
    margin-top: var(--space-3);
    color: var(--text-muted);
    font-size: var(--text-sm);
  }
  details[open] :global(.table-region) {
    margin-top: var(--space-3);
  }
  @media (max-width: 1099px) {
    .columns {
      grid-template-columns: minmax(0, 1fr);
    }
  }
</style>
