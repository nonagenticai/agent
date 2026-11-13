<script lang="ts">
	// J13 Base & refinements — the job's DRAFT WORKSPACE (D54, owner 2026-10-19; overrides 05 K7 "live on write").
	// The template base (layers pinned by the live release, read-only) under this tenant's refinements (override /
	// extension / tombstone rows + preamble/actor/stance overrides, D8, 05 §2.5), and every unpublished change across
	// all design pages (draft vs live, discard) with base conflicts inline. Path: Draft → Test → Publish (J14).
	import { onMount } from 'svelte';
	import { api, app } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite, writeHint } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { jobPath } from '$lib/scope';
	import type { JobSummary } from '$lib/api/types/core';
	import type {
		DraftChange,
		OverlayItem,
		ReleaseConflict,
		ReleaseDetail,
		ReleaseStatus,
	} from '$lib/api/types/release';
	import type { SideObject, SideRow } from '$lib/release/refinements';
	import { baseLayers, changesByPage, openConflicts, workspacePath } from '$lib/release/J13-workspace';
	import Callout from '$lib/ui/Callout.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Tabs from '$lib/ui/Tabs.svelte';
	import BaseConflicts from './BaseConflicts.svelte';
	import ContextSpaceEditor from './ContextSpaceEditor.svelte';
	import InstructionEditor from './InstructionEditor.svelte';
	import DraftChanges from './DraftChanges.svelte';
	import OverlayTable from './OverlayTable.svelte';
	import SettingsTable from './SettingsTable.svelte';
	import JobLanguages from './JobLanguages.svelte';
	import WorkspaceModel from './WorkspaceModel.svelte';

	// `embedded`: shown as a tab of Releases, which already lists the draft changes and the conflicts.
	let { embedded = false }: { embedded?: boolean } = $props();

	const OBJECTS: [SideObject, string][] = [
		['activity', 'Activities'],
		['context_space', 'Context spaces'],
		['ontology_attribute', 'Attributes'],
		['gate_option', 'Gate options'],
		['knowledge_store', 'Knowledge stores'],
	];

	// D132.4: `#job-settings` (linked from the job overview and the selection editor) opens the Job settings tab.
	let tab = $state<string>(
		typeof location !== 'undefined' && location.hash === '#job-settings' ? 'settings' : 'activity',
	);
	const job = new Resource<JobSummary>(() => api().job(app.job));
	const status = new Resource<ReleaseStatus>(
		() => api().releaseStatus(),
		() => false,
	);
	const live = new Resource<ReleaseDetail | null>(
		async () => {
			const j = await api().job(app.job);
			return j.current_release ? api().releaseDetail(j.current_release.id) : null;
		},
		() => false,
	);
	const draft = new Resource<DraftChange[]>(
		() => api().draftDiff(),
		() => false,
	);
	const conflicts = new Resource<ReleaseConflict[]>(
		async () => (await api().releaseConflicts('open')).items,
		() => false,
	);
	/** Tenant rows per object (tab counts) and the settings overrides. */
	const counts = new Resource<Record<string, number>>(
		async () => {
			const [layers, settings] = await Promise.all([
				Promise.all(OBJECTS.map(([o]) => api().refinementLayers(o))),
				api().jobSettingsView(),
			]);
			const out: Record<string, number> = Object.fromEntries(layers.map((l) => [l.object, l.tenant.length]));
			out.settings = Object.keys(settings.override).length;
			return out;
		},
		() => false,
	);
	/** Bumped after every write so the tables re-read (they mark draft rows from `changes`). */
	let version = $state(0);
	function refreshAll() {
		void job.refresh();
		void status.refresh();
		void draft.refresh();
		void conflicts.refresh();
		void counts.refresh();
	}
	function changed() {
		version++;
		refreshAll();
	}
	onMount(() => {
		refreshAll();
		void live.refresh();
	});

	const ok = <T,>(r: Resource<T>): T | null => (r.value.state === 'ok' ? r.value.data : null);
	const writer = $derived(canWrite(session, 'job_designer'));
	const changes = $derived(ok(draft));
	const open = $derived(openConflicts(ok(conflicts) ?? []));
	const n = (k: string) => ok(counts)?.[k];
	const totalRefinements = $derived.by(() => {
		const c = ok(counts);
		return c ? Object.values(c).reduce((a, b) => a + b, 0) : null;
	});
	const liveRelease = $derived(ok(job)?.current_release?.id ?? null);
	const layers = $derived.by(() => {
		const d = ok(live);
		return d ? baseLayers(d.provenance.pins) : null;
	});
	const steps = $derived(
		workspacePath({
			job: app.job,
			drafts: changes?.length ?? 0,
			pages: changesByPage(changes ?? []).length,
			liveRelease,
			approvedWaiting: ok(status)?.approved_waiting ?? null,
			newerPins: ok(status)?.newer_pins.length ?? 0,
		}),
	);
	const conflictsHref = $derived(`${jobPath(app.job, '/release')}#conflicts`);

	// Context-space editor: the row being edited.
	let editing = $state<SideRow | null>(null);
	let editEntities = $state<string[]>([]);
	let editorOpen = $state(false);
	let editorDirty = $state(false);
	function editSpace(row: SideRow, entities: string[]) {
		editing = row;
		editEntities = entities;
		editorDirty = false;
		editorOpen = true;
	}
	function spaceSaved() {
		editorOpen = false;
		changed();
	}
