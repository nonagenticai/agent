<script lang="ts">
	// Insert-only "add a value" form for one dimension (P13). The value is added for every tenant at once (D8).
	import { superAdminAttrs } from '$lib/auth/roles';
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import type { Dimension } from '$lib/api/types/platform';

	let { dimension, onadded }: { dimension: Dimension; onadded: () => void } = $props();
	let key = $state('');
	let label = $state('');

	async function add(e: SubmitEvent) {
		e.preventDefault();
		const d = dimension;
		const ok = await guard(
			() => api().addDimensionValue(d.key, { key, label }),
			`${d.label}: “${label}” added for every tenant. Values are insert-only.`,
		);
		if (ok) {
			key = '';
			label = '';
			onadded();
		}
	}
</script>

<form class="add" onsubmit={add}>
	<div class="add-head">
		<strong>Add a {dimension.label.toLowerCase()} value</strong>
		<span class="muted">Every tenant can pick it. The key can never be renamed or removed afterwards.</span>
	</div>
	<div class="row">
		<input
			class="input mono"
			aria-label={`New ${dimension.label} key`}
			placeholder="key, e.g. order-support"
			bind:value={key}
			required
		/>
		<input
			class="input"
			aria-label={`New ${dimension.label} label`}
			placeholder="Label"
			bind:value={label}
			required
		/>
		<button class="btn sm" type="submit" {...superAdminAttrs(session)}>Add value</button>
	</div>
</form>

<style>
	.add {
		display: grid;
		gap: 8px;
		margin-top: 12px;
		padding-top: 12px;
		border-top: 1px dashed var(--line);
	}
	.add-head {
		display: flex;
		flex-wrap: wrap;
		gap: 2px 10px;
		font-size: 12.5px;
	}
	.row {
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(0, 1.4fr) auto;
		gap: 8px;
		align-items: center;
	}
	@media (max-width: 560px) {
		.row {
			grid-template-columns: minmax(0, 1fr);
		}
		.row .btn {
			justify-self: start;
		}
	}
</style>
