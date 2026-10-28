<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // What identifies an artifact: the games of loaded DATs its preservation
  // hashes match, and the hashes themselves, which the server computes in
  // the background. A match only names a game; it changes nothing.
  import { onDestroy, onMount } from 'svelte';
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
      <p><strong>Could not load the identification.</strong> {problem.detail}</p>
    {:else if view === null}
      <p>Loading identification…</p>
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
              <p>
                <span aria-hidden="true">{match.kind === 'exact' ? '✓' : '!'}</span>
                <strong>{matchLabel(match)}</strong>
                <br /><small
                  >In {matchSource(match)}{#if match.category}, {match.category}{/if}.</small
                >
              </p>
              {#if match.description && match.description !== match.game_name}
                <p>{match.description}</p>
              {/if}
              <ul>
                {#each match.files as file, index (index)}
                  <li>
                    <span aria-hidden="true">{file.matched_by ? '✓' : '✕'}</span>
                    {fileLine(file)}
                  </li>
                {/each}
              </ul>
              {#if hint}<p><small>{hint}</small></p>{/if}
            </li>
          {/each}
        </ul>
      {/if}

      <p>
        <span aria-hidden="true"
          >{view.state === 'done' ? '✓' : view.state === 'failed' ? '✕' : '·'}</span
        >
        {identificationStateLabel(view)}{#if view.hashed_at}, {when(view.hashed_at)}{/if}.
      </p>

      {#if view.state === 'done' && (view.components.length > 0 || view.tracks.length > 0)}
        <details>
          <summary>Preservation hashes</summary>
          <p>
            <small
              >CRC32, MD5 and SHA-1, as preservation databases list a dump. Each track is hashed as
              Redump cuts it, its pregap included where the image holds it.</small
            >
          </p>
          <table>
            <caption class="visually-hidden">Preservation hashes</caption>
            <thead>
              <tr>
                <th scope="col">File or track</th>
                <th scope="col">Size</th>
                <th scope="col">CRC32</th>
                <th scope="col">MD5</th>
                <th scope="col">SHA-1</th>
              </tr>
            </thead>
            <tbody>
              {#each view.components as component (component.logical_path)}
                <tr>
                  <th scope="row">{component.logical_path}</th>
                  <td>{formatBytes(component.length_bytes)}</td>
                  <td><code>{component.crc32 ?? '—'}</code></td>
                  <td><code>{component.md5 ?? '—'}</code></td>
                  <td><code>{component.sha1 ?? '—'}</code></td>
                </tr>
              {/each}
              {#each view.tracks as track (track.number)}
                <tr>
                  <th scope="row">Track {track.number}</th>
                  <td>{formatBytes(track.length_bytes)}</td>
                  <td><code>{track.crc32 ?? '—'}</code></td>
                  <td><code>{track.md5 ?? '—'}</code></td>
                  <td><code>{track.sha1 ?? '—'}</code></td>
                </tr>
              {/each}
            </tbody>
          </table>
        </details>
      {/if}

      {#if mayAsk && view.state !== 'pending'}
        <p>
          <button type="button" onclick={askAgain} disabled={asking}>
            {asking ? 'Asking…' : 'Hash again'}
          </button>
          <br />
          <small
            >Reads every stored file of this artifact once more to compute its hashes, then matches
            them against the loaded DATs. Useful after its files were restored from a backup.
            Changes nothing.</small
          >
        </p>
      {/if}
    {/if}
  </div>

  <div role="alert">
    {#if refusal}<p><strong>Not asked.</strong> {refusal.detail}</p>{/if}
  </div>
</section>

<style>
  .matches > li {
    border: 1px solid currentColor;
    padding: 0.75rem;
    margin-bottom: 0.75rem;
    list-style: none;
  }
  .matches {
    padding-left: 0;
  }
  details {
    overflow-x: auto;
  }
  table {
    border-collapse: collapse;
  }
  th,
  td {
    text-align: left;
    padding: 0.25rem 0.5rem;
    border-bottom: 1px solid currentColor;
  }
  code {
    word-break: break-all;
  }
  .visually-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
  }
</style>
