<script lang="ts">
	// J8: every agent role × every activity of the job, one cell per computed toolset (callable / expected by the
	// activity). The reader sees at a glance that only the procedure role acts, where a tenant narrowing or a
	// missing route leaves a required operation uncallable, and picks the cell to explain.
	import type {
		ActivityRef,
		AgentRole,
		AgentRoleKey,
		Toolset,
		ToolsetActivityProcedure,
	} from '$lib/api/types/ontology';
	import '$lib/phone-stack.css';
	import { STAGES } from '$lib/ontology/catalogue';
	import { ROLE_LABEL, cellCounts } from '$lib/ontology/toolsetsJ8';

	let {
		roles,
		activities,
		toolsets,
		procedures,
		role,
		activity,
		onpick,
	}: {
		roles: AgentRole[];
		activities: ActivityRef[];
		toolsets: Toolset[];
		procedures: ToolsetActivityProcedure[];
		role: string;
		activity: string;
		onpick: (role: AgentRoleKey, activity: string) => void;
	} = $props();

	const stageOrder = (s: string) => STAGES.findIndex(([k]) => k === s);
	const groups = $derived(
		[...new Set(activities.map((a) => a.stage))]
			.sort((a, b) => stageOrder(a) - stageOrder(b))
			.map((stage) => ({
				stage,
				label: STAGES.find(([k]) => k === stage)?.[1] ?? stage,
				rows: activities.filter((a) => a.stage === stage),
			})),
	);
	const find = (r: string, a: string) => toolsets.find((t) => t.role === r && t.activity === a);
	const procsFor = (a: string) => procedures.filter((p) => p.activity === a);
</script>

