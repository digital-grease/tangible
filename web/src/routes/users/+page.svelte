<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { onMount } from 'svelte';
  import {
    createUser,
    loadUsers,
    MIN_PASSWORD_CHARS,
    roleLabel,
    type Problem,
    type UserView,
  } from '$lib/session';

  type View =
    | { kind: 'loading' }
    | { kind: 'ready'; users: UserView[] }
    | { kind: 'forbidden' }
    | { kind: 'error'; problem: Problem };

  let view = $state<View>({ kind: 'loading' });

  let username = $state('');
  let password = $state('');
  let role = $state('operator');
  let refusal = $state<Problem | null>(null);
  let created = $state<UserView | null>(null);
  let submitting = $state(false);

  async function refresh() {
    const result = await loadUsers();
    if (result.kind === 'ok') {
      view = { kind: 'ready', users: result.data.items };
    } else if (result.problem.code === 'FORBIDDEN') {
      view = { kind: 'forbidden' };
    } else {
      view = { kind: 'error', problem: result.problem };
    }
  }

  onMount(refresh);

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (submitting) return;
    submitting = true;
    refusal = null;
    created = null;
    const result = await createUser({ username, password, role });
    submitting = false;
    if (result.kind === 'error') {
      refusal = result.problem;
      return;
    }
    created = result.data;
    username = '';
    password = '';
    await refresh();
  }

  function when(value: string | null | undefined): string {
    return value ? new Date(value).toLocaleString() : 'Never';
  }
</script>

<svelte:head><title>Accounts — Tangible</title></svelte:head>

<h1>Accounts</h1>

<section aria-live="polite" aria-busy={view.kind === 'loading'}>
  {#if view.kind === 'loading'}
    <p>Loading accounts…</p>
  {:else if view.kind === 'forbidden'}
    <p>Only an administrator can see or create accounts.</p>
  {:else if view.kind === 'error'}
    <div role="alert">
      <h2>{view.problem.title}</h2>
      <p>{view.problem.detail}</p>
    </div>
  {:else}
    <table>
      <caption>Everyone who can sign in</caption>
      <thead>
        <tr>
          <th scope="col">Username</th>
          <th scope="col">Role</th>
          <th scope="col">Status</th>
          <th scope="col">Last signed in</th>
          <th scope="col">Created</th>
        </tr>
      </thead>
      <tbody>
        {#each view.users as user (user.id)}
          <tr>
            <th scope="row">{user.username}</th>
            <td>{roleLabel(user.role)}</td>
            <td>{user.disabled ? 'Disabled' : 'Active'}</td>
            <td>{when(user.last_signed_in_at)}</td>
            <td>{when(user.created_at)}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</section>

{#if view.kind === 'ready'}
  <section aria-labelledby="create-heading">
    <h2 id="create-heading">Create an account</h2>
    <form onsubmit={submit}>
      <p>
        <label for="username">Username</label><br />
        <input
          id="username"
          name="username"
          autocomplete="off"
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
          autocomplete="new-password"
          minlength={MIN_PASSWORD_CHARS}
          bind:value={password}
          required
          aria-describedby="password-hint"
        />
        <br />
        <small id="password-hint">
          At least {MIN_PASSWORD_CHARS} characters. Pass it on in person or through a channel you trust;
          it is not shown again.
        </small>
      </p>
      <p>
        <label for="role">Role</label><br />
        <select id="role" name="role" bind:value={role}>
          <option value="viewer">Viewer: sees the library, burns and discs; changes nothing</option>
          <option value="operator">Operator: imports, catalogs and burns</option>
          <option value="administrator">Administrator: also manages accounts and workers</option>
        </select>
      </p>

      <div role="alert">
        {#if refusal}
          <p><strong>Not created.</strong> {refusal.detail}</p>
        {/if}
      </div>
      <div aria-live="polite">
        {#if created}
          <p>Created {created.username} as {roleLabel(created.role).toLowerCase()}.</p>
        {/if}
      </div>

      <button type="submit" disabled={submitting}>
        {submitting ? 'Creating…' : 'Create account'}
      </button>
    </form>
  </section>
{/if}
