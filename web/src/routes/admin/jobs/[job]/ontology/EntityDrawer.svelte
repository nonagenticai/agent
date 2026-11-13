<script lang="ts">
	// J11 entity detail: layer + origin, the platform object it is a view of (owner call #8), synonyms, attributes
	// (personal data, tenant-added, parent_missing), relations in/out. Writes: add synonym, add attribute, hide/unhide,
	// delete a tenant relation (base relations go with their entity). J11: opens with who owns the entity (inherited = locked,
	// what the tenant may still do; tenant = editable, saved to draft until published) and what uses it (activities and their
	// procedures, the slots it supplies, the platform operations on its object).
	// from cb:web/src/routes/admin/jobs/[job]/ontology/+page.svelte@45ee128 (entity drawer)
	import { api, app, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs, isSuperAdmin } from '$lib/auth/roles';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import type { ContextSpace, OntologyEntity, OntologyRelation } from '$lib/api/types/ontology';
	import { usageOf } from '$lib/ontology/J11usage';
	import { SIDE_EFFECT_LABEL, SIDE_EFFECT_VARIANT } from '$lib/ontology/shared';
	import { opLabel } from '$lib/ontology/catalogue';
	import { SCOPE_LABEL, relationsOf } from '$lib/ontology/layout';
	import { ORIGIN_LABEL, explain, when } from '$lib/ontology/shared';
	import { draftTitle, type J11DraftMarks } from '$lib/ontology/J11drafts';

	let {
		open = $bindable(),
		entity,
		entities,
		relations,
		spaces = [],
		marks,
		canWrite,
		onpick,
		onaddrelation,
		onchanged,
	}: {
		open: boolean;
		entity: OntologyEntity | null;
		entities: OntologyEntity[];
		relations: OntologyRelation[];
		/** Context spaces, for the "Used by" section. */
		spaces?: ContextSpace[];
		/** D54: the draft's per-row marks (from `draftDiff`); this entity's unpublished changes show in the header. */
		marks?: J11DraftMarks;
		canWrite: boolean;
		onpick: (name: string) => void;
		onaddrelation: (subject: string) => void;
		onchanged: () => Promise<void>;
	} = $props();

	const rel = $derived(entity ? relationsOf(entity.name, relations) : { out: [], in: [] });
	const pending = $derived(entity ? (marks?.entities.get(entity.name) ?? []) : []);
	const usage = $derived(entity ? usageOf(entity, spaces) : null);
	const sa = $derived(isSuperAdmin(session));
	const labelOf = (name: string) => entities.find((e) => e.name === name)?.label ?? name;
	const relKey = (r: OntologyRelation) => `${r.subject}|${r.predicate}|${r.object}`;

	let synonym = $state('');
	const ATTR_TYPES = ['string', 'text', 'number', 'decimal', 'money', 'date', 'datetime', 'boolean', 'enum'];
	let attrName = $state('');
	let attrType = $state('string');
	let attrPii = $state(false);
	let confirmKey = $state<string | null>(null);
	let relError = $state<string | null>(null);
	let busy = $state(false);
	$effect(() => {
		void entity?.name;
		synonym = '';
		attrName = '';
		attrType = 'string';
		attrPii = false;
		confirmKey = null;
		relError = null;
	});

	async function addSynonym(e: SubmitEvent) {
		e.preventDefault();
		if (!entity || !synonym.trim()) return;
		const term = synonym.trim();
		const ok = await guard(
			() => api().addOntologySynonym(entity.name, term),
			`Added “${term}” to ${entity.label}. Saved to draft.`,
		);
		if (ok) {
			synonym = '';
			await onchanged();
		}
	}
	async function addAttribute(e: SubmitEvent) {
		e.preventDefault();
		if (!entity || !attrName.trim()) return;
		const name = attrName.trim();
		const input = { name, data_type: attrType, pii: attrPii, synonyms: [] };
		const ok = await guard(
			() => api().addOntologyAttribute(entity.name, input),
			`Added attribute ${name} to ${entity.label}. Saved to draft.`,
		);
		if (ok) {
			attrName = '';
			attrPii = false;
			await onchanged();
		}
	}
	async function toggleHidden() {
		if (!entity) return;
		const to = !entity.hidden;
		const ok = await guard(
			() => api().setOntologyEntityHidden(entity.name, to),
			(to ? `${entity.label} is hidden for this job.` : `${entity.label} is visible again.`) +
				' Saved to draft.',
		);
		if (ok) await onchanged();
	}
	async function deleteRelation(r: OntologyRelation) {
		busy = true;
		relError = null;
		try {
			await api().deleteOntologyRelation(r);
			confirmKey = null;
			await onchanged();
		} catch (e) {
			relError = explain(e, 'delete relations');
		} finally {
			busy = false;
		}
	}
