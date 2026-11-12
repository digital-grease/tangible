<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Accounts (UX.md 11; docs/plans/ui-overhaul.md, "Pages"): everyone who
  // can sign in, with their role and how they sign in, and a form that
  // creates an account. Administrators only; anybody else is told so.
  import Badge from '$lib/components/ui/Badge.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import EmptyState from '$lib/components/ui/EmptyState.svelte';
  import Field from '$lib/components/ui/Field.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import Panel from '$lib/components/ui/Panel.svelte';
  import Select from '$lib/components/ui/Select.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import Table from '$lib/components/ui/Table.svelte';
  import { when } from '$lib/overview';
  import {
    apiFetch,
    createUser,
    loadUsers,
    MIN_PASSWORD_CHARS,
    roleLabel,
    type Problem,
    type UserView,
  } from '$lib/session';
  import { onMount } from 'svelte';

  type View =
    | { kind: 'loading' }
    | { kind: 'ready'; users: UserView[]; nextCursor: string | null }
    | { kind: 'forbidden' }
    | { kind: 'error'; problem: Problem };

  let view = $state<View>({ kind: 'loading' });

  let username = $state('');
  let password = $state('');
  let role = $state('operator');
  let refusal = $state<Problem | null>(null);
  let created = $state<UserView | null>(null);
  let submitting = $state(false);
  let loadingMore = $state(false);
  let moreProblem = $state<Problem | null>(null);

  const ROLES = [
    { value: 'viewer', label: 'Viewer: sees the library, burns and discs; changes nothing' },
    { value: 'operator', label: 'Operator: imports, catalogs and burns' },
    { value: 'administrator', label: 'Administrator: also manages accounts and workers' },
  ] as const;

  async function refresh() {
    const result = await loadUsers();
    moreProblem = null;
    if (result.kind === 'ok') {
      view = {
        kind: 'ready',
        users: result.data.items,
        nextCursor: result.data.next_cursor ?? null,
      };
    } else if (result.problem.code === 'FORBIDDEN') {
      view = { kind: 'forbidden' };
    } else {
      view = { kind: 'error', problem: result.problem };
    }
  }

  onMount(refresh);

  async function loadMore() {
    if (view.kind !== 'ready' || !view.nextCursor || loadingMore) return;
    loadingMore = true;
    moreProblem = null;
    const next = await loadUsers(apiFetch, view.nextCursor);
    loadingMore = false;
    if (next.kind === 'error') {
      // The accounts already shown stay; only the next page is missing.
      moreProblem = next.problem;
      return;
    }
    if (view.kind === 'ready') {
      view = {
        kind: 'ready',
        users: [...view.users, ...next.data.items],
        nextCursor: next.data.next_cursor ?? null,
      };
    }
  }

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

  /** The full date and time, for a time's tooltip. */
  function full(value: string): string {
    return new Date(value).toLocaleString();
  }
</script>

<svelte:head><title>Accounts — Tangible</title></svelte:head>

<PageHeader title="Accounts" description="Everyone who can sign in, and what each may do." />

