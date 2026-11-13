<script lang="ts">
	// Role-aware landing (07 §4.3): tenant callers → /admin; a super_admin with no tenant → Platform › Tenants.
	// Blocked callers stay here; the shell shows their blocked state.
	import { goto } from '$app/navigation';
	import { session } from '$lib/auth/session.svelte';
	import { app } from '$lib/app.svelte';
	import { landingFor } from '$lib/pages';

	$effect(() => {
		const target = app.ready && !app.blocked ? landingFor(session) : null;
		if (target) void goto(target, { replaceState: true });
	});
</script>

<p class="muted">Opening…</p>
