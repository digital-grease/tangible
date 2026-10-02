<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { onMount } from 'svelte';
  import { goto } from '$app/navigation';
  import { completeSetup, loadSetupStatus, MIN_PASSWORD_CHARS, type Problem } from '$lib/session';

  type View =
    | { kind: 'loading' }
    | { kind: 'needed' }
    | { kind: 'done' }
    | { kind: 'error'; problem: Problem };

  let view = $state<View>({ kind: 'loading' });
  let username = $state('');
  let password = $state('');
  let confirm = $state('');
  let refusal = $state<string | null>(null);
  let submitting = $state(false);

  onMount(async () => {
    const status = await loadSetupStatus(fetch);
    view =
      status.kind === 'error'
        ? { kind: 'error', problem: status.problem }
        : status.data.needed
          ? { kind: 'needed' }
          : { kind: 'done' };
  });

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (submitting) return;
    refusal = null;
    if (password !== confirm) {
      refusal = 'The two passwords are different.';
      return;
    }
    submitting = true;
    const result = await completeSetup(username, password, fetch);
    submitting = false;
    if (result.kind === 'error') {
      if (result.problem.code === 'CONFLICT') {
        // Somebody else finished setup first. That is worth knowing.
        view = { kind: 'done' };
        refusal = null;
        return;
      }
      refusal = result.problem.detail;
      return;
    }
    password = '';
    confirm = '';
    void goto('/', { replaceState: true });
  }
</script>

<svelte:head><title>Set up — Tangible</title></svelte:head>

<h1>Set up Tangible</h1>

<section aria-live="polite" aria-busy={view.kind === 'loading'}>
  {#if view.kind === 'loading'}
    <p>Checking whether this server is set up…</p>
  {:else if view.kind === 'error'}
    <div role="alert">
      <h2>{view.problem.title}</h2>
      <p>{view.problem.detail}</p>
    </div>
  {:else if view.kind === 'done'}
    <h2>Already set up</h2>
    <p>
      This server already has an administrator, so setup is closed. If that was not you, stop the
      server and find out who it was before going further.
    </p>
    <p><a href="/login">Sign in</a></p>
  {:else}
    <p>
      This server has no accounts yet. The account you create now is its administrator: it can
      create other accounts, enroll burn workers, and do everything else. Setup closes as soon as it
      exists.
    </p>

    <form onsubmit={submit}>
      <p>
        <label for="username">Username</label><br />
        <input
          id="username"
          name="username"
          autocomplete="username"
          autocapitalize="none"
          spellcheck="false"
          bind:value={username}
          required
          aria-describedby="username-hint"
        />
        <br />
        <small id="username-hint"
          >Letters, digits, dots, dashes and underscores. Not case sensitive.</small
        >
      </p>
      <p>
        <label for="password">Password</label><br />
        <input
          id="password"
          name="password"
          type="password"
          autocomplete="new-password"
          minlength={MIN_PASSWORD_CHARS}
          bind:value={password}
          required
          aria-describedby="password-hint"
        />
        <br />
        <small id="password-hint"
          >At least {MIN_PASSWORD_CHARS} characters. A passphrase is fine.</small
        >
      </p>
      <p>
        <label for="confirm">Password again</label><br />
        <input
          id="confirm"
          name="confirm"
          type="password"
          autocomplete="new-password"
          bind:value={confirm}
          required
        />
      </p>

      <div role="alert">
        {#if refusal}
          <p><strong>Not set up.</strong> {refusal}</p>
        {/if}
      </div>

      <button type="submit" disabled={submitting}>
        {submitting ? 'Creating…' : 'Create the administrator'}
      </button>
    </form>
  {/if}
</section>
