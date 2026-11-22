<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Accounts (UX.md 11; docs/plans/ui-overhaul.md, "Pages"): everyone who
  // can sign in, with their role and how they sign in, and a form that
  // creates an account. Administrators only; anybody else is told so.
  //
  // Where only the account setup made may use a password
  // (TANGIBLE_PASSWORD_SIGN_IN=setup-only), the form is not offered, since
  // an account made with it could never sign in: the server refuses it, and
  // the page says how accounts are made there instead.
  //
  // An account is disabled, which signs it out and keeps it from signing in,
  // after a question under the list; enabled again in one click. Not one's
  // own: the server refuses that, so it is not offered.
  import Badge from '$lib/components/ui/Badge.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import Callout from '$lib/components/ui/Callout.svelte';
  import Confirm from '$lib/components/ui/Confirm.svelte';
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
    loadOidcStatus,
    loadUsers,
    MIN_PASSWORD_CHARS,
    roleLabel,
    sessionUsername,
    setUserDisabled,
    type OidcStatus,
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
  /** How people sign in here; unknown until it loads, or if it cannot. */
  let signIn = $state<OidcStatus | null>(null);

  /** The account a question is being asked about, before it is disabled. */
  let confirming = $state<UserView | null>(null);
  let changing = $state(false);
  let changed = $state<string | null>(null);
  let changeProblem = $state<Problem | null>(null);
  const me = sessionUsername();

  /** Whether only the account setup made may use a password. */
  const passwordsOff = $derived(signIn?.password_sign_in === 'setup-only');
  const provider = $derived(signIn?.display_name || 'the identity provider');

  const ROLES = [
    {
      value: 'viewer',
      label: 'Viewer: sees the library, burns and discs, and asks for items; changes nothing',
    },
    { value: 'operator', label: 'Operator: imports, catalogs, burns and approves requests' },
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

  onMount(() => {
    void refresh();
    void loadOidcStatus().then((result) => {
      if (result.kind === 'ok') signIn = result.data;
    });
  });

  async function setDisabled(user: UserView, disabled: boolean) {
    if (changing) return;
    changing = true;
    changed = null;
    changeProblem = null;
    const result = await setUserDisabled(user.id, disabled);
    changing = false;
    if (result.kind === 'error') {
      changeProblem = result.problem;
      return;
    }
    confirming = null;
    changed = `${disabled ? 'Disabled' : 'Enabled'} ${user.username}.`;
    await refresh();
  }

  /** How an account signs in, in words. */
  function signsInWith(user: UserView): string {
    switch (user.signs_in_with) {
      case 'provider':
        return 'Identity provider';
      case 'none':
        return 'Cannot sign in';
      default:
        return 'Password';
    }
  }

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
              <th scope="col"><span class="visually-hidden">Actions</span></th>
            </tr>
          </thead>
          <tbody>
            {#each view.users as user (user.id)}
              <tr>
                <th scope="row">{user.username}</th>
                <td>
                  <Badge strong={user.role === 'administrator'}>{roleLabel(user.role)}</Badge>
                </td>
                <td>
                  {#if user.signs_in_with === 'none'}
                    <StatusMark tone="warn" label={signsInWith(user)} />
                  {:else}
                    {signsInWith(user)}
                  {/if}
                </td>
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
                <td class="actions">
                  {#if user.username === me}
                    <span class="quiet">You</span>
                  {:else if user.disabled}
                    <Button
                      size="sm"
                      onclick={() => setDisabled(user, false)}
                      disabled={changing}
                      label={`Enable ${user.username}`}>Enable</Button
                    >
                  {:else}
                    <Button
                      size="sm"
                      onclick={() => {
                        confirming = user;
                        changed = null;
                        changeProblem = null;
                      }}
                      disabled={changing}
                      label={`Disable ${user.username}…`}>Disable…</Button
                    >
                  {/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </Table>
        {#if confirming}
          {@const user = confirming}
          <div class="question">
            <Confirm title={`Disable ${user.username}?`}>
              <p>
                They are signed out now, and cannot sign in, with a password or through the identity
                provider, until the account is enabled again. What they did stays in the audit log
                and on what they asked for and burned.
              </p>
              {#snippet actions()}
                <Button variant="danger" onclick={() => setDisabled(user, true)} disabled={changing}
                  >{changing ? 'Disabling…' : `Disable ${user.username}`}</Button
                >
                <Button onclick={() => (confirming = null)}>Keep it</Button>
              {/snippet}
            </Confirm>
          </div>
        {/if}
        <div aria-live="polite">
          {#if changed}<p class="changed"><StatusMark tone="ok" label={changed} /></p>{/if}
        </div>
        {#if changeProblem}
          <p role="alert" class="problem changed">{changeProblem.detail}</p>
        {/if}
      </Panel>
      {#if view.users.some((user) => user.signs_in_with === 'none')}
        <p class="quiet note">
          An account that cannot sign in was made with a password, but only the account setup made
          may use one here (<code>TANGIBLE_PASSWORD_SIGN_IN=setup-only</code>).
        </p>
      {/if}
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

  {#if view.kind === 'ready' && passwordsOff}
    <Panel title="New accounts">
      <Callout tone="info">
        <p>
          Only the account setup made signs in with a password here (<code
            >TANGIBLE_PASSWORD_SIGN_IN=setup-only</code
          >), so an account made with one could never sign in.
        </p>
        <p>
          Everyone else signs in through {provider}. Their account is made here the first time they
          do, with the role their group gives them.
        </p>
        <p>
          To make password accounts, set <code>TANGIBLE_PASSWORD_SIGN_IN=all</code> and restart the server.
        </p>
      </Callout>
    </Panel>
  {:else if view.kind === 'ready'}
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
  .actions {
    text-align: right;
    white-space: nowrap;
  }
  .question,
  .changed {
    margin: var(--space-3) var(--space-4);
  }
  .note {
    margin: var(--space-3) 0 0;
    font-size: var(--text-sm);
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
