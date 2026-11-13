<script lang="ts">
	// P17 version lineage: every version of the key oldest → newest, how many tenant systems sit on each, and what
	// this version changed against the one before it (doc 10 §4.2 vendor pin; doc 11 §6.1 pinning rule 4).
	import type { TemplateStatus } from '$lib/api/types/connectors';
	import type { TemplateDiff } from '$lib/connectors/P17-diff';
	import Chip from '$lib/ui/Chip.svelte';

	let {
		tplKey,
		current,
		versions,
		pinCounts,
		diff,
		onpick,
	}: {
		tplKey: string;
		current: string;
		/** Newest first, as the API sends them. */
		versions: { version: string; status: TemplateStatus }[];
		pinCounts: Record<string, number> | null;
		diff: TemplateDiff | null;
		onpick: (op: string) => void;
	} = $props();

	const ordered = $derived([...versions].reverse());
	const changed = $derived(diff ? diff.ops.filter((o) => o.change !== 'same') : []);
	const STATUS_VARIANT: Record<TemplateStatus, 'ok' | 'muted' | 'dead'> = {
		active: 'ok',
		draft: 'muted',
		retired: 'dead',
	};
	const href = (ver: string) =>
		`/platform/connector-templates/${encodeURIComponent(tplKey)}/${encodeURIComponent(ver)}`;
</script>

<section class="p17-lineage" aria-label="Version lineage">
	<ol class="p17-track">
		{#each ordered as ver, i (ver.version)}
			<li class="p17-node" class:is-current={ver.version === current} class:is-draft={ver.status === 'draft'}>
				{#if i > 0}<span class="p17-link" aria-hidden="true"></span>{/if}
				<a href={href(ver.version)} aria-current={ver.version === current ? 'page' : undefined}>
					<span class="mono p17-ver">@{ver.version}</span>
					<Chip text={ver.status} variant={STATUS_VARIANT[ver.status]} />
					<span class="p17-pins">
						{#if pinCounts === null}—{:else}{pinCounts[ver.version] ?? 0}
							{(pinCounts[ver.version] ?? 0) === 1 ? 'system' : 'systems'}{/if}
					</span>
				</a>
			</li>
		{/each}
	</ol>

	{#if diff}
		<div class="p17-diffline" data-testid="p17-diff-summary">
			{#if diff.against === null}
				<p class="muted">First version of <span class="mono">{tplKey}</span> — nothing to compare against.</p>
			{:else}
				<p>
					<b>Changes vs @{diff.against}:</b>
					{#if diff.counts.added}<Chip text="+{diff.counts.added} op" variant="ok" />{/if}
					{#if diff.counts.removed}<Chip text="−{diff.counts.removed} op" variant="err" />{/if}
					<Chip text="{diff.counts.changed} changed" variant={diff.counts.changed ? 'work' : 'muted'} />
					<Chip text="{diff.counts.same} unchanged" variant="muted" />
					{#each diff.meta as m (m.label)}<Chip
							text={m.label}
							variant="tool"
							title="{m.before} → {m.after}"
						/>{/each}
				</p>
				{#if changed.length || diff.meta.length}
					<details class="p17-diffmore">
						<summary>Show every change</summary>
						{#if diff.meta.length}
							<div class="tbl-wrap">
								<table class="tbl dense p17-metatbl">
									<thead><tr><th>Template</th><th>@{diff.against}</th><th>@{current}</th></tr></thead>
									<tbody>
										{#each diff.meta as m (m.label)}
											<tr
												><td>{m.label}</td><td class="mono small">{m.before}</td><td class="mono small"
													>{m.after}</td
												></tr
											>
										{/each}
									</tbody>
								</table>
							</div>
						{/if}
						<ul class="p17-oplist">
							{#each changed as c (c.op)}
								<li>
									<button type="button" class="linkish mono" onclick={() => onpick(c.op)}>{c.op}</button>
									<Chip
										text={c.change}
										variant={c.change === 'added' ? 'ok' : c.change === 'removed' ? 'err' : 'work'}
									/>
									{#if c.details.length}
										<ul class="p17-details">
											{#each c.details as line, j (j)}<li class="mono small">{line}</li>{/each}
										</ul>
									{/if}
								</li>
							{/each}
						</ul>
					</details>
				{/if}
			{/if}
		</div>
	{/if}
</section>

<style>
	.p17-track {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 0;
		align-items: center;
	}
	.p17-node {
		display: flex;
		align-items: center;
	}
	.p17-link {
		display: inline-block;
		width: 28px;
		height: 2px;
		background: var(--line);
		margin: 0 4px;
	}
	.p17-node a {
		display: grid;
		gap: 4px;
		justify-items: start;
		padding: 8px 12px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
		color: var(--fg);
		text-decoration: none;
		min-width: 140px;
	}
	.p17-node.is-draft a {
		border-style: dashed;
	}
	.p17-node.is-current a {
		border-color: var(--accent);
		box-shadow: inset 0 0 0 1px var(--accent);
	}
	.p17-ver {
		font-weight: 600;
	}
	.p17-pins {
		font-size: 12px;
		color: var(--muted);
	}
	.p17-diffline {
		margin-top: 12px;
		padding-top: 10px;
		border-top: 1px solid var(--line-2);
	}
	.p17-diffline p {
		margin: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
	}
	/* long API paths / values wrap so the before→after table fits; .tbl-wrap catches anything wider */
	.p17-metatbl td {
		overflow-wrap: anywhere;
		min-width: 0;
	}
	.p17-diffmore summary {
		cursor: pointer;
		margin-top: 8px;
		color: var(--accent);
		font-size: 13px;
	}
	.p17-oplist {
		list-style: none;
		margin: 10px 0 0;
		padding: 0;
		display: grid;
		gap: 8px;
	}
	.p17-details {
		margin: 4px 0 0;
		padding-left: 18px;
		overflow-wrap: anywhere;
	}
	.small {
		font-size: 12px;
	}
	.linkish {
		background: none;
		border: 0;
		padding: 0;
		color: var(--accent);
		cursor: pointer;
	}
	@media (max-width: 560px) {
		.p17-link {
			width: 12px;
		}
		.p17-node a {
			min-width: 0;
		}
	}
</style>
