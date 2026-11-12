<!-- SPDX-FileCopyrightText: 2026 digitalgrease -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts" module>
  export type Case =
    | 'field'
    | 'select'
    | 'checkbox'
    | 'tabs'
    | 'menu'
    | 'panel'
    | 'header'
    | 'empty'
    | 'disc'
    | 'callout'
    | 'confirm'
    | 'all';
</script>

<script lang="ts">
  // The UI components put together as pages put them together, for the
  // component tests (and, rendered in a scratch route, for looking them
  // over). Imported by tests only, so it is never bundled.
  import Badge from '$lib/components/ui/Badge.svelte';
  import Button from '$lib/components/ui/Button.svelte';
  import Callout from '$lib/components/ui/Callout.svelte';
  import Checkbox from '$lib/components/ui/Checkbox.svelte';
  import Confirm from '$lib/components/ui/Confirm.svelte';
  import Disc from '$lib/components/ui/Disc.svelte';
  import EmptyState from '$lib/components/ui/EmptyState.svelte';
  import Field from '$lib/components/ui/Field.svelte';
  import Menu from '$lib/components/ui/Menu.svelte';
  import PageHeader from '$lib/components/ui/PageHeader.svelte';
  import Panel from '$lib/components/ui/Panel.svelte';
  import Progress from '$lib/components/ui/Progress.svelte';
  import Select from '$lib/components/ui/Select.svelte';
  import StatusMark from '$lib/components/ui/StatusMark.svelte';
  import Table from '$lib/components/ui/Table.svelte';
  import Tabs from '$lib/components/ui/Tabs.svelte';

  let {
    show = 'all',
    fieldError = null,
    onselect,
  }: {
    show?: Case;
    fieldError?: string | null;
    onselect?: (what: string) => void;
  } = $props();

  /** The confirmation's state, for its test. */
  let confirming = $state(false);
  let understood = $state(false);

  let name = $state('');
  let format = $state('');
  let deleted = $state(false);
  let tab = $state('summary');

  const formats = [
    { value: '', label: 'Any' },
    { value: 'iso', label: 'ISO' },
    { value: 'cue_bin', label: 'CUE/BIN' },
  ];
  const tabs = [
    { id: 'summary', label: 'Summary' },
    { id: 'components', label: 'Components', count: 9 },
    { id: 'topology', label: 'Topology' },
  ];
  const audioCd = {
    media_family: 'cd',
    sector_count: 318_000,
    tracks: Array.from({ length: 12 }, () => ({
      session: 1,
      sector_count: 26_500,
      is_audio: true,
    })),
  };
  const mixedMode = {
    media_family: 'cd',
    sector_count: 260_000,
    tracks: [
      { session: 1, sector_count: 110_000, is_audio: false },
      ...Array.from({ length: 7 }, () => ({ session: 1, sector_count: 21_000, is_audio: true })),
    ],
  };
  const dvd = { media_family: 'dvd', sector_count: 2_050_000, tracks: [] };
  const iso = { media_family: 'cd', sector_count: 300_000, tracks: [] };
  const items = [
    { label: 'Import', href: '/imports', icon: 'import' as const },
    { label: 'Rip a disc', href: '/rips', icon: 'rip' as const },
    { label: 'Create burn job', icon: 'burn' as const, onselect: () => onselect?.('burn') },
  ];
</script>

