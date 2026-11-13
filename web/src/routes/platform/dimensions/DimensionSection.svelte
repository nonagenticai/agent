<script lang="ts">
	// P13: one dimension — what it does in a compile, and per value WHERE it is used (layers, context spaces, job
	// selections, frozen releases). Unused values stay listed: insert-only means they are never cleaned up (D8).
	import type { Dimension, DimensionValueUsage } from '$lib/api/types/platform';
	import '$lib/phone-stack.css';
	import Chip from '$lib/ui/Chip.svelte';
	import { utc } from '$lib/platform/ui';
	import { ROLE_TEXT, STATUS_CHIP, statusOf, type DimensionRole } from '$lib/platform/P13Dimensions';
	import AddValue from './AddValue.svelte';

	let {
		dim,
		role,
		usage,
		selectionKey,
		onadded,
		groupOf = null,
		title = null,
	}: {
		dim: Dimension;
		/** D60 (areas): value → the department(s) whose context spaces it groups. */
		groupOf?: Record<string, string[]> | null;
		/** Section heading override (defaults to the dimension label). */
		title?: string | null;
		role: DimensionRole | null;
		/** Null while usage is loading or unavailable: the values still list, the "used by" cells say so. */
		usage: DimensionValueUsage[] | null;
		selectionKey: string;
		onadded: () => void;
	} = $props();

	const use = (v: string) => usage?.find((u) => u.value === v) ?? null;
	const releaseTitle = (u: DimensionValueUsage) =>
		u.releases.map((r) => `${r.id} · ${r.tenant_name} · ${r.state}`).join('\n');
</script>

<section class="card dim" class:placeholder={dim.kind === 'placeholder'} aria-labelledby={`dim-h-${dim.key}`}>
	<header class="card-head">
		<div>
			<h2 id={`dim-h-${dim.key}`}>{title ?? dim.label}</h2>
			<p>{dim.description}</p>
		</div>
		<div class="actions"><Chip text={`${dim.values.length} values`} /></div>
	</header>
	<div class="card-body">
		{#if role}
			<p class="role" data-testid={`dim-role-${dim.key}`}>
				<strong>{ROLE_TEXT[role].title}.</strong>
				{ROLE_TEXT[role].text}
				{#if selectionKey !== dim.key}
					<span class="muted">Stored in a selection as <code>{selectionKey}</code>.</span>
				{/if}
			</p>
		{/if}
		<div class="tbl-wrap ph-stack">
			<table class="tbl" data-testid={`dim-${dim.key}`}>
				<thead>
					<tr>
						<th scope="col">Value</th>
						<th scope="col">Used by</th>
						<th scope="col">Releases</th>
						<th scope="col">Added</th>
					</tr>
				</thead>
				<tbody>
					{#each dim.values as v (v.key)}
						{@const u = use(v.key)}
						<tr data-testid="dim-value" data-value={v.key}>
							<td>
								<div class="cell-title">{v.label}</div>
								<div class="cell-sub mono">{v.key}</div>
								{#if v.note}<div class="cell-sub">{v.note}</div>{/if}
								{#if groupOf?.[v.key]?.length}<div class="cell-sub in-dept">
										in {groupOf[v.key]!.join(', ')}
									</div>{/if}
							</td>
							<td data-label="Used by"
								><div class="used">
									{#if !u}
										<span class="muted">—</span>
									{:else}
										{@const st = STATUS_CHIP[statusOf(u)]}
										<Chip text={st.text} variant={st.variant} />
										{#each u.selections as s (s.tenant_id + s.job)}
											<a class="ref" href={`/platform/tenants/${s.tenant_id}`}
												>{s.tenant_name} · {s.job_label}</a
											>
										{/each}
										{#each u.layers as l (l.id)}
											<a
												class="ref mono layer"
												href={l.row_id
													? `/platform/layers?row=${encodeURIComponent(l.row_id)}`
													: '/platform/layers'}
												title={`Adds ${l.adds.join(', ')}`}>{l.path}</a
											>
										{/each}
										{#if u.context_spaces.length}
											<span class="ref muted" title={u.context_spaces.join('\n')}
												>{u.context_spaces.length} context space{u.context_spaces.length === 1
													? ''
													: 's'}</span
											>
										{/if}
										{#if statusOf(u) === 'unused'}
											<span class="ref muted">kept for every tenant to pick</span>
										{/if}
									{/if}
								</div></td
							>
							<td class="nowrap" data-label="Releases">
								{#if u && u.releases.length}
									<span title={releaseTitle(u)}>{u.releases.length} frozen</span>
								{:else if u}
									<span class="muted">none</span>
								{:else}
									<span class="muted">—</span>
								{/if}
							</td>
							<td class="nowrap muted mono small" data-label="Added">{utc(v.added_at)}<br />{v.added_by}</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
		<AddValue dimension={dim} {onadded} />
	</div>
</section>

<style>
	.placeholder {
		border-style: dashed;
	}
	.in-dept {
		color: var(--accent);
	}
	.role {
		margin: 0 0 10px;
		font-size: 13px;
	}
	.used {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 8px;
		align-items: center;
		min-width: 220px;
	}
	.ref {
		font-size: 12.5px;
	}
	.layer {
		font-size: 11.5px;
	}
	.small {
		font-size: 11.5px;
	}
</style>
