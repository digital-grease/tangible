<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Ripping a disc someone owns into the library. Pick the drive holding it;
  // its worker reads the disc, sends what it read, and the result is
  // imported like an upload. Reading changes nothing on the disc.
  import { goto } from '$app/navigation';
  import { onDestroy, onMount } from 'svelte';
  import { watch } from '$lib/events';
  import ErrorPanel from '$lib/components/ErrorPanel.svelte';
  import Badge from '$lib/components/ui/Badge.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import Panel from '$lib/components/ui/Panel.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import Table from '$lib/components/ui/Table.svelte';
  import RipDialog from '$lib/components/RipDialog.svelte';
  import { formatBytes } from '$lib/library';
  import {
    requestRip,
    loadRips,
    ripStateLabel,
    ripStateTone,
    type RipRequest,
    type RipView,
  } from '$lib/rips';
  import type { Problem } from '$lib/session';
  import { driveModel, loadDrives, type DriveView } from '$lib/workers';

  type View<T> =
    { kind: 'loading' } | { kind: 'error'; problem: Problem } | { kind: 'ready'; data: T };

  let drives = $state<View<DriveView[]>>({ kind: 'loading' });
  let rips = $state<View<RipView[]>>({ kind: 'loading' });
  let refusal = $state<Problem | null>(null);
  let asking = $state<string | null>(null);
  // The drive whose rip dialog is open: only drives with a choice to make
  // get one, those that can decrypt or that read CDs with redumper.
  let choosing = $state<DriveView | null>(null);
  let dialogOpen = $state(false);
  let stopWatching: (() => void) | undefined;

  /** How often to poll, while something is going, where the browser has no event stream. */
  const FALLBACK_MS = 3_000;

  async function refresh() {
    const [driveList, ripList] = await Promise.all([loadDrives(), loadRips()]);
    if (driveList.kind === 'ok') drives = { kind: 'ready', data: driveList.data.items };
    else if (drives.kind !== 'ready') drives = { kind: 'error', problem: driveList.problem };
    if (ripList.kind === 'ok') rips = { kind: 'ready', data: ripList.data.items };
    else if (rips.kind !== 'ready') rips = { kind: 'error', problem: ripList.problem };
  }

  onMount(() => {
    void refresh();
    // A rip or a drive changing is news; the timer only matters while a rip
    // is going.
    stopWatching = watch(
      ['rips', 'workers'],
      (reason) => {
        if (reason === 'event' || (rips.kind === 'ready' && rips.data.some((rip) => rip.is_open)))
          void refresh();
      },
      { fallbackMs: FALLBACK_MS },
    );
  });

  onDestroy(() => stopWatching?.());

  /** Rip straight away, or ask first where the drive offers a choice. */
  function choose(drive: DriveView) {
    if (drive.can_decrypt_rips || drive.can_rip_with_redumper) {
      refusal = null;
      choosing = drive;
      dialogOpen = true;
      return;
    }
    void rip(drive, {});
  }

  async function rip(drive: DriveView, options: RipRequest) {
    asking = drive.id;
    refusal = null;
    const result = await requestRip(drive.id, options);
    asking = null;
    dialogOpen = false;
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

<PageHeader
  title="Rip a disc"
  description="Put a disc you own in a drive and rip it into the library. The drive's worker reads it, sends what it read, and it is imported and kept exactly as read. Reading changes nothing on the disc. CDs and DVDs, for now; a CD becomes a table of contents and its data, a DVD an image."
/>

<p class="note">
  An encrypted disc is ripped only by a drive that offers to decrypt it: its worker runs an image
  built to decrypt, and an administrator has turned decryption on. Elsewhere an encrypted disc is
  refused and nothing read from it is kept.
</p>

{#if refusal}
  <ErrorPanel
    problem={refusal}
    area="import"
    endpoint="POST /api/v1/drives/{'{id}'}/rips"
    refusal
  />
{/if}

<div class="stack">
  <div aria-busy={drives.kind === 'loading'}>
    <Panel title="Drives" flush>
      {#if drives.kind === 'loading'}
        <p class="quiet padded">Loading drives…</p>
      {:else if drives.kind === 'error'}
        <div class="padded">
          <ErrorPanel problem={drives.problem} area="other" endpoint="GET /api/v1/drives" />
        </div>
      {:else if drives.data.length === 0}
        <p class="quiet padded">
          No drive is enrolled. An administrator adds one on the Workers page.
        </p>
      {:else}
        <Table caption="Drives that can be asked to rip">
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
                <td>
                  <StatusMark
                    tone={drive.worker_status === 'online' ? 'ok' : 'warn'}
                    label={`${drive.worker_name} (${drive.worker_status})`}
                  />
                </td>
                <td>
                  <div class="rip">
                    <Button
                      size="sm"
                      icon="rip"
                      onclick={() => choose(drive)}
                      disabled={asking !== null}
                    >
                      {asking === drive.id ? 'Asking…' : `Rip the disc in ${drive.name}`}
                    </Button>
                    {#if drive.can_rip_with_redumper}
                      <Badge>Corrects CD read offsets</Badge>
                    {/if}
                    {#if drive.can_decrypt_rips}
                      <Badge>Can decrypt</Badge>
                    {/if}
                  </div>
                </td>
              </tr>
            {/each}
          </tbody>
        </Table>
      {/if}
    </Panel>
  </div>

  {#if choosing}
    <RipDialog
      drive={choosing}
      bind:open={dialogOpen}
      asking={asking !== null}
      onstart={(options) => choosing && rip(choosing, options)}
    />
  {/if}

  <div aria-live="polite" aria-busy={rips.kind === 'loading'}>
    <Panel title="Recent rips" flush>
      {#if rips.kind === 'loading'}
        <p class="quiet padded">Loading rips…</p>
      {:else if rips.kind === 'error'}
        <div class="padded">
          <ErrorPanel problem={rips.problem} area="import" endpoint="GET /api/v1/rips" />
        </div>
      {:else if rips.data.length === 0}
        <p class="quiet padded">Nothing has been ripped yet.</p>
      {:else}
        <Table caption="Rips, newest first">
          <thead>
            <tr>
              <th scope="col">Asked for</th>
              <th scope="col">Drive</th>
              <th scope="col">State</th>
              <th scope="col" class="num">Read</th>
            </tr>
          </thead>
          <tbody>
            {#each rips.data as item (item.id)}
              <tr>
                <th scope="row"><a href={`/rips/${item.id}`}>{when(item.created_at)}</a></th>
                <td>{item.drive_name}</td>
                <td>
                  <StatusMark
                    tone={ripStateTone(item.state)}
                    label={ripStateLabel(item.state)}
                    pill
                  />
                </td>
                <td class="num"
                  >{item.bytes_received > 0 ? formatBytes(item.bytes_received) : '·'}</td
                >
              </tr>
            {/each}
          </tbody>
        </Table>
      {/if}
    </Panel>
  </div>
</div>

<style>
  .note {
    max-width: 75ch;
    color: var(--text-muted);
    margin: calc(-1 * var(--space-3)) 0 var(--space-5);
  }
  .stack {
    display: grid;
    gap: var(--space-4);
  }
  .rip {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    align-items: center;
  }
  .padded {
    padding: var(--space-3) var(--space-4);
  }
  .quiet {
    color: var(--text-muted);
    margin: 0;
  }
</style>