</script>

{#if !writer}
	<Callout kind="info" title="Read only"
		>You can see the base, the refinements and the draft. Changing or discarding them: {writeHint(
			'job_designer',
		).toLowerCase()}.</Callout
	>
{/if}

<WorkspaceModel
	{layers}
	{liveRelease}
	noRelease={!!ok(job) && !liveRelease}
	refinements={totalRefinements}
	drafts={changes?.length ?? null}
	draftRev={ok(job)?.draft_rev ?? null}
	{steps}
/>

{#if !embedded}
	<DraftChanges {changes} {liveRelease} onchange={changed} />

	<BaseConflicts conflicts={open} href={conflictsHref} onchange={changed} />
{/if}

<section class="br" aria-labelledby="br-h">
	<h2 id="br-h">Base &amp; your refinements</h2>
	<p class="muted br-lead">
		Each row: what the template says, and what you made of it. <b>Hide</b> drops a base row for this tenant,
		<b>Revert</b> brings the base back, <b>Rebase</b> keeps yours on a changed base. Rows marked
		<span class="chip accent">Draft</span> are not live yet.
	</p>
	<Tabs
		items={[
			...OBJECTS.map(([o, label]): [string, string, number?] => [o, label, n(o)]),
			['settings', 'Job settings', n('settings')],
			['instructions', 'Instructions'],
		]}
		bind:active={tab}
		label="Refinable objects"
	>
		{#key `${tab}:${version}`}
			{#if tab === 'settings'}
				<JobLanguages onchange={changed} />
				<SettingsTable drafts={changes ?? []} onchange={changed} />
			{:else if tab === 'instructions'}
				<InstructionEditor onchange={changed} />
			{:else}
				<OverlayTable
					object={tab as SideObject}
					conflicts={open}
					drafts={changes ?? []}
					onchange={changed}
					edit={tab === 'context_space' ? editSpace : undefined}
					{conflictsHref}
				/>
			{/if}
		{/key}
	</Tabs>
</section>

<Drawer bind:open={editorOpen} title={editing ? `Context space ${editing.key}` : ''} wide dirty={editorDirty}>
	{#if editing && editorOpen}
		{#key editing.key}
			<ContextSpaceEditor
				base={editing.base}
				tenant={(editing.tenant?.item ?? null) as OverlayItem | null}
				entities={editEntities}
				bind:dirty={editorDirty}
				onsaved={spaceSaved}
				oncancel={() => (editorOpen = false)}
			/>
		{/key}
	{/if}
</Drawer>

<style>
	.br h2 {
		font-size: 15px;
		margin: 8px 0 0;
	}
	.br-lead {
		margin: 2px 0 10px;
		font-size: 13px;
		max-width: 90ch;
	}
</style>
