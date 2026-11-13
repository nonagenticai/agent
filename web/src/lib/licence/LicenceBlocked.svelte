<script lang="ts">
	// The blocking licence state the shell shows instead of the page (licence gate 2). The topbar stays, so who-am-I
	// and Sign out remain reachable; Retry re-asks `GET /api/license/status`.
	import type { LicenceCode } from '$lib/api/errors';
	import { LICENCE_TEXT, LICENCE_TITLE, retryHelps } from './text';

	let {
		code,
		tenantName = null,
		retry,
		backToPlatform,
	}: {
		code: LicenceCode;
		tenantName?: string | null;
		/** Resolves true when the tenant is admitted again (the shell then remounts the page). */
		retry: () => Promise<boolean>;
		/** A super_admin's way out to the platform shell. */
		backToPlatform?: () => void;
	} = $props();

	let busy = $state(false);
	let stillBlocked = $state(false);

	async function onRetry() {
		busy = true;
		stillBlocked = false;
		try {
			stillBlocked = !(await retry());
		} catch {
			stillBlocked = true;
		} finally {
			busy = false;
		}
	}
</script>

<div class="callout err" role="alert" data-testid="blocked-licence" data-code={code}>
	<strong>{LICENCE_TITLE}</strong>
	{#if tenantName}<span class="muted">{tenantName}</span>{/if}
	<span>{LICENCE_TEXT[code]}</span>
	<span class="muted">Code: <code>{code}</code></span>
</div>
<div class="row">
	<button
		class="btn {retryHelps(code) ? 'primary' : ''}"
		type="button"
		data-testid="licence-retry"
		disabled={busy}
		onclick={() => onRetry()}>{busy ? 'Checking…' : 'Try again'}</button
	>
	{#if backToPlatform}
		<button class="btn" type="button" onclick={() => backToPlatform?.()}>Back to platform</button>
	{/if}
</div>
{#if stillBlocked && !busy}
	<p class="muted" role="status" data-testid="licence-still-blocked">
		Still not active. Nothing was changed.
	</p>
{/if}
