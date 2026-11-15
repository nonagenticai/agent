<script lang="ts">
	import { untrack } from 'svelte';
	// J12 "New procedure version": a tenant DRAFT, optionally copied from an existing version. Steps pick an
	// operation from the catalogue; the live grant check shows which steps the cut will refuse (409).
	// from cb:web/src/routes/admin/jobs/[job]/procedures/+page.svelte@45ee128 (create drawer; JSON plan/steps
	// replaced by a step editor over `object.verb{variant}` operations)
	import { goto } from '$app/navigation';
	import { api, app } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import { grantCovers } from '$lib/api/areas/ontology';
	import { OPERATION_CATALOGUE, catalogueOp } from '$lib/ontology/catalogue';
	import type { Condition, ContextSpace, ProcedureStep, ProcedureView } from '$lib/api/types/ontology';
	import { SIDE_EFFECT_VARIANT, explain } from '$lib/ontology/shared';
	import {
		MAX_CAP,
		MAX_PDDL_BYTES,
		byteLength,
		parseTool,
		parseValue,
		stepsProblem,
		takesCap,
		valueText,
	} from '$lib/ontology/conditions';
	import { nextVersion, stepOp } from './view';
	import ConditionRows, { type CondDraft } from './ConditionRows.svelte';

	let {
		open = $bindable(),
		from,
		procedures,
		spaces,
		oncreated,
	}: {
		open: boolean;
		from: string;
		procedures: ProcedureView[];
		spaces: ContextSpace[];
		oncreated: () => Promise<void>;
	} = $props();

	type StepDraft = {
		phase: number;
		name: string;
		operation: string;
		variant: string;
		gate: boolean;
		// T7a (F-15, F-20)
		pre: CondDraft[];
		post: CondDraft[];
		performed_by: string;
		max_per_case: string;
		open: boolean;
	};
	type Draft = {
		from: string;
		name: string;
		version: string;
		context_space: string;
		description: string;
		steps: StepDraft[];
		success: CondDraft[];
		tools: string[];
		pddl: string;
	};
	const blank = (): Draft => ({
		from: '',
		name: '',
		version: '1',
		context_space: '',
		description: '',
		steps: [],
		success: [],
		tools: [],
		pddl: '',
	});
	const toDraft = (cs: Condition[] | undefined): CondDraft[] =>
		(cs ?? []).map((c) => ({ id: c.id, fact: c.fact, op: c.op, value: valueText(c.value) }));
	const fromDraft = (cs: CondDraft[]): Condition[] =>
		cs.map((c) => {
			const value = parseValue(c.op, c.value);
			return value === undefined
				? { id: c.id.trim(), fact: c.fact.trim(), op: c.op }
				: { id: c.id.trim(), fact: c.fact.trim(), op: c.op, value };
		});

	/** The job's agent roles (`performed_by`); the server refuses any other (422 `unknown_role`). */
	let roles = $state<string[]>(['qualifier', 'procedure', 'post_gate', 'assistant']);
	let toolPick = $state('');
	let form = $state<Draft>(blank());
	let start = $state('');
	let error = $state<string | null>(null);
	let saving = $state(false);
	const dirty = $derived(open && JSON.stringify(form) !== start);

	// A fresh form each time the drawer opens; later prop changes (a refresh) must not wipe the edits.
	$effect(() => {
		if (!open) return;
		untrack(() => {
			form = blank();
			if (from) copyFrom(from);
			start = JSON.stringify(form);
			error = null;
			api()
				.toolsetMeta()
				.then((m) => {
					if (m.roles.length) roles = [...new Set(m.roles.map((r) => r.role as string))];
				})
				.catch(() => {});
		});
	});

	function copyFrom(key: string) {
		form.from = key;
		const p = procedures.find((x) => `${x.name}@${x.version}` === key);
		if (!p) return;
		const versions = procedures.filter((x) => x.name === p.name).map((x) => x.version);
		form.name = p.name;
		form.version = String(nextVersion(versions));
		form.context_space = p.context_space;
		form.description = p.description;
		form.steps = p.steps.map((s) => ({
			phase: s.phase,
			name: s.name,
			operation: s.operation,
			variant: s.variant ?? '',
			gate: s.gate,
			pre: toDraft(s.preconditions),
			post: toDraft(s.postconditions),
			performed_by: s.performed_by ?? '',
			max_per_case: s.max_per_case == null ? '' : String(s.max_per_case),
			open: !!(s.preconditions?.length || s.postconditions?.length || s.performed_by || s.max_per_case),
		}));
		form.success = toDraft(p.success_criteria);
		form.tools = [...(p.tool_requirements ?? [])];
		form.pddl = p.pddl_domain ?? '';
	}
	function addTool() {
		const t = toolPick.trim();
		if (t && !form.tools.includes(t)) form.tools.push(t);
		toolPick = '';
	}
	/** Fact paths to suggest at step `i` (its own step only for postconditions; the server is the judge). */
	function factsFor(i: number | null, post = false): string[] {
		const slots = spaces.find((x) => x.key === form.context_space)?.required ?? [];
		const me = i === null ? null : (form.steps[i] ?? null);
		const steps = form.steps.filter(
			(s, j) => s.name.trim() && (me === null || s.phase < me.phase || (post && j === i)),
		);
		return [
			...slots.map((x) => `slots.${x}`),
			'identity.level',
			'customer.tier',
			'case.activity',
			...steps.flatMap((s) => [`steps.${s.name.trim()}.outcome`, `steps.${s.name.trim()}.out.`]),
		];
	}
	function pickSpace(key: string) {
		form.context_space = key;
		const sp = spaces.find((s) => s.key === key);
		if (sp && !form.name) form.name = sp.procedure;
	}

	const grant = $derived(
		procedures.find((p) => p.context_space === form.context_space)?.activity_grant ?? null,
	);
	const outside = $derived(
		grant
			? form.steps
					.filter((s) => s.operation)
					.map((s) => stepOp({ operation: s.operation, variant: s.variant.trim() || null }))
					.filter(
						(op) =>
							!['knowledge.search', 'knowledge.answer', 'rule.evaluate'].includes(op.split('{')[0] ?? '') &&
							!grant.some((g) => grantCovers(g, op)),
					)
			: [],
	);

	async function submit(e: SubmitEvent) {
		e.preventDefault();
		error = null;
		const steps: ProcedureStep[] = form.steps.map((s, i) => {
			const c = catalogueOp(s.operation);
			const cap = s.max_per_case.trim();
			return {
				step_number: i + 1,
				phase: Number(s.phase) || 0,
				name: s.name.trim() || s.operation,
				operation: s.operation,
				variant: s.variant.trim() || null,
				modality: c?.modality ?? 'observe',
				side_effect: c?.side_effect ?? 'R',
				identity_level: c?.identity_level ?? '0',
				confirm_back: (c?.side_effect ?? 'R') !== 'R',
				gate: s.gate,
				args: {},
				preconditions: fromDraft(s.pre),
				postconditions: fromDraft(s.post),
				performed_by: s.performed_by || null,
				max_per_case: cap && takesCap(c?.side_effect ?? 'R') ? Number(cap) : null,
			};
		});
		if (steps.some((s) => !s.operation)) return (error = 'Every step needs an operation.');
		const success = fromDraft(form.success);
		const bad = stepsProblem(steps, roles, success);
		if (bad) return (error = bad.message);
		if (form.tools.some((t) => !parseTool(t) || !catalogueOp(parseTool(t)![0])))
			return (error = 'Every needed tool is an operation from the catalogue.');
		if (byteLength(form.pddl) > MAX_PDDL_BYTES)
			return (error = `A PDDL domain is at most ${MAX_PDDL_BYTES / 1024} KiB.`);
		saving = true;
		try {
			const d = await api().createProcedureVersion({
				name: form.name.trim(),
				version: Number(form.version.trim()),
				context_space: form.context_space,
				description: form.description,
				steps,
				success_criteria: success,
				tool_requirements: form.tools,
				pddl_domain: form.pddl.trim() ? form.pddl : null,
			});
			toast(
				`Saved to draft: ${d.name}@${d.version}. Test it in the test chat; it goes live only with a release.`,
			);
			start = JSON.stringify(form);
			open = false;
			await oncreated();
			goto(`/admin/jobs/${app.job}/procedures/${d.name}?v=${d.version}`);
		} catch (err) {
			error = explain(err, 'create procedure versions');
		} finally {
			saving = false;
		}
	}
