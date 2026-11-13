<script lang="ts">
	// A T3 contract result per (instance, op, env): green / red / write unverified / needs your OK / not run.
	import Chip from '$lib/ui/Chip.svelte';
	import type { ContractResult } from '$lib/api/types/connectors';
	let { outcome, code = null }: { outcome: ContractResult['outcome']; code?: string | null } = $props();
	const MAP = {
		green: ['green', 'ok'],
		red: ['red', 'err'],
		write_unverified: ['write unverified', 'warn'],
		needs_ok: ['needs your OK', 'accent'],
		not_run: ['not run', 'muted'],
	} as const;
	const m = $derived(MAP[outcome]);
</script>

<Chip text={code && outcome === 'red' ? `red · ${code}` : m[0]} variant={m[1]} />