<div class="tbl-wrap j8m ph-stack" data-testid="j8-matrix">
	<table class="tbl dense">
		<thead>
			<tr>
				<th>Activity</th>
				{#each roles as r (r.role)}
					<th class="c" class:off={!r.enabled}
						>{ROLE_LABEL[r.role]}{#if !r.enabled}<span class="j8m-off"> · off</span>{/if}<span class="j8m-max"
							>max {r.max_side_effect}</span
						></th
					>
				{/each}
				<th>Procedure that runs</th>
			</tr>
		</thead>
		<tbody>
			{#each groups as g (g.stage)}
				<tr class="j8m-stage"><th colspan={roles.length + 2} scope="colgroup">{g.label}</th></tr>
				{#each g.rows as a (a.key)}
					<tr class:hid={a.hidden}>
						<td class="j8m-act">
							<span class="j8m-al">{a.label}</span>
							<span class="j8m-ak mono">{a.key}</span>
							{#if a.hidden}<span class="chip muted">hidden</span>{/if}
							{#if a.origin === 'tenant'}<span class="chip cust">tenant</span>{/if}
						</td>
						{#each roles as r (r.role)}
							{@const ts = find(r.role, a.key)}
							{@const c = ts ? cellCounts(ts) : null}
							{@const on = role === r.role && activity === a.key}
							<td class="c" class:ph-off={!ts} data-label={ROLE_LABEL[r.role]}>
								{#if ts && c}
									<button
										type="button"
										class="j8m-cell"
										class:on
										class:zero={c.callable === 0}
										class:missing={r.role === 'procedure' && c.requiredMissing > 0}
										class:offr={!r.enabled}
										aria-pressed={on}
										aria-label="{ROLE_LABEL[r.role]} · {a.label}: {c.callable} of {c.expected} callable"
										onclick={() => onpick(r.role, a.key)}
									>
										{#if !r.enabled}
											<span class="j8m-n">off</span>
										{:else}
											<span class="j8m-n">{c.callable}<span class="j8m-of">/{c.expected}</span></span>
											<span class="j8m-bar" aria-hidden="true"
												><i style="width:{c.expected ? (100 * c.callable) / c.expected : 0}%"></i></span
											>
										{/if}
										{#if c.narrowed}<span class="j8m-flag" title="Narrowed by this tenant">✂</span>{/if}
									</button>
								{/if}
							</td>
						{/each}
						<td class="j8m-proc" data-label="Procedure">
							{#each procsFor(a.key) as p (p.name)}
								<span class="mono">{p.name}</span><span class="faint j8m-v">v{p.version}</span><br />
							{:else}
								<span class="faint">none yet</span>
							{/each}
						</td>
					</tr>
				{/each}
			{/each}
		</tbody>
	</table>
</div>
<p class="j8m-legend">
	<span><b>3/5</b> = callable / operations the activity expects</span>
	<span><span class="sw ok"></span>all callable</span>
	<span><span class="sw warn"></span>a required operation is cut for the procedure</span>
	<span>✂ narrowed by you</span>
</p>

<style>
	.j8m th.c,
	.j8m td.c {
		text-align: center;
	}
	.j8m th.off {
		color: var(--faint);
	}
	.j8m-off {
		font-weight: 400;
	}
	.j8m-max {
		display: block;
		font: 500 10px/1.3 var(--mono);
		letter-spacing: 0;
		text-transform: none;
		color: var(--faint);
	}
	.j8m-stage th {
		background: var(--sunk);
		font: 600 11px/1.2 var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--muted);
		padding: 5px 10px;
	}
	.j8m-act {
		min-width: 190px;
	}
	.j8m-al {
		font-weight: 500;
		margin-right: 6px;
	}
	.j8m-ak {
		display: block;
		font-size: 11px;
		color: var(--faint);
	}
	tr.hid .j8m-al {
		color: var(--faint);
	}
	.j8m-cell {
		position: relative;
		display: inline-grid;
		gap: 3px;
		justify-items: center;
		min-width: 64px;
		padding: 5px 8px;
		border: 1px solid var(--line-2);
		border-radius: 6px;
		background: var(--panel);
		color: var(--fg);
		cursor: pointer;
		font: inherit;
	}
	.j8m-cell:hover {
		border-color: var(--accent);
	}
	.j8m-cell.on {
		border-color: var(--accent);
		box-shadow: 0 0 0 2px color-mix(in srgb, var(--accent) 35%, transparent);
		background: color-mix(in srgb, var(--accent) 8%, var(--panel));
	}
	.j8m-cell.zero .j8m-n,
	.j8m-cell.offr .j8m-n {
		color: var(--faint);
	}
	.j8m-cell.missing {
		border-color: color-mix(in srgb, var(--warn) 60%, var(--line));
	}
	.j8m-n {
		font: 600 13px/1 var(--mono);
	}
	.j8m-of {
		font-weight: 400;
		color: var(--faint);
	}
	.j8m-bar {
		display: block;
		width: 44px;
		height: 4px;
		border-radius: 2px;
		background: var(--line-2);
		overflow: hidden;
	}
	.j8m-bar i {
		display: block;
		height: 100%;
		background: var(--ok);
	}
	.missing .j8m-bar i {
		background: var(--warn);
	}
	.j8m-flag {
		position: absolute;
		top: -7px;
		right: -6px;
		font-size: 11px;
		line-height: 1;
		padding: 2px 3px;
		border-radius: 4px;
		background: var(--panel);
		border: 1px solid color-mix(in srgb, var(--warn) 60%, var(--line));
		color: var(--warn);
	}
	.j8m-proc {
		font-size: 12px;
		white-space: nowrap;
	}
	.faint {
		color: var(--faint);
	}
	.j8m-v {
		margin-left: 6px;
	}
	@media (max-width: 860px) {
		/* Stacked (phone/tablet): the role cells sit side by side under the activity, each with its role name.
		   `.j8m.j8m` outranks phone-stack.css (Svelte scopes inner selectors with :where, adding no weight). */
		.j8m.j8m td.c[data-label] {
			flex: 0 0 auto;
			display: grid;
			justify-items: center;
			gap: 3px;
			padding-left: 0;
		}
		.j8m.j8m td.c[data-label]::before {
			position: static;
			width: auto;
			text-align: center;
		}
		.j8m-act {
			min-width: 0;
		}
		.j8m-proc {
			white-space: normal;
		}
	}
	.j8m-legend {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 16px;
		margin: 8px 14px 12px;
		font-size: 12px;
		color: var(--muted);
	}
	.sw {
		display: inline-block;
		width: 14px;
		height: 4px;
		border-radius: 2px;
		margin-right: 5px;
		vertical-align: middle;
	}
	.sw.ok {
		background: var(--ok);
	}
	.sw.warn {
		background: var(--warn);
	}
</style>
