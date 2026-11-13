<script lang="ts">
	// P7 version timeline: every immutable tag of one layer, oldest → newest, the viewed tag highlighted, each job's
	// pin plotted under the tag it is on, and the untagged revisions as a dashed "next" node.
	import type { TemplatePinRow, TemplateTag } from '$lib/api/types/model';
	import { tagHref } from './logic';
	import { when } from './ui';

	let {
		tags,
		viewed,
		pins,
		drafts,
	}: { tags: TemplateTag[]; viewed: number; pins: TemplatePinRow[]; drafts: number } = $props();

	const latest = $derived(tags.length ? tags[tags.length - 1]!.version : null);
	const pinsOn = (v: number) => pins.filter((p) => p.version === v);
	const day = (iso: string) => when(iso).split(',')[0];
</script>

<ol class="track scroll-x" aria-label="Tags of this layer, oldest first">
	{#each tags as t (t.version)}
		{@const on = pinsOn(t.version)}
		<li class="node" class:here={t.version === viewed} class:prov={t.provisional}>
			<a
				class="dot"
				href={tagHref(t.layer_key, t.version)}
				aria-current={t.version === viewed ? 'page' : undefined}
				title={t.note}
			>
				<span class="v mono">@v{t.version}</span>
			</a>
			<span class="meta">
				{day(t.tagged_at)} · {t.item_count} items
				{#if t.version === latest}<b class="lat">latest</b>{/if}
				{#if t.provisional}<b class="provl">provisional</b>{/if}
			</span>
			{#if on.length}
				<span class="pins">
					{#each on as p (p.tenant_id + p.job)}
						<a class="pin" class:off={p.tenant_status !== 'active'} href="/platform/tenants/{p.tenant_id}"
							>{p.tenant_name}</a
						>
					{/each}
				</span>
			{/if}
		</li>
	{/each}
	{#if drafts > 0}
		<li class="node next">
			<span class="dot"><span class="v mono">@v{(latest ?? 0) + 1}?</span></span>
			<span class="meta">{drafts} untagged revision{drafts === 1 ? '' : 's'}</span>
		</li>
	{/if}
</ol>

<style>
	.track {
		list-style: none;
		margin: 0;
		padding: 4px 0 2px;
		display: flex;
		gap: 0;
		overflow-x: auto;
	}
	.node {
		position: relative;
		flex: 1 0 112px;
		display: flex;
		flex-direction: column;
		align-items: flex-start;
		gap: 6px;
		padding-right: 12px;
	}
	/* The rail between nodes. */
	.node::before {
		content: '';
		position: absolute;
		top: 15px;
		left: 0;
		right: 0;
		border-top: 2px solid var(--line);
	}
	.node:last-child::before {
		right: auto;
		width: 40px;
	}
	.node.next::before {
		border-top-style: dashed;
	}
	.dot {
		position: relative;
		z-index: 1;
		display: inline-flex;
		align-items: center;
		height: 32px;
		padding: 0 12px;
		border-radius: 16px;
		border: 2px solid var(--line);
		background: var(--panel);
		color: var(--fg);
		text-decoration: none;
		font-weight: 600;
	}
	a.dot:hover {
		border-color: var(--accent);
	}
	.here .dot {
		background: var(--accent);
		border-color: var(--accent);
		color: var(--panel);
	}
	.prov .dot {
		border-style: dashed;
		border-color: var(--warn);
	}
	.next .dot {
		border-style: dashed;
		color: var(--muted);
		font-weight: 500;
	}
	.meta {
		font-size: 12px;
		color: var(--muted);
	}
	.meta b {
		font-weight: 600;
		margin-left: 4px;
	}
	.lat {
		color: var(--ok);
	}
	.provl {
		color: var(--warn);
	}
	.pins {
		display: flex;
		flex-direction: column;
		gap: 3px;
	}
	.pin {
		font-size: 12px;
		padding: 1px 7px;
		border-radius: 10px;
		background: var(--sunk);
		color: var(--fg);
		text-decoration: none;
		border-left: 3px solid var(--cust);
	}
	.pin.off {
		color: var(--muted);
		border-left-color: var(--line);
	}
</style>
