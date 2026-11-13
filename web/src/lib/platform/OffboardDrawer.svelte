<script lang="ts">
	// Offboard confirmation (04 §7b): the exact tenant name must be typed before anything is posted.
	// From sc:web/src/routes/tenants/+page.svelte@71aa7af (`<dialog>` → cb Drawer).
	import { api } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { ApiError } from '$lib/api/errors';
	import type { OffboardResult } from '$lib/api/types/platform';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import MilestoneChip from '$lib/ui/MilestoneChip.svelte';
	import { toast } from '$lib/ui/toaster.svelte';

	let {
		open = $bindable(false),
		tenant,
		ondone,
	}: {
		open?: boolean;
		tenant: { id: string; name: string } | null;
		ondone: (result: OffboardResult, name: string) => void;
	} = $props();

	let typed = $state('');
	let saving = $state(false);
	let error = $state<{ code: string; message: string } | null>(null);
	const confirmed = $derived(tenant !== null && typed.trim() === tenant.name);

	$effect(() => {
		if (open) {
			typed = '';
			error = null;
		}
	});

	async function submit(e: SubmitEvent) {
		e.preventDefault();
		if (!tenant || !confirmed) return;
		const t = tenant;
		saving = true;
		error = null;
		try {
			const result = await api().offboardTenant(t.id, typed);
			toast(`Offboarded ${t.name}.`);
			open = false;
			ondone(result, t.name);
		} catch (err) {
			error =
				err instanceof ApiError
					? { code: err.code, message: err.message }
					: { code: 'error', message: (err as Error).message };
		} finally {
			saving = false;
		}
	}
</script>

<Drawer bind:open title={tenant ? `Offboard ${tenant.name}` : 'Offboard'} dirty={!!typed}>
	{#snippet sub()}<MilestoneChip milestone="M2" />{/snippet}
	{#if tenant}
		<form class="stack" onsubmit={submit}>
			<Callout kind="err" title="This cannot be undone">
				Revokes the agent licence first, then archives the workspace and disables every user in it. Agents
				stop serving it; open cases are cancelled at their next step. Logins (shared with other apps) are
				kept; knowledge and ingest data are retained by strata.
			</Callout>
			<div class="field">
				<label for="confirm-name">Type <strong>{tenant.name}</strong> to confirm</label>
				<input id="confirm-name" class="input" bind:value={typed} autocomplete="off" required />
			</div>
			{#if error}
				<Callout kind="err" title={error.code}>{error.message}</Callout>
			{/if}
			<div class="actions">
				<button class="btn danger" type="submit" disabled={!confirmed || saving || !session.superAdmin}
					>{saving ? 'Offboarding…' : 'Offboard this tenant'}</button
				>
			</div>
		</form>
	{/if}
</Drawer>
