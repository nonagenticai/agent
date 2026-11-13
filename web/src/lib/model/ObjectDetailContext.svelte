<script lang="ts">
	// P10 "Where it lives": the three places an object is anchored outside its own rows —
	// template layers that define or reference it (composed base → industry → department → job, 11 §6.1),
	// connector templates that hold it in a system of record (doc 10 §4, status mapping), and the journey activities
	// that read or write it (11 §6.2 activity_object / operation_expectation).
	import type { ObjectContext, ObjectLayerTouch, TagItemKind } from '$lib/api/types/model';
	import Chip from '$lib/ui/Chip.svelte';
	import { tagHref } from './logic';
	import { SCOPE_LABEL } from './ui';

	let { ctx, label }: { ctx: ObjectContext; label: string } = $props();

	const KIND: Partial<Record<TagItemKind, string>> = {
		activity: 'activity',
		operation_expectation: 'operation expectation',
		axis_rule: 'axis rule',
		object: 'object',
		profile: 'business model',
		journey_stage: 'stage',
		agent_role: 'agent role',
		job_setting: 'job setting',
	};

	const layers = $derived.by(() => {
		const out: {
			key: string;
			scope: ObjectLayerTouch['scope'];
			label: string;
			latest: number | null;
			items: ObjectLayerTouch[];
		}[] = [];
		for (const t of ctx.layers) {
			let l = out.find((x) => x.key === t.layer_key);
			if (!l)
				out.push(
					(l = { key: t.layer_key, scope: t.scope, label: t.layer_label, latest: t.latest_tag, items: [] }),
				);
			l.items.push(t);
		}
		return out;
	});

	const industries = $derived.by(() => {
		const out: { industry: string; label: string; items: ObjectContext['activities'] }[] = [];
		for (const a of ctx.activities) {
			let g = out.find((x) => x.industry === a.industry);
			if (!g) out.push((g = { industry: a.industry, label: a.industry_label, items: [] }));
			g.items.push(a);
		}
		return out;
	});

	const counted = (items: ObjectLayerTouch[]) => {
		const m = new Map<string, number>();
		for (const i of items) if (i.role === 'references') m.set(i.kind, (m.get(i.kind) ?? 0) + 1);
		return [...m]
			.map(([k, n]) => {
				const w = KIND[k as TagItemKind] ?? k;
				return `${n} ${n === 1 ? w : w.endsWith('y') ? `${w.slice(0, -1)}ies` : `${w}s`}`;
			})
			.join(' · ');
	};
	const tplHref = (k: string, v: string) =>
		`/platform/connector-templates/${encodeURIComponent(k)}/${encodeURIComponent(v)}`;
</script>

