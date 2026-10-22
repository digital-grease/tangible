<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Asking for a rip on a drive that can decrypt (ADR 0013). Decryption is
  // off unless the box is ticked; ticking it shows the choice of original.
  // The server decides whether the rip may decrypt; this only asks.
  import { ripOptions, type KeepChoice, type RipRequest } from '$lib/rips';
  import type { DriveView } from '$lib/workers';

  let {
    drive,
    open = $bindable(false),
    asking = false,
    onstart,
  }: {
    drive: DriveView;
    open?: boolean;
    asking?: boolean;
    onstart: (options: RipRequest) => void;
  } = $props();

  let dialog: HTMLDialogElement | undefined = $state();
  let decrypt = $state(false);
  let keep = $state<KeepChoice>('encrypted_original');
  let opener: HTMLElement | null = null;

  $effect(() => {
    if (!dialog) return;
    if (open && !dialog.open) {
      opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
      decrypt = false;
      keep = 'encrypted_original';
      dialog.showModal();
    } else if (!open && dialog.open) {
      dialog.close();
    }
  });

  function closed() {
    open = false;
    opener?.focus();
  }

  function start(event: SubmitEvent) {
    event.preventDefault();
    onstart(ripOptions(decrypt, keep));
  }
</script>

<dialog bind:this={dialog} onclose={closed} aria-labelledby="rip-dialog-heading">
  <form onsubmit={start}>
    <h2 id="rip-dialog-heading">Rip the disc in {drive.name}</h2>
    <p>
      The worker {drive.worker_name} reads the disc in this drive and sends what it read; it is imported
      into the library like an upload. Reading changes nothing on the disc.
    </p>

    <label class="choice">
      <input type="checkbox" bind:checked={decrypt} aria-describedby="rip-dialog-notice" />
      Decrypt an encrypted disc
    </label>
    <p id="rip-dialog-notice" class="notice">
      Whether ripping an encrypted disc is lawful where you are is yours to determine. Decryption
      uses only the libraries built into this worker's image. No key is kept, logged or sent
      anywhere. A disc that turns out not to be encrypted is ripped as it is.
    </p>

    {#if decrypt}
      <fieldset>
        <legend>What to keep</legend>
        <label class="choice">
          <input type="radio" name="keep" value="encrypted_original" bind:group={keep} />
          Keep the encrypted image, and a decrypted copy
        </label>
        <p class="hint">
          The image is a faithful copy of the disc, still encrypted. The decrypted copy is kept
          beside it as a derivative made from the same disc. Reads the disc twice.
        </p>
        <label class="choice">
          <input type="radio" name="keep" value="decrypted_original" bind:group={keep} />
          Keep only a decrypted image (not a faithful copy of the disc)
        </label>
        <p class="hint">
          Quicker. It is recorded as decrypted when ripped, and will not match a preservation
          database.
        </p>
      </fieldset>
    {/if}

    <div class="actions">
      <button type="submit" disabled={asking}>{asking ? 'Asking…' : 'Start ripping'}</button>
      <button type="button" onclick={() => (open = false)}>Cancel</button>
    </div>
  </form>
</dialog>

<style>
  dialog {
    max-width: 40rem;
    width: calc(100% - 2rem);
    max-height: calc(100% - 2rem);
    padding: 1.25rem;
  }
  dialog::backdrop {
    background: rgb(0 0 0 / 0.4);
  }
  .choice {
    display: flex;
    gap: 0.5rem;
    align-items: baseline;
    margin-top: 0.75rem;
  }
  .notice {
    border: 1px solid currentColor;
    padding: 0.5rem 0.75rem;
  }
  .hint {
    margin: 0.25rem 0 0 1.75rem;
    font-size: 0.9em;
  }
  fieldset {
    margin-top: 1rem;
  }
  .actions {
    display: flex;
    gap: 0.75rem;
    margin-top: 1.25rem;
  }
  button {
    padding: 0.5rem 1rem;
  }
</style>
