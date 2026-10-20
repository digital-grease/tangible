<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Ripping a disc someone owns into the library. Pick the drive holding it;
  // its worker reads the disc, sends what it read, and the result is
  // imported like an upload. Reading changes nothing on the disc.
  import { goto } from '$app/navigation';
  import { onDestroy, onMount } from 'svelte';
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import { formatBytes } from '$lib/library';
  import { requestRip, loadRips, ripStateLabel, type RipView } from '$lib/rips';
  import type { Problem } from '$lib/session';
  import { driveModel, loadDrives, type DriveView } from '$lib/workers';

  type View<T> =
    { kind: 'loading' } | { kind: 'error'; problem: Problem } | { kind: 'ready'; data: T };

  let drives = $state<View<DriveView[]>>({ kind: 'loading' });
  let rips = $state<View<RipView[]>>({ kind: 'loading' });
  let refusal = $state<Problem | null>(null);
  let asking = $state<string | null>(null);
  let timer: ReturnType<typeof setInterval> | undefined;

  /** A poll while something is going; slow, since a CD takes minutes. */
  const RECONCILE_MS = 3_000;

  async function refresh() {
    const [driveList, ripList] = await Promise.all([loadDrives(), loadRips()]);
    if (driveList.kind === 'ok') drives = { kind: 'ready', data: driveList.data.items };
    else if (drives.kind !== 'ready') drives = { kind: 'error', problem: driveList.problem };
    if (ripList.kind === 'ok') rips = { kind: 'ready', data: ripList.data.items };
    else if (rips.kind !== 'ready') rips = { kind: 'error', problem: ripList.problem };
  }

  onMount(() => {
    void refresh();
    timer = setInterval(() => {
      if (rips.kind === 'ready' && rips.data.some((rip) => rip.is_open)) void refresh();
    }, RECONCILE_MS);
  });

  onDestroy(() => clearInterval(timer));

  async function rip(drive: DriveView) {
    asking = drive.id;
    refusal = null;
    const result = await requestRip(drive.id);
    asking = null;
    if (result.kind === 'error') {
      refusal = result.problem;
      return;
    }
    await goto(`/rips/${result.data.id}`);
  }

  function when(value: string | null | undefined): string {
    return value ? new Date(value).toLocaleString() : '—';
  }
</script>

<svelte:head><title>Rips — Tangible</title></svelte:head>

<h1>Rip a disc</h1>

<p>
  Put a disc you own in a drive and rip it into the library. The drive's worker reads it, sends what
  it read, and it is imported and kept exactly as read. Reading changes nothing on the disc. CDs and
  DVDs, for now; a CD becomes a table of contents and its data, a DVD an image.
</p>

{#if refusal}
  <ErrorPanel
    problem={refusal}
    area="import"
    endpoint="POST /api/v1/drives/{'{id}'}/rips"
    refusal
  />
{/if}

<section aria-labelledby="drives-heading" aria-busy={drives.kind === 'loading'}>
  <h2 id="drives-heading">Drives</h2>
  {#if drives.kind === 'loading'}
    <p>Loading drives…</p>
  {:else if drives.kind === 'error'}
    <ErrorPanel problem={drives.problem} area="other" endpoint="GET /api/v1/drives" />
  {:else if drives.data.length === 0}
    <p>No drive is enrolled. An administrator adds one on the Workers page.</p>
  {:else}
    <table>
      <caption class="visually-hidden">Drives that can be asked to rip</caption>
      <thead>
        <tr>
          <th scope="col">Drive</th>
          <th scope="col">Hardware</th>
          <th scope="col">Worker</th>
          <th scope="col"><span class="visually-hidden">Rip</span></th>
        </tr>
      </thead>
      <tbody>
        {#each drives.data as drive (drive.id)}
          <tr>
            <th scope="row">{drive.name}</th>
            <td>{driveModel(drive)}</td>
            <td>{drive.worker_name} ({drive.worker_status})</td>
            <td>
              <button type="button" onclick={() => rip(drive)} disabled={asking !== null}>
                {asking === drive.id ? 'Asking…' : `Rip the disc in ${drive.name}`}
              </button>
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</section>

<section aria-labelledby="rips-heading" aria-live="polite" aria-busy={rips.kind === 'loading'}>
  <h2 id="rips-heading">Recent rips</h2>
  {#if rips.kind === 'loading'}
    <p>Loading rips…</p>
  {:else if rips.kind === 'error'}
    <ErrorPanel problem={rips.problem} area="import" endpoint="GET /api/v1/rips" />
  {:else if rips.data.length === 0}
    <p>Nothing has been ripped yet.</p>
  {:else}
    <table>
      <caption class="visually-hidden">Rips, newest first</caption>
      <thead>
        <tr>
          <th scope="col">Asked for</th>
          <th scope="col">Drive</th>
          <th scope="col">State</th>
          <th scope="col">Read</th>
        </tr>
      </thead>
      <tbody>
        {#each rips.data as item (item.id)}
          <tr>
            <th scope="row"><a href={`/rips/${item.id}`}>{when(item.created_at)}</a></th>
            <td>{item.drive_name}</td>
            <td>{ripStateLabel(item.state)}</td>
            <td>{item.bytes_received > 0 ? formatBytes(item.bytes_received) : '—'}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</section>

<style>
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
  button {
    padding: 0.5rem 1rem;
  }
  section {
    margin-top: 1.5rem;
  }
  .visually-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip: rect(0 0 0 0);
    white-space: nowrap;
  }
</style>
