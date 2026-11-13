<script lang="ts">
	// A contract-test tier chip (T1 recorded fixtures, T2 live sandbox; doc 10 §4.5).
	import Chip from '$lib/ui/Chip.svelte';
	import type { TierResult } from '$lib/api/types/connectors';
	let { tier, result }: { tier: 'T1' | 'T2'; result: TierResult } = $props();
	const variant = $derived(
		result.status === 'green'
			? 'ok'
			: result.status === 'red'
				? 'err'
				: result.status === 'drift'
					? 'warn'
					: 'muted',
	);
	const word = $derived(result.status === 'not_run' ? 'not run' : result.status);
	const title = $derived(
		[result.at ? `${tier} ${result.at.slice(0, 16).replace('T', ' ')} UTC` : `${tier} never run`, result.note]
			.filter(Boolean)
			.join(' · '),
	);
</script>

<Chip text="{tier} {word}" {variant} {title} />