</script>

<Drawer bind:open title="New procedure version" wide {dirty}>
	<form class="stack" novalidate onsubmit={submit}>
		<p class="muted">
			Creates a tenant DRAFT. Move it to CANARY for review, then a Release manager publishes it: it then
			shadows the base version of the same procedure.
		</p>
		<div class="field">
			<label for="nv-from">Start from</label>
			<select id="nv-from" class="input" value={form.from} onchange={(e) => copyFrom(e.currentTarget.value)}>
				<option value="">Empty procedure</option>
				{#each procedures as p (p.name + '@' + p.version)}
					<option value="{p.name}@{p.version}">{p.name}@{p.version} ({p.status}, {p.origin})</option>
				{/each}
			</select>
		</div>
		<div class="form-grid">
			<div class="field">
				<label for="nv-space">Context space</label>
				<select
					id="nv-space"
					class="input mono"
					value={form.context_space}
					onchange={(e) => pickSpace(e.currentTarget.value)}
				>
					<option value="">Pick a space…</option>
					{#each spaces as s (s.key)}<option value={s.key}>{s.key}</option>{/each}
				</select>
			</div>
			<div class="field">
				<label for="nv-name">Name</label>
				<input id="nv-name" class="input mono" bind:value={form.name} placeholder="return" />
			</div>
			<div class="field">
				<label for="nv-version">Version</label>
				<input
					id="nv-version"
					class="input mono"
					bind:value={form.version}
					placeholder="3"
					inputmode="numeric"
				/>
			</div>
			<div class="field">
				<label for="nv-desc">What changes</label>
				<input id="nv-desc" class="input" bind:value={form.description} />
			</div>
		</div>

		<h3>Steps</h3>
		<div class="tbl-wrap">
			<table class="tbl dense steps">
				<thead
					><tr
						><th>#</th><th>Phase</th><th>Name</th><th>Operation</th><th>Variant</th><th>Effect</th><th
							>Gate</th
						><th></th></tr
					></thead
				>
				<tbody>
					{#each form.steps as s, i (i)}
						{@const c = catalogueOp(s.operation)}
						{@const label = s.operation
							? stepOp({ operation: s.operation, variant: s.variant.trim() || null })
							: ''}
						<tr class:bad={outside.includes(label)}>
							<td class="r">{i + 1}</td>
							<td
								><input
									class="input mono ph"
									type="number"
									min="0"
									aria-label="Step {i + 1} phase"
									bind:value={s.phase}
								/></td
							>
							<td><input class="input" aria-label="Step {i + 1} name" bind:value={s.name} /></td>
							<td>
								<select class="input mono" aria-label="Step {i + 1} operation" bind:value={s.operation}>
									<option value="">Pick…</option>
									{#each OPERATION_CATALOGUE as o (o.op)}<option value={o.op}>{o.op}</option>{/each}
								</select>
							</td>
							<td
								><input
									class="input mono"
									aria-label="Step {i + 1} variant"
									bind:value={s.variant}
									placeholder="—"
								/></td
							>
							<td
								>{#if c}<Chip text={c.side_effect} variant={SIDE_EFFECT_VARIANT[c.side_effect]} />{/if}</td
							>
							<td><input type="checkbox" aria-label="Step {i + 1} gates the case" bind:checked={s.gate} /></td
							>
							<td>
								<button
									type="button"
									class="btn sm"
									aria-expanded={s.open}
									aria-label="Step {i + 1} conditions"
									onclick={() => (s.open = !s.open)}
									>Conditions{s.pre.length + s.post.length
										? ` (${s.pre.length + s.post.length})`
										: ''}</button
								>
								<button
									type="button"
									class="btn sm"
									aria-label="Remove step {i + 1}"
									onclick={() => form.steps.splice(i, 1)}>Remove</button
								>
							</td>
						</tr>
						{#if s.open}
							<tr class="checks">
								<td></td>
								<td colspan="7">
									<div class="checks-grid">
										<span class="k">Before it runs</span>
										<ConditionRows
											bind:rows={s.pre}
											label="Step {i + 1} preconditions"
											facts={factsFor(i)}
											idPrefix="nv-pre-{i}"
										/>
										<span class="k">After an ok answer</span>
										<ConditionRows
											bind:rows={s.post}
											label="Step {i + 1} postconditions"
											facts={factsFor(i, true)}
											idPrefix="nv-post-{i}"
										/>
										<label class="k" for="nv-by-{i}">Performed by</label>
										<select id="nv-by-{i}" class="input" bind:value={s.performed_by}>
											<option value="">The procedure role</option>
											{#each roles as r (r)}<option value={r}>{r}</option>{/each}
										</select>
										{#if c && takesCap(c.side_effect)}
											<label class="k" for="nv-cap-{i}">Max per case</label>
											<input
												id="nv-cap-{i}"
												class="input mono cap"
												type="number"
												min="1"
												max={MAX_CAP}
												placeholder="1"
												bind:value={s.max_per_case}
											/>
										{/if}
									</div>
								</td>
							</tr>
						{/if}
					{:else}
						<tr><td colspan="8" class="muted">No steps yet.</td></tr>
					{/each}
				</tbody>
			</table>
		</div>
		<button
			type="button"
			class="btn sm add"
			onclick={() =>
				form.steps.push({
					phase: form.steps.length ? form.steps[form.steps.length - 1]!.phase + 1 : 0,
					name: '',
					operation: '',
					variant: '',
					gate: false,
					pre: [],
					post: [],
					performed_by: '',
					max_per_case: '',
					open: false,
				})}>Add step</button
		>

		<h3>Success</h3>
		<p class="note">
			Checked when the last phase finished: if one does not hold, the case is handed over to staff.
		</p>
		<ConditionRows
			bind:rows={form.success}
			label="Success criteria"
			facts={factsFor(null)}
			idPrefix="nv-ok"
		/>

		<h3>Needs tools</h3>
		<div class="tools">
			{#each form.tools as t, i (t)}
				<span class="tool"
					><Chip text={t} variant="tool" /><button
						type="button"
						class="btn sm"
						aria-label="Remove needed tool {t}"
						onclick={() => form.tools.splice(i, 1)}>×</button
					></span
				>
			{:else}
				<span class="muted">None: the procedure starts without checking its tools are routed.</span>
			{/each}
		</div>
		<div class="row">
			<select class="input mono" aria-label="Needed tool" bind:value={toolPick}>
				<option value="">Pick an operation…</option>
				{#each OPERATION_CATALOGUE as o (o.op)}<option value={o.op}>{o.op}</option>{/each}
			</select>
			<button type="button" class="btn sm" disabled={!toolPick} onclick={addTool}>Add</button>
		</div>

		<details class="pddl">
			<summary>PDDL domain (optional, stored as written, not interpreted)</summary>
			<textarea
				class="input mono"
				rows="6"
				aria-label="PDDL domain"
				placeholder="(define (domain …))"
				bind:value={form.pddl}></textarea>
		</details>

		{#if grant}
			<p class="note">
				Activity grant: <span class="mono">{grant.join(' · ')}</span>
			</p>
			{#if outside.length}
				<div class="callout err" role="status">
					<strong>{outside.join(', ')} outside the activity grant</strong>
					<span
						>You can save the DRAFT, but moving it to CANARY or cutting a release with it is refused (409).</span
					>
				</div>
			{/if}
		{/if}
		{#if error}
			<div class="callout err" role="alert"><strong>Not created</strong><span>{error}</span></div>
		{/if}
		<div class="row">
			<button
				type="submit"
				class="btn primary"
				{...writeAttrs(session, 'job_designer')}
				disabled={saving || writeAttrs(session, 'job_designer').disabled}
				>{saving ? 'Creating…' : 'Create DRAFT'}</button
			>
			<button type="button" class="btn" onclick={() => (open = false)}>Cancel</button>
		</div>
	</form>
</Drawer>

<style>
	.steps .input {
		padding: 4px 7px;
	}
	.ph {
		width: 56px;
	}
	tr.bad td {
		background: color-mix(in srgb, var(--err) 8%, transparent);
	}
	.add {
		margin-top: 8px;
	}
	tr.checks td {
		background: var(--bg-2, transparent);
	}
	.checks-grid {
		display: grid;
		grid-template-columns: max-content 1fr;
		gap: 6px 12px;
		align-items: start;
	}
	.k {
		font-size: 12px;
		color: var(--muted);
		padding-top: 4px;
	}
	.cap {
		width: 72px;
	}
	.tools {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.tool {
		display: inline-flex;
		align-items: center;
		gap: 2px;
	}
	.pddl textarea {
		width: 100%;
		margin-top: 6px;
	}
	.row {
		display: flex;
		gap: 8px;
	}
	h3 {
		margin: 6px 0 0;
		font: 600 13px/1.3 var(--cond);
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--muted);
	}
</style>
