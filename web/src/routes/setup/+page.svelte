<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Setting up (docs/plans/ui-overhaul.md, U8): the first account, the
  // administrator, on a server with none; closed once it exists. The layout
  // puts this in the way-in card.
  import { onMount } from 'svelte';
  import { goto } from '$app/navigation';
  import Button from '$lib/components/ui/Button.svelte';
  import Field from '$lib/components/ui/Field.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
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

<div aria-live="polite" aria-busy={view.kind === 'loading'}>
  {#if view.kind === 'loading'}
    <p class="quiet">Checking whether this server is set up…</p>
  {:else if view.kind === 'error'}
    <div role="alert" class="problem-box">
      <h2>{view.problem.title}</h2>
      <p>{view.problem.detail}</p>
    </div>
  {:else if view.kind === 'done'}
    <h2 class="state"><StatusMark tone="warn" label="Already set up" /></h2>
    <p>
      This server already has an administrator, so setup is closed. If that was not you, stop the
      server and find out who it was before going further.
    </p>
    <Button variant="primary" href="/login">Sign in</Button>
  {:else}
    <p class="lede">
      This server has no accounts yet. The account you create now is its administrator: it can
      create other accounts, enroll burn workers, and do everything else. Setup closes as soon as it
      exists.
    </p>

    <form class="form" onsubmit={submit}>
      <Field
        label="Username"
        hint="Letters, digits, dots, dashes and underscores. Not case sensitive."
      >
        {#snippet children({ id, describedBy })}
          <input
            {id}
            name="username"
            autocomplete="username"
            autocapitalize="none"
            spellcheck="false"
            bind:value={username}
            required
            aria-describedby={describedBy}
          />
        {/snippet}
      </Field>
      <Field
        label="Password"
        hint={`At least ${MIN_PASSWORD_CHARS} characters. A passphrase is fine.`}
      >
        {#snippet children({ id, describedBy })}
          <input
            {id}
            name="password"
            type="password"
            autocomplete="new-password"
            minlength={MIN_PASSWORD_CHARS}
            bind:value={password}
            required
            aria-describedby={describedBy}
          />
        {/snippet}
      </Field>
      <Field label="Password again">
        {#snippet children({ id })}
          <input
            {id}
            name="confirm"
            type="password"
            autocomplete="new-password"
            bind:value={confirm}
            required
          />
        {/snippet}
      </Field>

      <!-- The refusal takes room only when there is one. -->
      <div class="submit">
        <div role="alert">
          {#if refusal}
            <p class="problem"><strong>Not set up.</strong> {refusal}</p>
          {/if}
        </div>
        <Button type="submit" variant="primary" disabled={submitting}>
          {submitting ? 'Creating…' : 'Create the administrator'}
        </Button>
      </div>
    </form>
  {/if}
</div>

<style>
  .lede {
    margin: 0 0 var(--space-5);
  }
  .form {
    display: grid;
    gap: var(--space-4);
  }
  .form input {
    width: 100%;
  }
  .form :global(.btn) {
    width: 100%;
  }
  .problem {
    margin: 0 0 var(--space-3);
    color: var(--bad);
  }
  .submit {
    display: grid;
  }
  .state {
    margin: 0 0 var(--space-2);
    font-size: var(--text-md);
  }
  .problem-box {
    padding: var(--space-3) var(--space-4);
    border-left: 4px solid var(--bad);
    background: color-mix(in srgb, var(--bad) 6%, var(--surface));
  }
  .problem-box h2 {
    margin: 0 0 var(--space-1);
    font-size: var(--text-md);
  }
  .problem-box p {
    margin: 0;
  }
</style>
