<script lang="ts">
	// D88 role-set editor: one checkbox per §R.1 role from `GET /api/roles` (R.1 order), "Observer" when none is
	// ticked. Used by the Members page (invite, add by ID, change roles) and the header "View as…" switcher, which
	// passes `only` = the caller's held roles (view-as can only narrow).
	import type { Role, RoleInfo } from '$lib/api/types/core';
	import { sortRoles } from '$lib/api/types/core';

	let {
		catalog,
		value = $bindable([]),
		disabled = false,
		only = null,
		locked = [],
		lockedHint = '',
		legend = 'Roles',
		name,
		compact = false,
	}: {
		catalog: RoleInfo[];
		value?: Role[];
		disabled?: boolean;
		/** Only these roles can be ticked (the others are listed, disabled). */
		only?: readonly Role[] | null;
		/** Ticked roles that cannot be unticked (the last tenant admin's `tenant_admin`). */
		locked?: readonly Role[];
		lockedHint?: string;
		legend?: string;
		/** Unique per editor on a page (input ids). */
		name: string;
		compact?: boolean;
	} = $props();

	function toggle(r: Role, on: boolean) {
		value = sortRoles(on ? [...value, r] : value.filter((x) => x !== r));
	}
</script>

<fieldset class="rse" class:compact {disabled} data-testid="role-set-{name}">
	<legend class="label"
		>{legend}
		{#if value.length === 0}<span class="rse-observer" data-testid="role-set-observer"
				>Observer — reads everything, changes nothing</span
			>{/if}</legend
	>
	{#each catalog as r (r.key)}
		{@const off =
			(only !== null && !only.includes(r.key)) || (locked.includes(r.key) && value.includes(r.key))}
		<label
			class="rse-opt"
			class:on={value.includes(r.key)}
			class:off
			title={locked.includes(r.key) && value.includes(r.key) ? lockedHint : undefined}
		>
			<input
				type="checkbox"
				name="{name}-{r.key}"
				value={r.key}
				checked={value.includes(r.key)}
				disabled={off}
				onchange={(e) => toggle(r.key, e.currentTarget.checked)}
			/>
			<span class="rse-text"
				><b>{r.label}</b>{#if !compact}<span class="cell-sub">{r.description}</span>{/if}</span
			>
		</label>
	{/each}
</fieldset>

<style>
	.rse {
		border: 0;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 4px;
		min-width: 0;
	}
	.rse legend {
		padding: 0;
		margin-bottom: 4px;
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: baseline;
	}
	.rse-observer {
		font-weight: 400;
		color: var(--muted);
		font-size: 12px;
	}
	.rse-opt {
		display: flex;
		gap: 8px;
		align-items: flex-start;
		padding: 5px 8px;
		border: 1px solid var(--line);
		border-radius: 6px;
		cursor: pointer;
		min-width: 0;
	}
	.rse-opt.on {
		border-color: color-mix(in srgb, var(--accent) 45%, var(--line));
		background: color-mix(in srgb, var(--accent) 6%, var(--panel));
	}
	.rse-opt.off {
		opacity: 0.6;
		cursor: not-allowed;
	}
	.rse-opt input {
		margin-top: 2px;
		flex: none;
	}
	.rse-text {
		display: grid;
		gap: 1px;
		min-width: 0;
	}
	.rse.compact {
		grid-template-columns: repeat(auto-fit, minmax(170px, 1fr));
	}
	.rse.compact legend {
		grid-column: 1 / -1;
	}
</style>
