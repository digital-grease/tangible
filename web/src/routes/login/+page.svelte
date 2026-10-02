<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { goto } from '$app/navigation';
  import { page } from '$app/state';
  import { safeNext, signIn, signInRefusal } from '$lib/session';

  let username = $state('');
  let password = $state('');
  let refusal = $state<string | null>(null);
  let submitting = $state(false);

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
    void goto(safeNext(page.url.searchParams.get('next')), { replaceState: true });
  }
</script>

<svelte:head><title>Sign in — Tangible</title></svelte:head>

<h1>Sign in to Tangible</h1>

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

  <!-- Announced as soon as it appears, and never only in colour. -->
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
