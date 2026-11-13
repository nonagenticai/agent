<script lang="ts">
	// A3 on a job: which of the tenant's identity procedures this job uses. Attaching makes the procedure's tools
	// available to this job's declared inputs; it is saved to the draft with the rest of the job's design.
	import { onMount } from 'svelte';
	import { api, app } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite, writeHint } from '$lib/auth/roles';
	import { Resource } from '$lib/load.svelte';
	import type { IamProcedure } from '$lib/api/types/iam';
	import { toast } from '$lib/ui/toaster.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import { explainCode } from './policy';

	let { onchanged = () => {} }: { onchanged?: () => void } = $props();

	interface Data {
		procedures: IamProcedure[];
		attached: string[];
	}
	const res = new Resource<Data>(async () => {
		const [procedures, attached] = await Promise.all([api().iamProcedures(), api().jobIamProcedures()]);
		return { procedures, attached };
	});
	onMount(() => void res.refresh());
	const writable = $derived(canWrite(session, 'integrations_manager'));
	let busy = $state<string | null>(null);
	let err = $state<string | null>(null);

	async function toggle(p: IamProcedure, on: boolean) {
		busy = p.id;
		err = null;
		try {
			if (on) await api().attachIamProcedure(p.id);
			else await api().detachIamProcedure(p.id);
			toast(`${p.name} ${on ? 'attached to' : 'detached from'} the draft.`);
			onchanged();
			await res.refresh();
		} catch (e) {
			const x = e as { code?: string; message: string };
			err = explainCode(x.code, x.message);
		} finally {
			busy = null;
		}
	}
</script>

<Card
	title="Identity procedures"
	sub="Reusable sign-in and lookup chains your company defined. Attach the ones this job uses."
>
	<div class="stack" data-testid="attach-procedures">
		{#if res.value.state === 'ok' || res.value.state === 'empty'}
			{@const d = res.value.data}
			{#each d.procedures as p (p.id)}
				<label class="opt inline" data-testid="attach-{p.id}">
					<input
						type="checkbox"
						checked={d.attached.includes(p.id)}
						disabled={!writable || busy !== null}
						onchange={(e) => toggle(p, e.currentTarget.checked)}
					/>
					<b>{p.name}</b>
					<span class="muted small">
						{p.strength === 'verified' ? 'Verified' : 'Identified'} ·
						{p.description || `${p.steps.length} steps`}
					</span>
				</label>
			{:else}
				<p class="muted small">
					Your company has no identity procedures yet. <a href="/admin/identity">Create one</a>.
				</p>
			{/each}
			{#if !writable}<p class="note">{writeHint('integrations_manager')} to attach or detach.</p>{/if}
		{:else if res.value.state === 'unavailable'}
			<p class="muted small">Identity procedures could not be loaded for {app.job}: {res.value.reason}.</p>
		{:else}
			<p class="muted small">Loading…</p>
		{/if}
		{#if err}<Callout kind="err" title="Not changed"><span role="alert">{err}</span></Callout>{/if}
	</div>
</Card>
