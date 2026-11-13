<script lang="ts">
	// P21 Industries — the super_admin's industries and departments (plan 2026-10-30-super-admin-platform-fixes T3/T4),
	// with no tenant. Create, rename, retire (refused while a tenant job selects it) and restore an industry or a
	// department; attach the knowledge each layer gives every job built on it: base → industry → department.
	// An industry or department reaches tenants once its layer is published (its first tag, e.g. on attaching
	// knowledge); a retired one leaves the tenant choices, what pins it keeps working.
	import { onMount } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { superAdminAttrs } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import type { CuratorPack } from '$lib/api/types/ontology';
	import type {
		DepartmentArchetype,
		IndustryJobUse,
		PlatformDepartment,
		PlatformIndustries,
		PlatformIndustry,
	} from '$lib/api/types/platform';
	import PageHead from '$lib/ui/PageHead.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import LayerKnowledge from './LayerKnowledge.svelte';

	const data = new Resource<PlatformIndustries>(() => api().platformIndustries());
	let packs = $state<CuratorPack[]>([]);
	let packsError = $state('');
	let archetypes = $state<DepartmentArchetype[]>([]);
	async function loadPacks() {
		try {
			packs = await api().curatorPacks();
			packsError = '';
		} catch (e) {
			packsError = (e as Error).message;
		}
	}
	onMount(() => {
		void data.refresh();
		void loadPacks();
		api()
			.departmentArchetypes()
			.then((a) => (archetypes = a))
			.catch(() => (archetypes = []));
	});
	const reload = () => data.refresh();
	const sa = $derived(superAdminAttrs(session));

	const all = $derived(data.value.state === 'ok' ? data.value.data : null);
	let picked = $state('');
	const current = $derived(all?.items.find((i) => i.industry === picked) ?? null);

	const KEY_RE = /^[a-z][a-z0-9_]{0,63}$/;
	const keyOf = (label: string) =>
		label
			.toLowerCase()
			.normalize('NFKD')
			.replace(/[^a-z0-9]+/g, '_')
			.replace(/^[^a-z]+/, '')
			.replace(/_+$/, '')
			.slice(0, 64);

	function status(x: { tag: string | null; retired_at: string | null }): {
		text: string;
		variant: 'ok' | 'warn' | 'dead';
	} {
		if (x.retired_at) return { text: 'Retired', variant: 'dead' };
		if (x.tag) return { text: 'Offered to tenants', variant: 'ok' };
		return { text: 'Not published yet', variant: 'warn' };
	}
	const usage = (jobs: IndustryJobUse[]) =>
		jobs.length === 0 ? 'No jobs' : jobs.length === 1 ? '1 job' : `${jobs.length} jobs`;

	// ---- create an industry
	let newLabel = $state('');
	let newKey = $state('');
	let keyTouched = $state(false);
	const shownKey = $derived(keyTouched ? newKey : keyOf(newLabel));
	async function createIndustry(e: SubmitEvent) {
		e.preventDefault();
		const label = newLabel.trim();
		const industry = shownKey;
		if (!label || !KEY_RE.test(industry)) return;
		const ok = await guard(
			() => api().createIndustry({ industry, label }),
			`Industry ${label} created.`,
			'Not created',
		);
		if (ok) {
			newLabel = '';
			newKey = '';
			keyTouched = false;
			await reload();
			picked = industry;
		}
	}

	// ---- the picked industry
	let rename = $state('');
	$effect(() => {
		rename = current?.label ?? '';
	});
	async function renameIndustry(i: PlatformIndustry, e: SubmitEvent) {
		e.preventDefault();
		const label = rename.trim();
		if (!label || label === i.label) return;
		if (await guard(() => api().renameIndustry(i.industry, label), `Renamed to ${label}.`)) await reload();
	}
	async function setIndustryRetired(i: PlatformIndustry, retire: boolean) {
		const ok = await guard(
			() => (retire ? api().retireIndustry(i.industry) : api().restoreIndustry(i.industry)),
			retire ? `${i.label} retired: tenants no longer see it.` : `${i.label} is offered again.`,
			retire ? 'Not retired' : 'Not restored',
		);
		if (ok) await reload();
	}

	// ---- departments
	let deptLabel = $state('');
	let deptKey = $state('');
	let deptKeyTouched = $state(false);
	let deptExtends = $state('');
	const shownDeptKey = $derived(deptKeyTouched ? deptKey : keyOf(deptLabel));
	async function createDepartment(i: PlatformIndustry, e: SubmitEvent) {
		e.preventDefault();
		const label = deptLabel.trim();
		const department = shownDeptKey;
		if (!label || !KEY_RE.test(department)) return;
		const ok = await guard(
			() => api().createDepartment(i.industry, { department, label, extends: deptExtends || null }),
			`Department ${label} added to ${i.label}.`,
			'Not created',
		);
		if (ok) {
			deptLabel = '';
			deptKey = '';
			deptKeyTouched = false;
			deptExtends = '';
			await reload();
		}
	}
	let editing = $state<string | null>(null);
	let editLabel = $state('');
	async function renameDepartment(i: PlatformIndustry, d: PlatformDepartment, e: SubmitEvent) {
		e.preventDefault();
		const label = editLabel.trim();
		if (!label) return;
		const ok = await guard(
			() => api().renameDepartment(i.industry, d.department, label),
			`Renamed to ${label}.`,
		);
		if (ok) {
			editing = null;
			await reload();
		}
	}
	async function setDepartmentRetired(i: PlatformIndustry, d: PlatformDepartment, retire: boolean) {
		const ok = await guard(
			() =>
				retire
					? api().retireDepartment(i.industry, d.department)
					: api().restoreDepartment(i.industry, d.department),
			retire ? `${d.label} retired: tenants no longer see it.` : `${d.label} is offered again.`,
			retire ? 'Not retired' : 'Not restored',
		);
		if (ok) await reload();
	}
	const archetypeLabel = (k: string | null) =>
		k ? (archetypes.find((a) => a.key === k)?.label ?? k) : 'No archetype';