<div class="columns" class:single={view.kind !== 'ready'}>
  <div aria-live="polite" aria-busy={view.kind === 'loading'}>
    {#if view.kind === 'loading'}
      <p class="quiet">Loading accounts…</p>
    {:else if view.kind === 'forbidden'}
      <EmptyState title="Administrators only" icon="lock">
        <p>Only an administrator can see or create accounts.</p>
      </EmptyState>
    {:else if view.kind === 'error'}
      <div class="problem-panel" role="alert">
        <h2>{view.problem.title}</h2>
        <p>{view.problem.detail}</p>
      </div>
    {:else}
      <Panel title="Accounts" aside={`${view.users.length}${view.nextCursor ? '+' : ''}`} flush>
        <Table caption="Everyone who can sign in">
          <thead>
            <tr>
              <th scope="col">Username</th>
              <th scope="col">Role</th>
              <th scope="col">Signs in with</th>
              <th scope="col">Status</th>
              <th scope="col">Last signed in</th>
              <th scope="col">Created</th>
            </tr>
          </thead>
          <tbody>
            {#each view.users as user (user.id)}
              <tr>
                <th scope="row">{user.username}</th>
                <td>
                  <Badge strong={user.role === 'administrator'}>{roleLabel(user.role)}</Badge>
                </td>
                <td>{user.signs_in_with === 'provider' ? 'Identity provider' : 'Password'}</td>
                <td>
                  <StatusMark
                    tone={user.disabled ? 'idle' : 'ok'}
                    label={user.disabled ? 'Disabled' : 'Active'}
                  />
                </td>
                <td>
                  {#if user.last_signed_in_at}
                    <time datetime={user.last_signed_in_at} title={full(user.last_signed_in_at)}
                      >{when(user.last_signed_in_at)}</time
                    >
                  {:else}
                    <span class="quiet">Never</span>
                  {/if}
                </td>
                <td>
                  <time datetime={user.created_at} title={full(user.created_at)}
                    >{when(user.created_at)}</time
                  >
                </td>
              </tr>
            {/each}
          </tbody>
        </Table>
      </Panel>
      {#if view.nextCursor || moreProblem}
        <div class="more">
          {#if view.nextCursor}
            <Button onclick={loadMore} disabled={loadingMore}>
              {loadingMore ? 'Loading…' : 'Show more accounts'}
            </Button>
          {/if}
          {#if moreProblem}
            <p role="alert" class="problem">
              Could not load more accounts: {moreProblem.detail}
            </p>
          {/if}
        </div>
      {/if}
    {/if}
  </div>

  {#if view.kind === 'ready'}
    <Panel title="Create an account">
      <form class="create" onsubmit={submit}>
        <Field label="Username" required>
          {#snippet children({ id })}
            <input
              {id}
              name="username"
              autocomplete="off"
              autocapitalize="none"
              spellcheck="false"
              bind:value={username}
              required
            />
          {/snippet}
        </Field>
        <Field
          label="Password"
          hint={`At least ${MIN_PASSWORD_CHARS} characters. Pass it on in person or through a channel you trust; it is not shown again.`}
          required
        >
          {#snippet children({ id, describedBy })}
            <input
              {id}
              name="password"
              type="password"
              autocomplete="new-password"
              minlength={MIN_PASSWORD_CHARS}
              bind:value={password}
              aria-describedby={describedBy}
              required
            />
          {/snippet}
        </Field>
        <Select label="Role" options={ROLES} bind:value={role} />

        <div class="submit">
          <div role="alert">
            {#if refusal}
              <p class="problem"><strong>Not created.</strong> {refusal.detail}</p>
            {/if}
          </div>
          <div aria-live="polite">
            {#if created}
              <p class="done">
                <StatusMark
                  tone="ok"
                  label={`Created ${created.username} as ${roleLabel(created.role).toLowerCase()}.`}
                />
              </p>
            {/if}
          </div>
          <div>
            <Button type="submit" variant="primary" disabled={submitting}>
              {submitting ? 'Creating…' : 'Create account'}
            </Button>
          </div>
        </div>
      </form>
    </Panel>
  {/if}
</div>

<style>
  .columns {
    display: grid;
    gap: var(--space-5);
    align-items: start;
  }
  @media (min-width: 1100px) {
    .columns:not(.single) {
      grid-template-columns: minmax(0, 2fr) minmax(20rem, 1fr);
    }
  }
  .quiet {
    color: var(--text-muted);
  }
  .more {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-3);
    align-items: center;
    margin-top: var(--space-4);
  }
  .problem {
    margin: 0;
    color: var(--bad);
  }
  .problem-panel {
    padding: var(--space-3) var(--space-4);
    background: var(--surface);
    border: 1px solid var(--line);
    border-left: 4px solid var(--bad);
    border-radius: var(--radius);
  }
  .problem-panel h2 {
    margin: 0 0 var(--space-1);
    font-size: var(--text-md);
  }
  .problem-panel p {
    margin: 0;
  }
  .create {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    gap: var(--space-4);
  }
  .create input,
  .create :global(select) {
    width: 100%;
  }
  /* The messages take room only when they say something. */
  .submit .problem,
  .done {
    margin: 0 0 var(--space-3);
  }
  .done :global(.mark) {
    font-weight: var(--weight-medium);
  }
</style>
