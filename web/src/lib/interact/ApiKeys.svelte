<script lang="ts">
	// B13 per-job API keys on "Use the API" (L4): machine callers authenticate to the job's interaction routes with
	// `Authorization: Bearer ak_…` (or `X-Api-Key`). Integrations managers list, create and revoke them; the secret is
	// shown once, right after create, and the snippets above use it until it is dismissed (`onsecret(null)`).
	// Without the role the section shows disabled with the hint (the list itself needs the role too).
	import { api, guard } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { canWrite, writeHint } from '$lib/auth/roles';
	import type { CreatedJobApiKey, JobApiKey } from '$lib/api/types/conversations';
	import { stamp } from '$lib/conversations/labels';
	import { toast } from '$lib/ui/toaster.svelte';

	let { job, onsecret }: { job: string; onsecret?: (secret: string | null) => void } = $props();

	const may = $derived(canWrite(session, 'integrations_manager'));
	const hint = $derived(may ? undefined : writeHint('integrations_manager'));
	const res = new Resource(() => api().jobApiKeys(job));
	$effect(() => {
		if (may && job) void res.refresh();
	});

	let label = $state('');
	/** `YYYY-MM-DD` from the date input; the key expires at the end of that day (local). Empty = never. */
	let expires = $state('');
	let saving = $state(false);
	let created = $state<CreatedJobApiKey | null>(null);
	let confirming = $state<string | null>(null);
	let busy = $state<string | null>(null);

	const STATE_CHIP: Record<JobApiKey['state'], string> = { active: 'ok', expired: 'warn', revoked: 'muted' };

	async function create() {
		const l = label.trim();
		if (!l || !may) return;
		const input: { label: string; expires_at?: string } = { label: l };
		if (expires) input.expires_at = new Date(`${expires}T23:59:59`).toISOString();
		saving = true;
		let made: CreatedJobApiKey | null = null;
		try {
			if (
				await guard(
					async () => {
						made = await api().createJobApiKey(job, input);
					},
					`API key “${l}” created. Copy the secret now: it is not shown again.`,
					'Not created',
				)
			) {
				created = made;
				onsecret?.((made as CreatedJobApiKey | null)?.secret ?? null);
				label = '';
				expires = '';
				await res.refresh();
			}
		} finally {
			saving = false;
		}
	}
	function dismiss() {
		created = null;
		onsecret?.(null);
	}
	async function revoke(k: JobApiKey) {
		busy = k.id;
		try {
			if (await guard(() => api().revokeJobApiKey(job, k.id), `“${k.label}” revoked.`, 'Not revoked')) {
				confirming = null;
				if (created?.key.id === k.id) dismiss();
				await res.refresh();
			}
		} finally {
			busy = null;
		}
	}
	async function copy(text: string) {
		try {
			await navigator.clipboard.writeText(text);
			toast('Secret copied.');
		} catch {
			toast('Copy failed: select the secret and copy it by hand.', 'err');
		}
	}
	const today = new Date().toISOString().slice(0, 10);
</script>

<section class="ak" data-testid="api-keys" aria-labelledby="ak-h">
	<h3 id="ak-h">API keys</h3>
	<p class="muted">
		For servers and scripts that call this job without a person signed in. Send the key as
		<code>Authorization: Bearer ak_…</code> (or <code>X-Api-Key</code>) on the interaction routes only; it
		works for this job alone.
	</p>
	{#if !may}
		<p class="muted small" data-testid="api-keys-role-note">{writeHint('integrations_manager')}</p>
	{/if}

	{#if created}
		<div class="ak-secret" role="status" data-testid="api-key-secret">
			<b>Secret of “{created.key.label}” — shown this once.</b>
			<code class="mono">{created.secret}</code>
			<span class="row">
				<button class="btn sm primary" type="button" onclick={() => copy(created!.secret)}>Copy secret</button
				>
				<button class="btn sm" type="button" data-testid="api-key-dismiss" onclick={dismiss}
					>I stored it</button
				>
			</span>
			<span class="muted small">The snippets on this page use it until you dismiss it.</span>
		</div>
	{/if}

	<form
		class="ak-form"
		onsubmit={(e) => {
			e.preventDefault();
			void create();
		}}
	>
		<label class="field"
			>Label
			<input
				class="input"
				bind:value={label}
				maxlength="120"
				placeholder="order-portal backend"
				data-testid="api-key-label"
				disabled={!may}
			/>
		</label>
		<label class="field"
			>Expires (optional)
			<input
				class="input"
				type="date"
				min={today}
				bind:value={expires}
				data-testid="api-key-expires"
				disabled={!may}
			/>
		</label>
		<button
			class="btn primary"
			type="submit"
			data-testid="api-key-create"
			disabled={!may || saving || !label.trim()}
			title={hint}>Create key</button
		>
	</form>

	{#if may}
		{#if res.value.state === 'loading'}
			<p class="muted small" role="status">Loading keys…</p>
		{:else if res.value.state === 'unavailable'}
			<p class="err small" role="alert">The keys could not be loaded: {res.value.reason}</p>
		{:else if res.value.state === 'empty'}
			<p class="muted small" data-testid="api-keys-empty">No API keys yet.</p>
		{:else}
			<div class="tbl-wrap">
				<table class="tbl" data-testid="api-key-list">
					<thead>
						<tr
							><th>Label</th><th>Key</th><th>Created</th><th>Last used</th><th>Expires</th><th>State</th><th
								aria-label="Key actions"
							></th></tr
						>
					</thead>
					<tbody>
						{#each res.value.data.items as k (k.id)}
							<tr data-testid="api-key-row" data-id={k.id} data-state={k.state}>
								<td><b>{k.label}</b></td>
								<td data-label="Key"><span class="mono">{k.prefix}…</span></td>
								<td data-label="Created">{stamp(k.created_at)}</td>
								<td data-label="Last used">{k.last_used_at ? stamp(k.last_used_at) : 'never'}</td>
								<td data-label="Expires">{k.expires_at ? stamp(k.expires_at) : 'never'}</td>
								<td data-label="State"
									><span class="chip {STATE_CHIP[k.state] ?? 'muted'}">{k.state}</span>{#if k.revoked_at}<span
											class="cell-sub">{stamp(k.revoked_at)}</span
										>{/if}</td
								>
								<td class="r">
									{#if k.state !== 'revoked'}
										{#if confirming === k.id}
											<span class="row" role="group" aria-label={`Revoke “${k.label}”?`}>
												<button
													class="btn sm danger"
													type="button"
													data-testid="api-key-revoke-confirm"
													disabled={busy === k.id}
													onclick={() => revoke(k)}>Revoke — callers get 401</button
												>
												<button class="btn sm" type="button" onclick={() => (confirming = null)}
													>Cancel</button
												>
											</span>
										{:else}
											<button
												class="btn sm danger"
												type="button"
												data-testid="api-key-revoke"
												onclick={() => (confirming = k.id)}>Revoke…</button
											>
										{/if}
									{/if}
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{/if}
	{/if}
</section>

<style>
	.ak {
		display: grid;
		gap: 8px;
		margin-bottom: 12px;
	}
	.ak-form {
		display: flex;
		flex-wrap: wrap;
		gap: 10px;
		align-items: flex-end;
	}
	.ak-secret {
		display: grid;
		gap: 6px;
		padding: 10px 12px;
		border-radius: 8px;
		background: color-mix(in srgb, var(--warn, #c80) 10%, transparent);
	}
	.ak-secret code {
		overflow-wrap: anywhere;
	}
	.err {
		color: var(--err);
	}
</style>
