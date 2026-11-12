<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // What identifies an artifact: the games of loaded DATs its preservation
  // hashes match, and the hashes themselves, which the server computes in
  // the background. A match only names a game; it changes nothing.
  import { onDestroy, onMount } from 'svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import Table from '$lib/components/ui/Table.svelte';
  import type { Tone } from '$lib/components/ui/tone';
  import {
    FOLLOW_EVERY_MS,
    identificationStateLabel,
    isPending,
    loadIdentification,
    matchLabel,
    matchSource,
    matchedByLabel,
    partialHint,
    rehash,
    type DatMatchFile,
    type Identification,
  } from '$lib/identification';
  import { formatBytes } from '$lib/library';
  import { sessionRole, type Problem } from '$lib/session';

  let { artifactId }: { artifactId: string } = $props();

  /** The hashes' state as a tone, beside identificationStateLabel's words. */
  function stateTone(state: string): Tone {
    return state === 'done' ? 'ok' : state === 'failed' ? 'bad' : 'busy';
  }

  let view = $state<Identification | null>(null);
  let problem = $state<Problem | null>(null);
  let asking = $state(false);
  let refusal = $state<Problem | null>(null);
  let timer: ReturnType<typeof setTimeout> | undefined;

  onDestroy(() => clearTimeout(timer));

  // A viewer may not ask; the server refuses anyone else it should.
  const mayAsk = sessionRole() !== 'viewer';

  async function load() {
    clearTimeout(timer);
    const result = await loadIdentification(artifactId);
    if (result.kind === 'error') {
      problem = result.problem;
      return;
    }
    problem = null;
    view = result.data;
    // Hashing takes as long as reading the image once; look again until done.
    if (isPending(result.data)) timer = setTimeout(load, FOLLOW_EVERY_MS);
  }

  onMount(load);

  async function askAgain() {
    if (asking) return;
    asking = true;
    refusal = null;
    const result = await rehash(artifactId);
    asking = false;
    if (result.kind === 'error') {
      refusal = result.problem;
      return;
    }
    view = result.data;
    if (isPending(result.data)) timer = setTimeout(load, FOLLOW_EVERY_MS);
  }

  /** One file of a matched game, with its size, and what matched it. */
  function fileLine(file: DatMatchFile): string {
    const size =
      file.size === null || file.size === undefined ? '' : ` (${formatBytes(file.size)})`;
    return `${file.name}${size}: ${matchedByLabel(file)}`;
  }

  function when(value: string): string {
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
  }
</script>