<div class="grid g3 where" data-testid="object-where">
	<section class="col" aria-labelledby="w-layers">
		<h3 id="w-layers">Defined in templates</h3>
		<p class="muted lead">
			Layers compose base → industry → department → job; a later layer references or extends.
		</p>
		{#if layers.length === 0}
			<p class="empty">No template layer carries {label} yet.</p>
		{:else}
			<ol class="lineage" data-testid="object-layers">
				{#each layers as l (l.key)}
					{@const def = l.items.find((i) => i.role === 'defines')}
					<li class:defines={!!def}>
						<div class="split">
							<span class="scope">{SCOPE_LABEL[l.scope]}</span>
							{#if l.latest !== null}<a class="mono" href={tagHref(l.key, l.latest)}>{l.key}@v{l.latest}</a>
							{:else}<span class="mono">{l.key}</span>{/if}
						</div>
						{#if def}
							<p class="what">
								<b>Defines {label}</b> since v{def.since}{def.changed_in.length
									? `, changed in ${def.changed_in.map((v) => `v${v}`).join(', ')}`
									: ''}
								{#if def.pending}<Chip text="untagged edit" variant="warn" />{/if}
							</p>
							<p class="muted detail">{def.detail}</p>
						{/if}
						{#if l.items.some((i) => i.role === 'references')}
							<p class="what">References it: {counted(l.items)}</p>
							<p class="muted detail">
								{l.items
									.filter((i) => i.role === 'references')
									.map((i) => i.label)
									.join(' · ')}
							</p>
						{/if}
					</li>
				{/each}
			</ol>
		{/if}
	</section>

	<section class="col" aria-labelledby="w-systems">
		<h3 id="w-systems">Held in systems</h3>
		<p class="muted lead">
			Connector templates that read or write {label}, and how their statuses map onto ours.
		</p>
		{#if ctx.systems.length === 0}
			<p class="empty">No connector template implements an operation on {label}: it lives only in agent.</p>
		{:else}
			<ul class="list" data-testid="object-systems">
				{#each ctx.systems as s (s.template_key)}
					<li>
						<div class="split">
							<a href={tplHref(s.template_key, s.version)}>{s.label}</a>
							<span class="mono muted">@{s.version}</span>
						</div>
						<div class="chips">
							{#each s.ops as o (o)}<span class="mono op">{o.slice(o.indexOf('.') + 1)}</span>{/each}
						</div>
						<p class="muted detail">
							{s.status_map.length
								? `Maps ${s.status_map.length} system status${s.status_map.length === 1 ? '' : 'es'} onto ${new Set(s.status_map.map((m) => m.canonical)).size} canonical`
								: 'No status mapping'}
							·
							{s.instances.length
								? `${s.instances.length} instance${s.instances.length === 1 ? '' : 's'} (${[...new Set(s.instances.map((i) => i.tenant_name))].join(', ')})`
								: 'no tenant instance yet'}
						</p>
					</li>
				{/each}
			</ul>
		{/if}
	</section>

	<section class="col" aria-labelledby="w-journeys">
		<h3 id="w-journeys">Touched by journey activities</h3>
		<p class="muted lead">
			Activities that bind {label} as their subject or as context, and the operations they expect.
		</p>
		{#if industries.length === 0}
			<p class="empty">No journey activity uses {label}: rule 3 flags an unused model object.</p>
		{:else}
			{#each industries as g (g.industry)}
				<div class="ind">
					<div class="split">
						<a href="/platform/journeys/{g.industry}">{g.label}</a>
						<span class="muted"
							>{g.items.filter((a) => a.role === 'subject').length} subject · {g.items.filter(
								(a) => a.role === 'context',
							).length} context</span
						>
					</div>
					<ul class="list acts" data-testid="object-activities">
						{#each g.items as a (a.key)}
							<li>
								<span class="stage">{a.stage}</span>
								<span class="act" class:subject={a.role === 'subject'}>{a.label}{a.variant ?? ''}</span>
								<span class="ops">
									{#each a.operations as o (o.op)}<span
											class="mono op"
											class:opt={!o.required}
											title={o.required ? 'required' : 'optional'}
											>{o.op.slice(o.op.indexOf('.') + 1)}{o.required ? '' : '?'}</span
										>{/each}
								</span>
							</li>
						{/each}
					</ul>
				</div>
			{/each}
		{/if}
	</section>
</div>

<style>
	.where {
		gap: 20px;
	}
	.col {
		display: grid;
		gap: 8px;
		align-content: start;
	}
	h3 {
		margin: 0;
		font: 600 13px/1.2 var(--cond);
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.lead {
		margin: 0;
		font-size: 12.5px;
	}
	.lineage {
		list-style: none;
		margin: 0;
		padding: 0 0 0 14px;
		border-left: 2px solid var(--line);
		display: grid;
		gap: 10px;
	}
	.lineage li {
		position: relative;
		padding: 8px 10px;
		border: 1px solid var(--line-2);
		border-radius: 6px;
		background: var(--panel);
	}
	.lineage li::before {
		content: '';
		position: absolute;
		left: -21px;
		top: 14px;
		width: 10px;
		height: 10px;
		border-radius: 50%;
		background: var(--panel);
		border: 2px solid var(--line);
	}
	.lineage li.defines::before {
		background: var(--accent);
		border-color: var(--accent);
	}
	.lineage li.defines {
		border-color: color-mix(in srgb, var(--accent) 40%, var(--line-2));
	}
	.scope {
		font: 600 11px var(--cond);
		letter-spacing: 0.05em;
		text-transform: uppercase;
		color: var(--faint);
	}
	.what {
		margin: 4px 0 0;
		font-size: 13px;
		display: flex;
		gap: 6px;
		align-items: center;
		flex-wrap: wrap;
	}
	.detail {
		margin: 2px 0 0;
		font-size: 12px;
	}
	.list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 8px;
	}
	.list > li {
		padding: 8px 10px;
		border: 1px solid var(--line-2);
		border-radius: 6px;
		background: var(--panel);
		display: grid;
		gap: 4px;
	}
	.op {
		font-size: 11px;
		padding: 1px 6px;
		border-radius: 4px;
		background: color-mix(in srgb, var(--accent) 11%, transparent);
		color: var(--accent);
	}
	.op.opt {
		background: transparent;
		border: 1px dashed color-mix(in srgb, var(--accent) 50%, transparent);
	}
	.ind {
		display: grid;
		gap: 6px;
	}
	.ind .split {
		font-size: 13px;
	}
	.acts > li {
		grid-template-columns: 84px minmax(0, 1fr);
		gap: 2px 8px;
		padding: 6px 8px;
		font-size: 13px;
	}
	.stage {
		font: 500 11px var(--mono);
		color: var(--faint);
		padding-top: 2px;
	}
	.act.subject {
		font-weight: 600;
	}
	.ops {
		grid-column: 2;
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
	}
	.ops:empty {
		display: none;
	}
</style>
