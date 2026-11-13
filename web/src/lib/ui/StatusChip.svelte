<script lang="ts">
	// The one place an agent's state is rendered: "Live", "Live · 1 urgent", "Draft"... Every page that shows an agent's
	// state shows this chip, fed by `agentStatus()`, so two pages can never disagree.
	import type { AgentStatus } from '$lib/api/types/status';
	import { STATUS_LABEL, type AgentState } from './labels';
	import Chip from './Chip.svelte';

	let { status }: { status: Pick<AgentStatus, 'state' | 'urgent'> } = $props();

	const VARIANT: Record<AgentState, 'ok' | 'warn' | 'err' | 'muted' | 'accent'> = {
		live: 'ok',
		live_with_issues: 'warn',
		paused: 'err',
		ready: 'accent',
		draft: 'muted',
	};
	const text = $derived(
		status.urgent > 0
			? `${STATUS_LABEL[status.state]} · ${status.urgent} urgent`
			: STATUS_LABEL[status.state],
	);
</script>

<span data-testid="status-chip" data-state={status.state}
	><Chip {text} variant={VARIANT[status.state]} /></span
>
