<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { ENROLLMENT_MINUTES, issueEnrollment, type IssuedEnrollment } from '$lib/workers';
  import type { Problem } from '$lib/session';

  let minutes = $state<number>(ENROLLMENT_MINUTES.default);
  let issued = $state<IssuedEnrollment | null>(null);
  let problem = $state<Problem | null>(null);
  let submitting = $state(false);

  async function issue(event: SubmitEvent) {
    event.preventDefault();
    if (submitting) return;
    submitting = true;
    problem = null;
    issued = null;
    const result = await issueEnrollment(minutes);
    submitting = false;
    if (result.kind === 'error') {
      problem = result.problem;
      return;
    }
    issued = result.data;
  }
</script>

<svelte:head><title>Workers — Tangible</title></svelte:head>

<h1>Workers</h1>

<section aria-labelledby="enroll-heading">
  <h2 id="enroll-heading">Enroll a burn worker</h2>
  <p>
    A new worker needs a one-use enrollment token on its first start. It exchanges the token for a
    credential of its own, kept on the worker's volume, and the token is spent.
  </p>

  <form onsubmit={issue}>
    <p>
      <label for="minutes">Valid for (minutes)</label><br />
      <input
        id="minutes"
        name="minutes"
        type="number"
        min={ENROLLMENT_MINUTES.min}
        max={ENROLLMENT_MINUTES.max}
        bind:value={minutes}
        required
      />
      <br />
      <small>Long enough to paste it into the worker's settings and start it.</small>
    </p>

    <div role="alert">
      {#if problem}
        <p>
          <strong>Not issued.</strong>
          {problem.code === 'FORBIDDEN'
            ? 'Only an administrator can enroll workers.'
            : problem.detail}
        </p>
      {/if}
    </div>

    <button type="submit" disabled={submitting}>
      {submitting ? 'Issuing…' : 'Issue an enrollment token'}
    </button>
  </form>

  {#if issued}
    <div aria-live="polite" class="issued">
      <h3>Enrollment token</h3>
      <p>
        Shown this once. The server keeps only its hash, so if it is lost, issue another. It lapses
        at {new Date(issued.expires_at).toLocaleString()}.
      </p>
      <p>
        <label for="token">Token</label><br />
        <input id="token" readonly value={issued.enrollment_token} size="80" />
      </p>
      <p>
        Set it as <code>TANGIBLE_ENROLLMENT_TOKEN</code> in the worker's environment, start the worker,
        then remove the setting again.
      </p>
    </div>
  {/if}
</section>

<style>
  .issued {
    border: 2px solid currentColor;
    padding: 0 1rem;
    margin-top: 1rem;
  }
  #token {
    font-family: ui-monospace, monospace;
    max-width: 100%;
  }
</style>
