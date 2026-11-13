<script lang="ts">
	// D59: the languages a job serves — a JOB setting (not a selection, not a region). Feeds base gate B6 (which
	// languages the agent accepts), the reply language and the knowledge coverage check. D54: saved to the draft;
	// the Test environment (test chat) runs the draft, Production keeps the live list until a release publishes it.
	import { onMount } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import type { JobLanguagesView } from '$lib/api/types/release';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';

	let { onchange }: { onchange: () => void } = $props();

	const res = new Resource<JobLanguagesView>(
		() => api().jobLanguages(),
		() => false,
	);
	onMount(() => res.refresh());
	const wa = $derived(writeAttrs(session, 'job_designer'));

	/** The edited list (null = untouched, shows the draft). */
	let picked = $state<string[] | null>(null);
	let busy = $state(false);
	const KNOW: Record<string, { text: string; variant: 'ok' | 'warn' | 'err' }> = {
		full: { text: 'knowledge: full', variant: 'ok' },
		partial: { text: 'knowledge: partial', variant: 'warn' },
		none: { text: 'knowledge: none', variant: 'err' },
	};

	function toggle(cur: string[], k: string, on: boolean): string[] {
		return on ? [...cur, k] : cur.filter((x) => x !== k);
	}
	function makeDefault(cur: string[], k: string): string[] {
		return [k, ...cur.filter((x) => x !== k)];
	}
	async function save(list: string[]) {
		if (busy) return;
		busy = true;
		const ok = await guard(
			() => api().saveJobLanguages(list),
			'Saved to draft. Test it in test chat; it goes live with the next release.',
			'Not saved',
		);
		busy = false;
		if (ok) {
			picked = null;
			await res.refresh();
			onchange();
		}
	}
</script>

<LoadState resource={res}>
	{#snippet children(v)}
		{@const cur = picked ?? v.draft}
		{@const dirty = picked !== null && JSON.stringify(picked) !== JSON.stringify(v.draft)}
		<Card
			title="Languages served"
			sub="A job setting. Feeds gate B6 (which languages the agent accepts), the reply language and the knowledge coverage check."
		>
			{#snippet actions()}
				{#if v.unpublished}<Chip text="Draft · not live" variant="accent" />{:else}<Chip
						text="Live"
						variant="ok"
					/>{/if}
			{/snippet}
			<div class="langs" data-testid="job-languages">
				<div class="lv-row">
					<span class="lv-label">Live (Production)</span>
					<span class="mono">{v.live.join(', ') || 'none'}</span>
				</div>
				{#if v.unpublished}
					<div class="lv-row">
						<span class="lv-label">Draft (test chat)</span>
						<span class="mono" data-testid="languages-draft">{v.draft.join(', ')}</span>
					</div>
				{/if}
				<ul class="opts">
					{#each v.options as o (o.key)}
						{@const on = cur.includes(o.key)}
						<li class="opt" class:on>
							<label
								><input
									type="checkbox"
									checked={on}
									disabled={wa.disabled || busy}
									onchange={(e) => (picked = toggle(cur, o.key, e.currentTarget.checked))}
								/>
								<b>{o.label}</b> <span class="mono muted">{o.key}</span></label
							>
							<Chip text={KNOW[o.knowledge]!.text} variant={KNOW[o.knowledge]!.variant} />
							{#if on && cur[0] === o.key}
								<span class="def">default reply language</span>
							{:else if on}
								<button
									type="button"
									class="table-action"
									{...wa}
									onclick={() => (picked = makeDefault(cur, o.key))}>Make default</button
								>
							{/if}
						</li>
					{/each}
				</ul>
				{#if cur.some((k) => v.options.find((o) => o.key === k)?.knowledge === 'none')}
					<p class="warn small">
						A language without knowledge documents will answer from the template only — add sources in that
						language first.
					</p>
				{/if}
				<div class="row">
					<button
						type="button"
						class="btn primary sm"
						{...wa}
						disabled={wa.disabled || busy || !dirty || !cur.length}
						onclick={() => save(cur)}>{busy ? 'Saving…' : 'Save to draft'}</button
					>
					{#if dirty}<button type="button" class="btn sm" onclick={() => (picked = null)}>Cancel</button>{/if}
					<a class="small" href="/chat">Test in test chat</a>
				</div>
				<p class="muted small">
					Not a region: data residency is a tenant setting, and country law becomes a jurisdiction pack later
					(D59).
				</p>
			</div>
		</Card>
	{/snippet}
</LoadState>

<style>
	.langs {
		display: grid;
		gap: 8px;
	}
	.lv-row {
		display: flex;
		gap: 10px;
		align-items: baseline;
		font-size: 13px;
	}
	.lv-label {
		font: 600 10.5px/1.2 var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--muted);
		min-width: 120px;
	}
	.opts {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 4px;
	}
	.opt {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 10px;
		align-items: center;
		padding: 4px 8px;
		border: 1px solid var(--line);
		border-radius: 6px;
	}
	.opt.on {
		border-color: var(--accent);
	}
	.opt label {
		display: inline-flex;
		gap: 6px;
		align-items: center;
		min-width: 150px;
	}
	.def {
		font-size: 12px;
		color: var(--accent);
	}
	.row {
		display: flex;
		gap: 10px;
		align-items: center;
	}
	.small {
		font-size: 12px;
	}
	.warn {
		color: var(--warn);
		margin: 0;
	}
	p {
		margin: 0;
	}
</style>