<section aria-labelledby="identification-heading">
  <h2 id="identification-heading">Identification</h2>

  <div aria-live="polite">
    {#if problem}
      <p class="problem"><strong>Could not load the identification.</strong> {problem.detail}</p>
    {:else if view === null}
      <p class="quiet">Loading identification…</p>
    {:else}
      {#if view.matches.length === 0}
        {#if view.state === 'done'}
          <p>
            Matches no game in the loaded DATs. An administrator loads DATs on the
            <a href="/dats">DATs</a> page.
          </p>
        {/if}
      {:else}
        <ul class="matches">
          {#each view.matches as match (match.dat_id + match.game_name)}
            {@const hint = partialHint(match)}
            <li>
              <StatusMark tone={match.kind === 'exact' ? 'ok' : 'warn'} label={matchLabel(match)} />
              <p class="source">
                In {matchSource(match)}{#if match.category}, {match.category}{/if}.
              </p>
              {#if match.description && match.description !== match.game_name}
                <p>{match.description}</p>
              {/if}
              <ul class="files">
                {#each match.files as file, index (index)}
                  <li>
                    <StatusMark tone={file.matched_by ? 'ok' : 'bad'} label={fileLine(file)} />
                  </li>
                {/each}
              </ul>
              {#if hint}<p class="source">{hint}</p>{/if}
            </li>
          {/each}
        </ul>
      {/if}

      <p class="state">
        <StatusMark
          tone={stateTone(view.state)}
          label={`${identificationStateLabel(view)}${view.hashed_at ? `, ${when(view.hashed_at)}` : ''}.`}
        />
      </p>

      {#if view.state === 'done' && (view.components.length > 0 || view.tracks.length > 0)}
        <details>
          <summary>Preservation hashes</summary>
          <p class="hint">
            CRC32, MD5 and SHA-1, as preservation databases list a dump. Each track is hashed as
            Redump cuts it, its pregap included where the image holds it.
          </p>
          <div class="hashes">
            <Table caption="Preservation hashes">
              <thead>
                <tr>
                  <th scope="col">File or track</th>
                  <th scope="col" class="num">Size</th>
                  <th scope="col">CRC32</th>
                  <th scope="col">MD5</th>
                  <th scope="col">SHA-1</th>
                </tr>
              </thead>
              <tbody>
                {#each view.components as component (component.logical_path)}
                  <tr>
                    <th scope="row">{component.logical_path}</th>
                    <td class="num">{formatBytes(component.length_bytes)}</td>
                    <td>{@render hash(component.crc32)}</td>
                    <td>{@render hash(component.md5)}</td>
                    <td>{@render hash(component.sha1)}</td>
                  </tr>
                {/each}
                {#each view.tracks as track (track.number)}
                  <tr>
                    <th scope="row">Track {track.number}</th>
                    <td class="num">{formatBytes(track.length_bytes)}</td>
                    <td>{@render hash(track.crc32)}</td>
                    <td>{@render hash(track.md5)}</td>
                    <td>{@render hash(track.sha1)}</td>
                  </tr>
                {/each}
              </tbody>
            </Table>
          </div>
        </details>
      {/if}

      {#if mayAsk && view.state !== 'pending'}
        <div class="action">
          <Button onclick={askAgain} disabled={asking}>
            {asking ? 'Asking…' : 'Hash again'}
          </Button>
          <p class="hint">
            Reads every stored file of this artifact once more to compute its hashes, then matches
            them against the loaded DATs. Useful after its files were restored from a backup.
            Changes nothing.
          </p>
        </div>
      {/if}
    {/if}
  </div>

  <div role="alert">
    {#if refusal}<p class="problem"><strong>Not asked.</strong> {refusal.detail}</p>{/if}
  </div>
</section>

{#snippet hash(value: string | null | undefined)}
  {#if value}<code>{value}</code>{:else}<span class="quiet">None</span>{/if}
{/snippet}

<style>
  .matches {
    display: grid;
    gap: var(--space-3);
    margin: 0 0 var(--space-3);
    padding: 0;
    list-style: none;
    max-width: 50rem;
  }
  .matches > li {
    display: grid;
    gap: var(--space-1);
    padding: var(--space-3) var(--space-4);
    background: var(--surface-2);
    border: 1px solid var(--line);
    border-radius: var(--radius);
  }
  .matches p {
    margin: 0;
  }
  .source {
    font-size: var(--text-sm);
    color: var(--text-muted);
  }
  .files {
    display: grid;
    gap: var(--space-1);
    margin: var(--space-1) 0 0;
    padding: 0;
    list-style: none;
  }
  .files :global(.mark),
  .state :global(.mark) {
    font-weight: var(--weight-medium);
  }
  .files :global(.mark) {
    white-space: normal;
  }
  details {
    margin-bottom: var(--space-3);
  }
  summary {
    cursor: pointer;
    font-weight: var(--weight-medium);
  }
  .hashes {
    margin-top: var(--space-2);
    border: 1px solid var(--line);
    border-radius: var(--radius);
    overflow: hidden;
  }
  .action {
    display: grid;
    gap: var(--space-2);
    justify-items: start;
  }
  .hint {
    max-width: 75ch;
    margin: var(--space-2) 0 0;
    font-size: var(--text-sm);
    color: var(--text-muted);
  }
  .action .hint {
    margin: 0;
  }
  .problem {
    color: var(--bad);
  }
  code {
    word-break: break-all;
  }
</style>
