<script lang="ts">
	// P20: the keyring as a lifecycle (K43). Each kid sits in its phase; the action that moves it on sits in the
	// same column. "Dropped" is not a row — a dropped kid is simply gone from LICENSE_TOKEN_ENC_KEYS (SOPS).
	import type { LicensingOverview } from '$lib/api/types/platform';
	import { superAdminAttrs } from '$lib/auth/roles';
	import { session } from '$lib/auth/session.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import { utc } from '$lib/platform/ui';
	import { kidPhase, tenantsOnKid, type KidPhase } from '$lib/platform/licensing-P20';

	let {
		overview,
		busy,
		onreseal,
	}: {
		overview: LicensingOverview;
		busy: boolean;
		onreseal: () => void;
	} = $props();

	const PHASES: { key: KidPhase; label: string; hint: string }[] = [
		{ key: 'current', label: 'Current', hint: 'Seals every new or resealed token.' },
		{
			key: 'retiring',
			label: 'Retiring',
			hint: 'Still opens tokens sealed under it. Reseal moves them off.',
		},
		{ key: 'drop-ready', label: 'Drop-ready', hint: '0 tokens left. Remove it from LICENSE_TOKEN_ENC_KEYS.' },
	];
	const byPhase = $derived(
		Object.fromEntries(
			PHASES.map((p) => [p.key, overview.keyring.filter((k) => kidPhase(k) === p.key)]),
		) as Record<KidPhase, LicensingOverview['keyring']>,
	);
</script>

<div class="kl" role="list" aria-label="Key rotation lifecycle">
	<section class="kl-col" role="listitem">
		<header><span class="kl-step">1</span><span class="label">Next</span></header>
		<p class="kl-hint">
			Add a kid to <code>LICENSE_TOKEN_ENC_KEYS</code> (SOPS) and point the current key at it.
		</p>
		<div class="kl-slot">
			<span class="muted">No key staged.</span>
		</div>
	</section>
	{#each PHASES as p, i (p.key)}
		<section class="kl-col" role="listitem">
			<header><span class="kl-step">{i + 2}</span><span class="label">{p.label}</span></header>
			<p class="kl-hint">{p.hint}</p>
			{#each byPhase[p.key] as k (k.kid)}
				{@const names = tenantsOnKid(overview, k.kid)}
				<article class="kl-kid {p.key}" data-testid="kid-row">
					<div class="kl-top">
						<strong class="mono">{k.kid}</strong>
						{#if k.current}<Chip text="current" variant="ok" />{/if}
						<span class="kl-added mono">added {utc(k.added_at)}</span>
					</div>
					<div class="kl-count">
						<b>{k.sealed}</b> token{k.sealed === 1 ? '' : 's'} sealed
					</div>
					{#if names.length}
						<div class="chips">
							{#each names as n (n)}<Chip text={n} variant={p.key === 'retiring' ? 'warn' : 'muted'} />{/each}
						</div>
					{/if}
					{#if p.key === 'drop-ready'}
						<Chip text="0 remaining · safe to drop" variant="ok" />
					{:else if p.key === 'retiring'}
						<Chip text={`${k.sealed} still sealed — reseal first`} variant="warn" />
						<button
							class="btn primary sm"
							{...superAdminAttrs(session)}
							disabled={busy || !session.superAdmin}
							onclick={onreseal}>Move {k.sealed} to {overview.current_kid}</button
						>
					{/if}
				</article>
			{:else}
				<div class="kl-slot"><span class="muted">—</span></div>
			{/each}
		</section>
	{/each}
</div>

<style>
	.kl {
		display: grid;
		/* reflows 4 → 2 → 1 columns by available width (sidebar-aware), never sideways */
		grid-template-columns: repeat(auto-fit, minmax(min(100%, 220px), 1fr));
		gap: 10px;
		margin: 0 0 12px;
	}
	.kl-col {
		position: relative;
		display: grid;
		align-content: start;
		grid-template-columns: minmax(0, 1fr);
		gap: 8px;
		min-width: 0;
		padding: 10px;
		border-radius: 8px;
		background: var(--sunk);
	}
	.kl-col header {
		display: flex;
		align-items: center;
		gap: 6px;
	}
	.kl-step {
		display: inline-grid;
		place-items: center;
		width: 18px;
		height: 18px;
		border-radius: 50%;
		background: var(--panel);
		border: 1px solid var(--line);
		font: 600 11px/1 var(--mono);
	}
	.kl-hint {
		margin: 0;
		overflow-wrap: anywhere;
		font-size: 12px;
		color: var(--muted);
	}
	.kl-slot {
		border: 1px dashed var(--line);
		border-radius: 6px;
		padding: 10px;
		display: grid;
		gap: 8px;
		justify-items: start;
		font-size: 12px;
	}
	.kl-kid {
		background: var(--panel);
		border: 1px solid var(--line);
		border-left: 4px solid var(--muted);
		border-radius: 6px;
		padding: 10px;
		display: grid;
		gap: 6px;
		justify-items: start;
		min-width: 0;
		font-size: 13px;
		overflow-wrap: anywhere;
	}
	.kl-kid :global(.chip),
	.kl-slot :global(.btn) {
		white-space: normal;
		max-width: 100%;
		line-height: 1.3;
		text-align: left;
	}
	.kl-kid.current {
		border-left-color: var(--ok);
	}
	.kl-kid.retiring {
		border-left-color: var(--warn);
	}
	.kl-kid.drop-ready {
		border-left-color: var(--faint);
	}
	.kl-top {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 6px;
	}
	.kl-top strong {
		font-size: 15px;
	}
	.kl-added {
		color: var(--muted);
		font-size: 11px;
	}
	.kl-count b {
		font: 600 18px var(--cond);
		font-variant-numeric: tabular-nums;
	}
</style>