</script>

<Drawer bind:open title={entity?.label ?? 'Entity'} wide>
	{#snippet sub()}
		{#if entity}
			<span class="level {entity.scope}">{SCOPE_LABEL[entity.scope]}</span>
			<Chip text={ORIGIN_LABEL[entity.origin]} variant={entity.origin === 'tenant' ? 'accent' : 'muted'} />
			<span class="mono"
				>{entity.name}{entity.industry ? ` · ${entity.industry}` : ''}{entity.department
					? ` · ${entity.department}`
					: ''}</span
			>
			{#if entity.hidden}<Chip text="hidden" variant="dead" />{/if}
			{#if pending.length}<Chip text="Draft" variant="warn" title={draftTitle(pending)} />{/if}
		{/if}
	{/snippet}
	{#if entity}
		{#if pending.length}
			<div class="j11-pending" data-testid="j11-entity-draft">
				<b>{pending.length} unpublished change{pending.length === 1 ? '' : 's'} on this entity</b>
				<ul>
					{#each pending as p (p.id)}
						<li>
							{p.label}
							{#if p.live !== p.draft}<span class="muted"
									>— live: {p.live ?? 'none'} · draft: {p.draft ?? 'none'}</span
								>{/if}
						</li>
					{/each}
				</ul>
				<span class="muted"
					>Saved to draft, not live: <a href="/chat">test it in the test chat</a>, then
					<a href="/admin/jobs/{app.job}/release">review &amp; publish</a>.</span
				>
			</div>
		{/if}
		{#if entity.origin === 'tenant'}
			<div class="j11-own tenant" data-testid="j11-own">
				<b>+ Added by you</b>
				<span
					>A refinement of this job ({entity.layer}{entity.created_by ? `, by ${entity.created_by}` : ''}).
					You can change it here; changes are saved to draft and customers see them once you
					<a href="/admin/jobs/{app.job}/release">publish</a>.</span
				>
			</div>
		{:else}
			<div class="j11-own" data-testid="j11-own">
				<b
					><svg viewBox="0 0 24 24" width="12" height="12" aria-hidden="true"
						><path
							d="M7 11V8a5 5 0 0 1 10 0v3M5 11h14v10H5z"
							fill="none"
							stroke="currentColor"
							stroke-width="2.2"
							stroke-linejoin="round"
						/></svg
					>
					Inherited from {entity.layer}</b
				>
				<span
					>Its fields come from the template and are read-only here. You can add synonyms customers use, add
					attributes and relations, or hide it for this job — saved to draft, live once you publish a release.</span
				>
			</div>
		{/if}

		{#if usage}
			<section data-testid="j11-used-by">
				<h3>Used by</h3>
				{#if usage.spaces.length}
					<p class="j11-lead">
						The agent brings {entity.label} into {usage.spaces.length}
						{usage.spaces.length === 1 ? 'activity' : 'activities'}{#if usage.required.length}, and must
							collect
							{#each usage.required as r, i (r)}{i ? ', ' : ''}<span class="mono">{r}</span>{/each} before it acts{/if}.
					</p>
					<ul class="j11-uses">
						{#each usage.spaces as s (s.key)}
							<li>
								<span class="j11-use-label">{s.label}</span>
								<span class="mono muted">{s.activity}</span>
								<span class="spacer"></span>
								{#if s.procedure}<a class="mono" href="/admin/jobs/{app.job}/procedures/{s.procedure}"
										>procedure {s.procedure}</a
									>{/if}
							</li>
						{/each}
					</ul>
				{:else}
					<p class="j11-lead warn">
						No activity uses {entity.label} yet, so the agent never looks it up. Add it to an activity's context
						space on <a href="/admin/jobs/{app.job}/procedures">Procedures</a>.
					</p>
				{/if}
				{#if usage.operations.length}
					<p class="j11-lead">
						What the agent can do with it — operations on the platform object
						<span class="mono">{entity.object_key}</span>
						(routed on <a href="/admin/jobs/{app.job}/operations">Operations</a>):
					</p>
					<div class="chips">
						{#each usage.operations as o (opLabel(o.op, o.variant))}
							<Chip
								text={opLabel(o.op, o.variant)}
								variant={SIDE_EFFECT_VARIANT[o.side_effect]}
								title={SIDE_EFFECT_LABEL[o.side_effect]}
							/>
						{/each}
					</div>
				{/if}
			</section>
		{/if}

		<dl class="kvs">
			<dt>Platform object</dt>
			<dd>
				{#if entity.object_key}
					{#if sa}<a class="mono" href="/platform/objects/{entity.object_key}">{entity.object_key}</a
						>{:else}<span class="mono">{entity.object_key}</span>{/if}
					<span class="muted"> — operations and states live on the platform object</span>
				{:else}
					<span class="muted">Not linked to a platform object; it cannot back an operation.</span>
				{/if}
			</dd>
			<dt>Layer</dt>
			<dd class="mono">{entity.layer}</dd>
			<dt>Added</dt>
			<dd>
				{when(entity.created_at)}{entity.origin === 'tenant' && entity.created_by
					? ` by ${entity.created_by}`
					: ''}
			</dd>
		</dl>

		<section>
			<div class="sec-head">
				<h3>Synonyms</h3>
				<button type="button" class="btn sm" {...writeAttrs(session, 'job_designer')} onclick={toggleHidden}
					>{entity.hidden ? 'Unhide entity' : 'Hide entity'}</button
				>
			</div>
			<div class="chips">
				{#each entity.synonyms as s (s)}<Chip text={s} />{:else}<span class="muted">None.</span>{/each}
			</div>
			<form class="row syn" onsubmit={addSynonym}>
				<input
					class="input"
					bind:value={synonym}
					placeholder="Add a synonym customers use"
					aria-label="New synonym"
					disabled={!canWrite}
				/>
				<button class="btn sm" type="submit" {...writeAttrs(session, 'job_designer')}>Add</button>
			</form>
		</section>

		<section>
			<h3>Attributes ({entity.attributes.length})</h3>
			<div class="tbl-wrap">
				<table class="tbl dense">
					<thead
						><tr><th>Name</th><th>Type</th><th>Personal data</th><th>Origin</th><th>Synonyms</th></tr></thead
					>
					<tbody>
						{#each entity.attributes as a (a.name)}
							<tr>
								<td class="mono">{a.name}</td>
								<td class="mono">{a.data_type}</td>
								<td
									>{#if a.pii}<Chip text="personal data" variant="err" />{:else}<span class="muted">no</span
										>{/if}</td
								>
								<td>
									<Chip text={ORIGIN_LABEL[a.origin]} variant={a.origin === 'tenant' ? 'accent' : 'muted'} />
									{#if a.conflict}<Chip
											text={a.conflict}
											variant="warn"
											title="Base parent changed; see Release → Conflicts"
										/>{/if}
								</td>
								<td>{a.synonyms.join(', ')}</td>
							</tr>
						{:else}
							<tr><td colspan="5" class="muted">No attributes.</td></tr>
						{/each}
					</tbody>
				</table>
			</div>
			<form class="row syn attr" onsubmit={addAttribute} data-testid="j11-add-attribute">
				<input
					class="input"
					bind:value={attrName}
					placeholder="New attribute, e.g. gift_message"
					aria-label="New attribute name"
					disabled={!canWrite}
				/>
				<select
					class="input attr-type"
					bind:value={attrType}
					aria-label="Attribute type"
					disabled={!canWrite}
				>
					{#each ATTR_TYPES as t (t)}<option value={t}>{t}</option>{/each}
				</select>
				<label class="attr-pii"
					><input type="checkbox" bind:checked={attrPii} disabled={!canWrite} /> personal data</label
				>
				<button class="btn sm" type="submit" {...writeAttrs(session, 'job_designer')}>Add attribute</button>
			</form>
		</section>

		{#if relError}
			<div class="callout err" role="alert"><strong>Not deleted</strong><span>{relError}</span></div>
		{/if}
		<section>
			<div class="sec-head">
				<h3>Relations out ({rel.out.length})</h3>
				<button
					type="button"
					class="btn sm"
					{...writeAttrs(session, 'job_designer')}
					onclick={() => onaddrelation(entity.name)}>Add relation</button
				>
			</div>
			{#each rel.out as r (relKey(r))}
				<div class="rel">
					<span class="mono">{r.predicate}</span> →
					<button type="button" class="link" onclick={() => onpick(r.object)}>{labelOf(r.object)}</button>
					<span class="mono muted">{r.cardinality}</span>
					{#if r.origin === 'tenant'}<Chip text="Tenant" variant="accent" />{/if}
					{@render relDelete(r)}
				</div>
			{:else}
				<p class="muted">None.</p>
			{/each}
		</section>
		<section>
			<h3>Relations in ({rel.in.length})</h3>
			{#each rel.in as r (relKey(r))}
				<div class="rel">
					<button type="button" class="link" onclick={() => onpick(r.subject)}>{labelOf(r.subject)}</button>
					<span class="mono">{r.predicate}</span> → this
					<span class="mono muted">{r.cardinality}</span>
					{#if r.origin === 'tenant'}<Chip text="Tenant" variant="accent" />{/if}
					{@render relDelete(r)}
				</div>
			{:else}
				<p class="muted">None.</p>
			{/each}
		</section>
	{/if}
</Drawer>

{#snippet relDelete(r: OntologyRelation)}
	{#if r.origin === 'tenant'}
		<span class="spacer"></span>
		{#if confirmKey === relKey(r)}
			<span class="confirm" role="group" aria-label="Confirm delete">
				<span class="muted">Delete {r.subject} {r.predicate} {r.object}?</span>
				<button type="button" class="btn sm danger" disabled={busy} onclick={() => deleteRelation(r)}
					>{busy ? 'Deleting…' : 'Delete'}</button
				>
				<button type="button" class="btn sm" onclick={() => (confirmKey = null)}>Keep</button>
			</span>
		{:else}
			<button
				type="button"
				class="btn sm"
				{...writeAttrs(session, 'job_designer')}
				aria-label="Delete relation {r.subject} {r.predicate} {r.object}"
				onclick={() => {
					confirmKey = relKey(r);
					relError = null;
				}}>Delete</button
			>
		{/if}
	{/if}
{/snippet}

<style>
	.sec-head {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
		gap: 8px;
	}
	.confirm {
		display: inline-flex;
		gap: 6px;
		align-items: baseline;
		flex-wrap: wrap;
	}
	.syn {
		display: flex;
		gap: 6px;
		margin-top: 8px;
	}
	.syn .input {
		flex: 1;
	}
	.attr {
		flex-wrap: wrap;
		align-items: center;
	}
	.syn .attr-type {
		flex: 0 0 auto;
	}
	.attr-pii {
		display: inline-flex;
		gap: 4px;
		align-items: center;
		white-space: nowrap;
	}
	.rel {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: baseline;
		padding: 3px 0;
		font-size: 13px;
	}
	.spacer {
		flex: 1;
	}
	.link {
		all: unset;
		cursor: pointer;
		color: var(--accent);
	}
	.link:hover {
		text-decoration: underline;
	}
	.link:focus-visible {
		outline: 2px solid var(--accent);
		outline-offset: 2px;
	}
	.level {
		--lv: var(--accent);
		font: 600 10px/1.4 var(--sans);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		padding: 2px 7px;
		border-radius: 999px;
		color: var(--lv);
		background: color-mix(in srgb, var(--lv) 14%, transparent);
	}
	.level.generic {
		--lv: var(--cust);
	}
	.level.industry {
		--lv: var(--tool);
	}
	.level.industry_dept {
		--lv: var(--work);
	}
	.j11-own {
		display: grid;
		gap: 3px;
		padding: 10px 12px;
		margin-bottom: 12px;
		border-radius: 8px;
		border: 1px solid var(--line);
		background: var(--sunk);
		font-size: 13px;
		color: var(--muted);
	}
	.j11-own b {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		color: var(--fg);
	}
	.j11-own.tenant {
		border: 1.5px dashed color-mix(in srgb, var(--accent) 60%, var(--line));
		background: color-mix(in srgb, var(--accent) 6%, var(--panel));
	}
	.j11-own.tenant b {
		color: var(--accent);
	}
	.j11-lead {
		margin: 0 0 8px;
		font-size: 13px;
	}
	.j11-lead.warn {
		color: var(--warn);
	}
	.j11-uses {
		list-style: none;
		margin: 0 0 10px;
		padding: 0;
		display: grid;
		gap: 4px;
	}
	.j11-uses li {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: baseline;
		padding: 6px 10px;
		border: 1px solid var(--line-2);
		border-radius: 6px;
		font-size: 13px;
	}
	.j11-use-label {
		font-weight: 600;
	}
	.chips {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
	}
	h3 {
		margin: 0 0 8px;
		font: 600 13px/1.3 var(--cond);
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--muted);
	}
	section {
		margin-top: 14px;
	}
	.j11-pending {
		border: 1px solid color-mix(in srgb, var(--warn) 45%, var(--line));
		border-left: 3px solid var(--warn);
		border-radius: var(--r);
		padding: 8px 12px;
		margin-bottom: 12px;
		font-size: 13px;
	}
	.j11-pending ul {
		margin: 4px 0 6px;
		padding-left: 18px;
	}
</style>
