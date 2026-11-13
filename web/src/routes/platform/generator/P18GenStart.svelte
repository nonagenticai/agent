<script lang="ts">
	// P18 "New run": inputs, which kind of run (and so what it outputs) and the usage estimate — all before the first
	// LLM call. Platform runs output template layers; tenant runs output refinements only (D8). D56: no "who pays"
	// choice — the run's tokens are metered like turns (same funding lane), as the tenant's usage or the platform's own.
	import { goto } from '$app/navigation';
	import { api } from '$lib/app.svelte';
	import type { GeneratorEstimate } from '$lib/api/types/journey';
	import type { TenantSummary } from '$lib/api/types/core';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import { ApiError } from '$lib/api/errors';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrsOrSuperAdmin } from '$lib/auth/roles';
	import { FUNDING_LABEL, tokens } from '$lib/journey/format';
	import { stepMeta, type P18GenSeed } from '$lib/journey/P18GenPipeline';

	let {
		open = $bindable(false),
		seed = null,
		tenants,
	}: { open?: boolean; seed?: P18GenSeed | null; tenants: TenantSummary[] } = $props();

	let scope = $state<'platform' | 'tenant'>('platform');
	let tenantId = $state('');
	let industry = $state('telco');
	let department = $state('customer_service');
	let job = $state('telco_care_agent');
	let description = $state('');
	let error = $state('');
	let busy = $state(false);
	let est = $state<GeneratorEstimate | null>(null);
	let estErr = $state('');

	$effect(() => {
		if (open && seed) {
			industry = seed.industry;
			department = seed.department;
			job = seed.job;
		}
	});

	$effect(() => {
		if (!open) return;
		const input = { scope, tenant_id: scope === 'tenant' ? tenantId || null : null };
		estErr = '';
		api()
			.estimateGeneratorRun(input)
			.then((e) => (est = e))
			.catch((e) => {
				est = null;
				estErr = e instanceof ApiError ? e.message : 'Estimate unavailable.';
			});
	});

	const tenantName = $derived(tenants.find((t) => t.id === tenantId)?.name ?? null);

	async function start() {
		busy = true;
		error = '';
		try {
			const r = await api().startGeneratorRun({
				scope,
				industry,
				department,
				job,
				description,
				tenant_id: scope === 'tenant' ? tenantId || null : null,
			});
			toast(`Run #${r.id} started.`);
			open = false;
			await goto(`/platform/generator/${r.id}`);
		} catch (e) {
			error = e instanceof ApiError ? e.message : String(e);
		} finally {
			busy = false;
		}
	}
</script>

<Drawer bind:open title="New generator run" wide>
	<div class="stack p18s">
		<Seg
			items={[
				['platform', 'Platform run'],
				['tenant', 'Tenant run'],
			]}
			bind:active={scope}
			label="Kind of run"
		/>
		<div class="p18s-lane">
			{#if scope === 'platform'}
				<p>
					<b>Drafts a new industry / department / job template.</b> Output: template layers, provisional until
					Package tags them <span class="mono">@vN</span>. Tenants only see it after they pin the tag.
				</p>
			{:else}
				<p>
					<b>Refines one tenant's existing job.</b> Output: extension and hidden activities as refinements only
					(D8) — no new objects or operations. Reaches chat with that tenant's next release.
				</p>
				<div class="field">
					<label for="g-tenant">Tenant (the run refines its job)</label>
					<select id="g-tenant" class="input" bind:value={tenantId}>
						<option value="">Choose a tenant…</option>
						{#each tenants as t (t.id)}<option value={t.id}>{t.name}</option>{/each}
					</select>
				</div>
			{/if}
		</div>

		<div class="form-grid">
			<div class="field">
				<label for="g-ind">Industry</label>
				<input id="g-ind" class="input mono" bind:value={industry} />
			</div>
			<div class="field">
				<label for="g-dep">Department</label>
				<input id="g-dep" class="input mono" bind:value={department} />
			</div>
		</div>
		<div class="field">
			<label for="g-job">Job</label>
			<input id="g-job" class="input mono" bind:value={job} />
		</div>
		<div class="field">
			<label for="g-desc">Description, website or source</label>
			<textarea
				id="g-desc"
				class="input"
				rows="3"
				placeholder="What the business sells and to whom; a website; or the discovery cluster this refines"
				bind:value={description}
			></textarea>
			<small class="muted"
				>Step 1 (Business model) drafts axis values from this. Existing systems are mapped later by connector
				templates.</small
			>
		</div>

		<section class="p18s-cost" data-testid="p18-estimate" aria-live="polite">
			<h3>Expected usage</h3>
			{#if est}
				<div class="p18s-cost-row">
					<div>
						<span class="p18s-big">{tokens(est.tokens_low)}–{tokens(est.tokens_high)}</span>
						<span class="muted">tokens · {FUNDING_LABEL[est.funding]}</span>
					</div>
					{#if est.attributed_to === 'tenant'}
						<span class="chip cust">usage of {tenantName ?? est.tenant_name ?? 'the tenant'}</span>
					{:else}
						<span class="chip tool">platform's own usage</span>
					{/if}
				</div>
				<p class="small">
					LLM calls at {est.llm_steps.length} of {est.steps.length} steps:
					{est.llm_steps.map((k) => stepMeta(k).label).join(', ')}. Each draft is a separate call; re-drafting
					a step uses tokens again. Metered through the same usage pipeline as conversation turns — not a
					separate bill.
				</p>
				<p class="small muted">
					Range from {est.basis.length
						? `packaged ${scope} run${est.basis.length > 1 ? 's' : ''} ${est.basis.map((b) => `#${b}`).join(', ')}`
						: 'a default (no packaged run of this kind yet)'}.
				</p>
				{#if scope === 'tenant' && !tenantId}
					<p class="small warn-text">Choose the tenant — the run's usage is attributed to it.</p>
				{/if}
			{:else if estErr}
				<p class="small error">{estErr}</p>
			{:else}
				<p class="small muted">Estimating…</p>
			{/if}
			<p class="small">
				Nothing is published by starting: you approve every step, and Validate must be green before Package.
			</p>
		</section>

		{#if error}<p class="error" role="alert">{error}</p>{/if}
		<div class="row">
			<button
				class="btn primary"
				type="button"
				disabled={busy || writeAttrsOrSuperAdmin(session, 'job_designer').disabled}
				title={writeAttrsOrSuperAdmin(session, 'job_designer').title}
				onclick={start}>{busy ? 'Starting…' : 'Start run'}</button
			>
			<button class="btn" type="button" onclick={() => (open = false)}>Cancel</button>
		</div>
	</div>
</Drawer>

<style>
	.p18s-lane p {
		margin: 0 0 8px;
		font-size: 13px;
		line-height: 1.45;
	}
	.p18s-cost {
		border: 1px solid var(--line);
		border-radius: var(--r);
		padding: 10px 12px;
		background: var(--sunk);
	}
	.p18s-cost h3 {
		margin: 0 0 6px;
		font-size: 12px;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--muted);
	}
	.p18s-cost-row {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: 8px;
		flex-wrap: wrap;
	}
	.p18s-big {
		font-size: 18px;
		font-weight: 600;
	}
	.small {
		font-size: 12.5px;
		margin: 6px 0 0;
	}
	.error {
		color: var(--err);
		margin: 0;
	}
	.warn-text {
		color: var(--warn);
	}
</style>
