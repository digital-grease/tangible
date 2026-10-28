<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { goto } from '$app/navigation';
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

<h1>Sign in to Tangible</h1>

<!-- Announced as soon as it appears, and never only in colour. -->
<div role="alert">
  {#if oidcError}
    <p><strong>Not signed in.</strong> {oidcRefusal(oidcError, oidc?.display_name)}</p>
  {/if}
</div>

{#if oidc?.enabled}
  <section aria-labelledby="provider-heading">
    <h2 id="provider-heading" class="visually-hidden">Sign in through {oidc.display_name}</h2>
    <p>
      <!-- A link, not a fetch: signing in is a trip to the provider and back. -->
      <a class="button" href={oidcStartUrl(next)}>Sign in with {oidc.display_name}</a>
    </p>
  </section>
  <p class="or">or with a password</p>
{/if}

{#if oidc?.password_sign_in === 'setup-only'}
  <p>
    <small
      >Passwords are for the administrator setup made, for when {oidc.display_name ??
        'the identity provider'} is down. Everyone else signs in through it.</small
    >
  </p>
{/if}

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
    />
  </p>
  <p>
    <label for="password">Password</label><br />
    <input
      id="password"
      name="password"
      type="password"
      autocomplete="current-password"
      bind:value={password}
      required
    />
  </p>

  <div role="alert">
    {#if refusal}
      <p><strong>Not signed in.</strong> {refusal}</p>
    {/if}
  </div>

  <button type="submit" disabled={submitting}>{submitting ? 'Signing in…' : 'Sign in'}</button>
</form>

<p>
  <small>
    Forgotten your password? An administrator can create you a new account. There is no reset by
    email.
  </small>
</p>

<style>
  .button {
    display: inline-block;
    padding: 0.5rem 1rem;
    border: 1px solid currentColor;
    text-decoration: none;
  }
  .button:focus-visible {
    outline: 3px solid currentColor;
    outline-offset: 2px;
  }
  .or {
    margin-top: 1.5rem;
  }
  .visually-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
  }
</style>
