<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Signing in (UX.md 2; docs/plans/ui-overhaul.md, U8): through the
  // identity provider when one is set up, or with a password. The layout
  // puts this in the way-in card.
  import { goto } from '$app/navigation';
  import Button from '$lib/components/ui/Button.svelte';
  import Field from '$lib/components/ui/Field.svelte';
  import { page } from '$app/state';
  import { onMount } from 'svelte';
  import {
    loadOidcStatus,
    oidcRefusal,
    oidcStartUrl,
    safeNext,
    signIn,
    signInRefusal,
    type OidcStatus,
  } from '$lib/session';

  let username = $state('');
  let password = $state('');
  let refusal = $state<string | null>(null);
  let submitting = $state(false);
  let oidc = $state<OidcStatus | null>(null);

  const next = $derived(page.url.searchParams.get('next'));
  /** Why signing in through the provider did not work, when it came back saying so. */
  const oidcError = $derived(page.url.searchParams.get('oidc_error'));

  onMount(async () => {
    // Plain fetch: whether a provider is offered is the page's business, and
    // a failure only means the button is not shown.
    const result = await loadOidcStatus(fetch);
    if (result.kind === 'ok') oidc = result.data;
  });

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (submitting) return;
    submitting = true;
    refusal = null;

    // Plain fetch: a refusal here is an answer to show, not a lapsed session.
    const result = await signIn(username, password, fetch);
    submitting = false;
    if (result.kind === 'error') {
      refusal = signInRefusal(result.problem, result.retryAfter);
      password = '';
      return;
    }
    password = '';
    void goto(safeNext(next), { replaceState: true });
  }
</script>

<svelte:head><title>Sign in — Tangible</title></svelte:head>

<h1>Sign in</h1>

<!-- Announced as soon as it appears, and never only in colour. -->
<div role="alert">
  {#if oidcError}
    <p class="problem">
      <strong>Not signed in.</strong>
      {oidcRefusal(oidcError, oidc?.display_name)}
    </p>
  {/if}
</div>

{#if oidc?.enabled}
  <section aria-labelledby="provider-heading" class="provider">
    <h2 id="provider-heading" class="visually-hidden">Sign in through {oidc.display_name}</h2>
    <!-- A link, not a fetch: signing in is a trip to the provider and back. -->
    <Button variant="primary" icon="external" href={oidcStartUrl(next)}
      >Sign in with {oidc.display_name}</Button
    >
  </section>
  <p class="or"><span>or with a password</span></p>
{/if}

{#if oidc?.password_sign_in === 'setup-only'}
  <p class="note before">
    Passwords are for the administrator setup made, for when {oidc.display_name ??
      'the identity provider'} is down. Everyone else signs in through it.
  </p>
{/if}

<form class="form" onsubmit={submit}>
  <Field label="Username">
    {#snippet children({ id })}
      <input
        {id}
        name="username"
        autocomplete="username"
        autocapitalize="none"
        spellcheck="false"
        bind:value={username}
        required
      />
    {/snippet}
  </Field>
  <Field label="Password">
    {#snippet children({ id })}
      <input
        {id}
        name="password"
        type="password"
        autocomplete="current-password"
        bind:value={password}
        required
      />
    {/snippet}
  </Field>

  <!-- The refusal takes room only when there is one. -->
  <div class="submit">
    <div role="alert">
      {#if refusal}
        <p class="problem"><strong>Not signed in.</strong> {refusal}</p>
      {/if}
    </div>
    <Button type="submit" variant={oidc?.enabled ? 'secondary' : 'primary'} disabled={submitting}
      >{submitting ? 'Signing in…' : 'Sign in'}</Button
    >
  </div>
</form>

<p class="note">
  Forgotten your password? An administrator can create you a new account. There is no reset by
  email.
</p>

<style>
  .or {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    margin: var(--space-5) 0 var(--space-4);
    color: var(--text-muted);
    font-size: var(--text-sm);
  }
  .or::before,
  .or::after {
    content: '';
    flex: 1;
    border-top: 1px solid var(--line);
  }
  .form {
    display: grid;
    gap: var(--space-4);
  }
  .form input {
    width: 100%;
  }
  .problem {
    margin: 0 0 var(--space-3);
    color: var(--bad);
  }
  .submit {
    display: grid;
  }
  .note {
    margin: var(--space-4) 0 0;
    font-size: var(--text-sm);
    color: var(--text-muted);
  }
  .note.before {
    margin: 0 0 var(--space-4);
  }
  .form :global(.btn),
  .provider :global(.btn) {
    width: 100%;
  }
</style>