{#if show === 'header' || show === 'all'}
  <PageHeader
    title="homebrew-comp-vol2.cue"
    crumbs={[{ label: 'Library', href: '/library' }, { label: 'homebrew-comp-vol2.cue' }]}
    description="Imported from a watched folder."
  >
    {#snippet actions()}
      <Button variant="primary" icon="burn">Create burn job</Button>
      <Button icon="more" label="More actions" />
    {/snippet}
    {#snippet badges()}
      <Badge icon="lock" strong>Original, immutable</Badge>
      <StatusMark tone="warn" label="Structurally valid, with findings" />
    {/snippet}
  </PageHeader>
{/if}

{#if show === 'field' || show === 'all'}
  <Field
    label="Name"
    hint="Part of the file name; case does not matter."
    error={fieldError}
    required
  >
    {#snippet children({ id, describedBy, invalid })}
      <input
        {id}
        bind:value={name}
        required
        aria-describedby={describedBy}
        aria-invalid={invalid || undefined}
      />
    {/snippet}
  </Field>
{/if}

{#if show === 'select' || show === 'all'}
  <Select label="Format" options={formats} bind:value={format} />
  <p data-testid="format">{format}</p>
{/if}

{#if show === 'checkbox' || show === 'all'}
  <Checkbox label="Only deleted artifacts" hint="Kept until collected." bind:checked={deleted} />
  <p data-testid="deleted">{deleted ? 'yes' : 'no'}</p>
{/if}

{#if show === 'menu' || show === 'all'}
  <Menu label="New" icon="plus" variant="primary" {items} />
{/if}

{#if show === 'tabs' || show === 'all'}
  <Tabs label="Artifact" {tabs} bind:selected={tab}>
    {#snippet panel(id)}
      <p>Showing {id}.</p>
    {/snippet}
  </Tabs>
{/if}

{#if show === 'panel' || show === 'all'}
  <Panel title="Latest burns" aside="3">
    {#snippet actions()}<Button size="sm" variant="quiet" href="/burns">All burns</Button>{/snippet}
    <Table caption="Latest burns">
      <thead>
        <tr
          ><th scope="col">Disc</th><th scope="col" class="num">Size</th><th scope="col">Outcome</th
          ></tr
        >
      </thead>
      <tbody>
        <tr>
          <th scope="row">Debian 13.0 netinst, disc 1</th>
          <td class="num">754 MiB</td>
          <td><StatusMark tone="ok" label="Verified" pill /></td>
        </tr>
        <tr>
          <th scope="row">Family photos 2009, disc 1</th>
          <td class="num">4.2 GiB</td>
          <td><StatusMark tone="bad" label="Verification failed" pill /></td>
        </tr>
      </tbody>
    </Table>
    <Progress value={0.43} label="Writing Debian 13.1 netinst" tone="laser" />
    <Progress value={null} label="Waiting for the worker" />
  </Panel>
{/if}

{#if show === 'empty' || show === 'all'}
  <EmptyState title="No artifacts yet" icon="library">
    <p>Import an authorized disc image to create your first artifact.</p>
    {#snippet action()}<Button variant="primary" href="/imports" icon="import">Import one</Button
      >{/snippet}
  </EmptyState>
{/if}

{#if show === 'disc' || show === 'all'}
  <div class="discs" data-testid="discs">
    <Disc shape={audioCd} size={120} />
    <Disc shape={mixedMode} size={120} />
    <Disc shape={dvd} size={120} />
    <Disc mode="burn" shape={iso} capacity={359_844} progress={0.43} size={120} />
    <Disc mode="rip" family="dvd" progress={0.5} size={120} />
    <Disc mode="empty" size={120} />
    <Disc shape={mixedMode} size={24} />
    <Disc shape={dvd} size={24} />
  </div>
{/if}

{#if show === 'callout' || show === 'all'}
  <Callout tone="warn" title="A burn is waiting for a disc" marked>
    <p>Insert a blank disc in the drive it names.</p>
  </Callout>
  <Callout role="note"><p>Nothing is written until a worker claims it.</p></Callout>
{/if}

{#if show === 'confirm' || show === 'all'}
  <Button variant="danger" onclick={() => (confirming = true)}>Erase the disc in sr0…</Button>
  {#if confirming}
    <Confirm
      title="Erase the disc in sr0"
      onsubmit={(event) => {
        event.preventDefault();
        onselect?.('erase');
      }}
    >
      <p><strong>This destroys everything on the disc in that drive.</strong></p>
      <Checkbox
        label="I understand everything on the disc will be destroyed"
        bind:checked={understood}
      />
      {#snippet actions()}
        <Button type="submit" variant="danger" disabled={!understood}>Erase the disc in sr0</Button>
        <Button onclick={() => (confirming = false)}>Keep the disc</Button>
      {/snippet}
    </Confirm>
  {/if}
{/if}

{#if show === 'all'}
  <p>
    <StatusMark tone="ok" label="Structurally valid" />
    <StatusMark tone="warn" label="Waiting for a disc" />
    <StatusMark tone="bad" label="Damaged" />
    <StatusMark tone="busy" label="Writing" />
    <StatusMark tone="idle" label="Not checked yet" />
  </p>
  <p>
    <Button variant="primary">Primary</Button>
    <Button>Secondary</Button>
    <Button variant="quiet">Quiet</Button>
    <Button variant="danger">Delete this artifact</Button>
    <Button disabled>Disabled</Button>
  </p>
{/if}

<style>
  .discs {
    display: flex;
    flex-wrap: wrap;
    gap: 1rem;
    align-items: center;
  }
</style>
