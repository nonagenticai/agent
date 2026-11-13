<script lang="ts">
	// P20: one tenant's licence end to end — what it is, which kid seals it, what the edge does with it, who else
	// reads it (metering attribution, minted bundle tokens), and the per-tenant reseal.
	import type { LicensingOverview } from '$lib/api/types/platform';
	import { superAdminAttrs } from '$lib/auth/roles';
	import { session } from '$lib/auth/session.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import KeyValues from '$lib/ui/KeyValues.svelte';
	import { age, utc } from '$lib/platform/ui';
	import { edgeVerdict, kidPhase } from '$lib/platform/licensing-P20';

	type Row = LicensingOverview['tenants'][number];
	let {
		open = $bindable(false),
		row,
		overview,
		busy,
		onreseal,
	}: {
		open?: boolean;
		row: Row | null;
		overview: LicensingOverview;
		busy: boolean;
		onreseal: (id: string) => void;
	} = $props();

	const e = $derived(edgeVerdict(row?.verdict ?? null));
	const key = $derived(
		row?.licence?.kid ? overview.keyring.find((k) => k.kid === row?.licence?.kid) : undefined,
	);
</script>

<Drawer bind:open title={row ? `Licence · ${row.name}` : 'Licence'}>
	{#if row}
		<div class="ld" data-testid="licence-drawer">
			<section>
				<span class="label">At the edge</span>
				<p class="ld-edge"><Chip text={e.label} variant={e.variant} /> {e.why}</p>
				{#if row.verdict}
					<p class="muted ld-small">
						Row checked {utc(row.verdict.checked_at)} UTC · {age(row.verdict.age_seconds)} old
					</p>
				{/if}
			</section>

			{#if row.licence}
				<section>
					<span class="label">Licence</span>
					<KeyValues
						pairs={[
							['Licence id', row.licence.licence_id],
							['App · plan', `${row.licence.app} · ${row.licence.plan}`],
							['Subject id', row.licence.subject_id],
							['Holder', row.licence.holder_sub],
							['Issued', `${utc(row.licence.issued_at)} UTC`],
							['Introspect', row.licence.introspect_status],
							['Expiry', 'long-lived (no expiry; ends by revoke)'],
						]}
					/>
				</section>

				<section>
					<span class="label">Sealed under</span>
					{#if row.licence.kid}
						<p class="ld-edge">
							<span class="mono">v2:{row.licence.kid}:••••</span>
							{#if key}<Chip
									text={kidPhase(key)}
									variant={kidPhase(key) === 'current' ? 'ok' : 'warn'}
								/>{/if}
						</p>
						{#if row.licence.kid !== overview.current_kid}
							<Callout kind="gap" title="Not on the current key">
								{row.licence.kid} cannot be dropped while this token is sealed under it.
							</Callout>
							<button
								class="btn primary sm"
								{...superAdminAttrs(session)}
								disabled={busy || !session.superAdmin || row.status === 'archived'}
								onclick={() => onreseal(row.tenant_id)}>Reseal this tenant to {overview.current_kid}</button
							>
						{:else}
							<p class="muted ld-small">On the current key — nothing to do.</p>
						{/if}
					{:else}
						<p class="muted ld-small">
							No stored token (revoked). Nothing is on the keyring for this tenant.
						</p>
					{/if}
				</section>

				<section>
					<span class="label">Who else reads this licence</span>
					<ul class="ld-list">
						<li>
							<strong>Metering</strong> — usage-ledger rows carry
							<span class="mono">{row.licence.licence_id}</span> +
							<span class="mono">{row.licence.subject_id}</span>
							(never the token). Billing emit {overview.metering_emit_enabled ? 'on' : 'off'}.
						</li>
						{#if overview.bundle}
							<li>
								<strong>Bundle {overview.bundle.key}</strong> —
								{overview.bundle.members
									.filter((m) => m.holding === 'minted')
									.map((m) => m.app)
									.join(', ')} tokens minted per call; stop when this licence is revoked.
							</li>
						{/if}
					</ul>
				</section>
			{:else}
				<p class="muted">No licence row for this tenant.</p>
			{/if}

			<p class="ld-links">
				<a href={`/platform/tenants/${row.tenant_id}`}>Open tenant</a>
				<a href="/platform/audit?action=license.reseal">Reseal audit</a>
			</p>
		</div>
	{/if}
</Drawer>

<style>
	.ld {
		display: grid;
		gap: 18px;
	}
	.ld section {
		display: grid;
		gap: 6px;
	}
	.ld-edge {
		margin: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
		font-size: 13px;
	}
	.ld-small {
		margin: 0;
		font-size: 12px;
	}
	.ld-list {
		margin: 0;
		padding-left: 18px;
		display: grid;
		gap: 6px;
		font-size: 13px;
	}
	.ld-links {
		display: flex;
		gap: 16px;
		margin: 0;
		font-size: 13px;
	}
</style>
