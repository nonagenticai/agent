<script lang="ts">
	// Job settings: the release's actor / stance beside the tenant's override (from cb
	// refinements/SettingsTable.svelte@45ee128). `null` override = the base value hidden. KH T4 (D108): the preamble is
	// the job's identity instruction, which tenants do not refine.
	import { onMount } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { writeAttrs } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import { fmtValue } from '$lib/release/refinements';
	import type { DraftChange, JobSettingsView } from '$lib/api/types/release';
	import { draftAt, settingDraftKey } from '$lib/release/J13-workspace';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import LoadState from '$lib/ui/LoadState.svelte';

	let { drafts: unpublished = [], onchange }: { drafts?: DraftChange[]; onchange: () => void } = $props();
	const KEYS = ['actor', 'stance'] as const;
	const HELP: Record<(typeof KEYS)[number], string> = {
		actor: 'Who the agent says it is',
		stance: 'Tone of voice',
	};
	const settings = new Resource<JobSettingsView>(
		() => api().jobSettingsView(),
		() => false,
	);
	onMount(() => settings.refresh());
	const wa = $derived(writeAttrs(session, 'job_designer'));
	let drafts = $state<Record<string, string>>({});
	let busy = $state(false);

	async function run(action: () => Promise<unknown>, msg: string) {
		if (busy) return;
		busy = true;
		const ok = await guard(action, msg, 'Not changed');
		busy = false;
		if (ok) {
			drafts = {};
			await settings.refresh();
			onchange();
		}
	}
	const has = (o: Record<string, unknown>, k: string) => Object.prototype.hasOwnProperty.call(o, k);
</script>

<Freshness resource={settings} />
<LoadState resource={settings}>
	{#snippet children(v)}
		<Card cls="flush">
			<div class="tbl-wrap">
				<table class="tbl" data-testid="job-settings">
					<thead>
						<tr
							><th>Setting</th><th>Base (release)</th><th>Yours</th><th>Draft · test chat runs</th><th
								aria-label="Actions"
							></th></tr
						>
					</thead>
					<tbody>
						{#each KEYS as k}
							{@const overridden = has(v.override, k)}
							{@const pending = draftAt(unpublished, settingDraftKey(k))}
							<tr data-key={k}>
								<td>
									<span class="mono">{k}</span>
									<div class="muted small">{HELP[k]}</div>
									{#if overridden}<Chip
											text={v.override[k] === null ? 'Hidden' : 'Overridden'}
											variant={v.override[k] === null ? 'warn' : 'accent'}
										/>{/if}
								</td>
								<td>{fmtValue(v.base[k])}</td>
								<td>
									<input
										class="input"
										aria-label={`Your ${k}`}
										disabled={wa.disabled || busy}
										title={wa.title}
										placeholder={overridden ? (v.override[k] === null ? 'Hidden' : '') : 'Uses the base'}
										value={drafts[k] ?? (overridden ? (v.override[k] ?? '') : '')}
										oninput={(e) => (drafts[k] = e.currentTarget.value)}
									/>
								</td>
								<td>
									{fmtValue(v.effective[k])}
									{#if pending}<div class="live-was">
											<Chip text="Draft" variant="accent" />
											<span class="muted small">Live: {pending.live ?? 'release value'}</span>
										</div>{/if}
								</td>
								<td class="r nowrap">
									<button
										class="btn sm primary"
										type="button"
										disabled={wa.disabled || busy || !(drafts[k] ?? '').trim()}
										title={wa.title}
										onclick={() =>
											run(
												() => api().saveJobSettingOverride({ [k]: drafts[k]!.trim() }),
												`Saved your ${k} to draft — live when you publish.`,
											)}>Save</button
									>
									<button
										class="btn sm"
										type="button"
										disabled={wa.disabled ||
											busy ||
											v.base[k] == null ||
											(overridden && v.override[k] === null)}
										title={wa.title ?? 'Hide the base value'}
										onclick={() =>
											run(
												() => api().saveJobSettingOverride({ [k]: null }),
												`Hid ${k}. Saved to draft — live when you publish.`,
											)}>Hide</button
									>
									<button
										class="btn sm"
										type="button"
										disabled={wa.disabled || busy || !overridden}
										title={wa.title}
										onclick={() =>
											run(
												() => api().revertJobSettingOverride(k),
												`Reverted ${k} to the base value. Saved to draft — live when you publish.`,
											)}>Revert</button
									>
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		</Card>
	{/snippet}
</LoadState>

<style>
	.small {
		font-size: 12px;
	}
	.live-was {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
		margin-top: 4px;
	}
	.nowrap {
		white-space: nowrap;
	}
</style>