</script>

<PageHead
	id="P21"
	sub="The industries and departments tenants build their jobs in, and the knowledge each one gives every job built on it."
	help="A job inherits knowledge in this order: base (every job), its industry, then each of its departments. An industry or department is offered to tenants once its layer is published — attaching knowledge publishes it. Retire takes one back from new choices; jobs that already use it must move first."
>
	{#snippet actions()}
		<Freshness resource={data} />
		<a class="btn" href="/platform/knowledge">Platform knowledge</a>
	{/snippet}
</PageHead>

{#if packsError}
	<p class="err" role="alert">Knowledge packs are unavailable ({packsError}): attaching knowledge is off.</p>
{/if}

<LoadState resource={data}>
	{#snippet children(d)}
		<div class="ind-grid">
			<Card
				title="Base knowledge"
				sub="Every job on the platform inherits these packs, whatever its industry."
			>
				<LayerKnowledge layer="base" refs={d.base_knowledge} {packs} onchange={reload} />
			</Card>

			<Card title="Industries" sub="Pick one to edit it, its departments and its knowledge.">
				{#if d.items.length}
					<div class="tbl-wrap">
						<table class="tbl" data-testid="industries">
							<thead
								><tr><th>Industry</th><th>Departments</th><th>Status</th><th>In use</th><th></th></tr></thead
							>
							<tbody>
								{#each d.items as i (i.industry)}
									{@const st = status(i)}
									<tr class:on={picked === i.industry}>
										<td><strong>{i.label}</strong> <span class="mono muted">{i.industry}</span></td>
										<td>{i.departments.filter((x) => !x.retired_at).length}</td>
										<td><Chip text={st.text} variant={st.variant} /></td>
										<td>{usage(i.jobs)}</td>
										<td
											><button
												class="btn sm"
												type="button"
												aria-pressed={picked === i.industry}
												onclick={() => (picked = picked === i.industry ? '' : i.industry)}
												>{picked === i.industry ? 'Close' : 'Open'}</button
											></td
										>
									</tr>
								{/each}
							</tbody>
						</table>
					</div>
				{:else}
					<p class="muted">No industries yet: create the first one below.</p>
				{/if}
				<form class="ind-form" onsubmit={createIndustry} data-testid="industry-create">
					<label class="field"
						><span class="label">New industry</span><input
							class="input"
							bind:value={newLabel}
							placeholder="Telecommunications"
							aria-label="New industry name"
						/></label
					>
					<label class="field"
						><span class="label">Key</span><input
							class="input mono"
							value={shownKey}
							oninput={(e) => {
								keyTouched = true;
								newKey = e.currentTarget.value;
							}}
							aria-label="New industry key"
						/></label
					>
					<button
						class="btn primary"
						type="submit"
						{...sa}
						disabled={!newLabel.trim() || !KEY_RE.test(shownKey)}>Create industry</button
					>
				</form>
				<p class="muted small">The key is permanent: tags, templates and job selections hold it.</p>
			</Card>
		</div>

		{#if current}
			{@const i = current}
			{@const st = status(i)}
			<Card title={i.label} sub="{i.key} · {st.text}{i.tag ? ` · ${i.tag}` : ''}">
				<div class="ind-detail" data-testid="industry-detail">
					<form class="ind-form" onsubmit={(e) => renameIndustry(i, e)}>
						<label class="field"
							><span class="label">Name</span><input
								class="input"
								bind:value={rename}
								aria-label="Industry name"
							/></label
						>
						<button class="btn" type="submit" {...sa} disabled={!rename.trim() || rename.trim() === i.label}
							>Rename</button
						>
						{#if i.retired_at}
							<button class="btn" type="button" {...sa} onclick={() => setIndustryRetired(i, false)}
								>Restore</button
							>
						{:else}
							<button
								class="btn danger"
								type="button"
								{...sa}
								disabled={i.jobs.length > 0}
								title={i.jobs.length ? 'Tenant jobs still use it' : undefined}
								onclick={() => setIndustryRetired(i, true)}>Retire</button
							>
						{/if}
					</form>
					{#if i.jobs.length}
						<p class="muted small">
							Used by {i.jobs.map((j) => `${j.job_label} (${j.tenant_name})`).join(', ')}.
						</p>
					{/if}

					<h3>Industry knowledge</h3>
					<LayerKnowledge layer={i.key} refs={i.knowledge} {packs} onchange={reload} />

					<h3>Departments</h3>
					{#if i.departments.length}
						<ul class="list-plain ind-depts" data-testid="departments">
							{#each i.departments as dpt (dpt.department)}
								{@const ds = status(dpt)}
								<li class="ind-dept">
									<div class="ind-dept-head">
										{#if editing === dpt.department}
											<form class="ind-form" onsubmit={(e) => renameDepartment(i, dpt, e)}>
												<input class="input" bind:value={editLabel} aria-label="Department name" />
												<button class="btn sm" type="submit" {...sa} disabled={!editLabel.trim()}>Save</button
												>
												<button class="btn sm ghost" type="button" onclick={() => (editing = null)}
													>Cancel</button
												>
											</form>
										{:else}
											<strong>{dpt.label}</strong>
											<span class="mono muted">{dpt.department}</span>
											<Chip text={ds.text} variant={ds.variant} />
											<span class="muted">{archetypeLabel(dpt.extends)} · {usage(dpt.jobs)}</span>
											<button
												class="btn sm ghost"
												type="button"
												{...sa}
												onclick={() => {
													editing = dpt.department;
													editLabel = dpt.label;
												}}>Rename</button
											>
											{#if dpt.retired_at}
												<button
													class="btn sm"
													type="button"
													{...sa}
													disabled={!!i.retired_at}
													onclick={() => setDepartmentRetired(i, dpt, false)}>Restore</button
												>
											{:else}
												<button
													class="btn sm danger"
													type="button"
													{...sa}
													disabled={dpt.jobs.length > 0}
													title={dpt.jobs.length ? 'Tenant jobs still use it' : undefined}
													onclick={() => setDepartmentRetired(i, dpt, true)}>Retire</button
												>
											{/if}
										{/if}
									</div>
									<LayerKnowledge
										layer={dpt.key}
										refs={dpt.knowledge}
										{packs}
										disabled={!!dpt.retired_at}
										onchange={reload}
									/>
								</li>
							{/each}
						</ul>
					{:else}
						<p class="muted">No departments yet.</p>
					{/if}
					{#if !i.retired_at}
						<form class="ind-form" onsubmit={(e) => createDepartment(i, e)} data-testid="department-create">
							<label class="field"
								><span class="label">New department</span><input
									class="input"
									bind:value={deptLabel}
									placeholder="Customer care"
									aria-label="New department name"
								/></label
							>
							<label class="field"
								><span class="label">Key</span><input
									class="input mono"
									value={shownDeptKey}
									oninput={(e) => {
										deptKeyTouched = true;
										deptKey = e.currentTarget.value;
									}}
									aria-label="New department key"
								/></label
							>
							<label class="field"
								><span class="label">Extends</span><select
									class="input"
									bind:value={deptExtends}
									aria-label="Department archetype"
								>
									<option value="">No archetype</option>
									{#each archetypes as a (a.key)}<option value={a.key}>{a.label}</option>{/each}
								</select></label
							>
							<button
								class="btn primary"
								type="submit"
								{...sa}
								disabled={!deptLabel.trim() || !KEY_RE.test(shownDeptKey)}>Add department</button
							>
						</form>
					{/if}
				</div>
			</Card>
		{/if}
	{/snippet}
</LoadState>

<style>
	.ind-grid {
		display: grid;
		gap: 16px;
		grid-template-columns: minmax(0, 1fr);
		margin-bottom: 16px;
	}
	.ind-form {
		display: flex;
		gap: 8px;
		align-items: flex-end;
		flex-wrap: wrap;
		margin-top: 12px;
	}
	.ind-detail h3 {
		margin: 20px 0 6px;
	}
	.ind-depts {
		display: grid;
		gap: 12px;
	}
	.ind-dept {
		border: 1px solid var(--line, #ddd);
		border-radius: 8px;
		padding: 10px 12px;
	}
	.ind-dept-head {
		display: flex;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
	}
	.ind-dept-head .ind-form {
		margin-top: 0;
	}
	tr.on td {
		background: var(--bg-2, rgba(0, 0, 0, 0.03));
	}
	.small {
		font-size: 0.85em;
	}
</style>
