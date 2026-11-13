<script lang="ts">
	// Which build milestone delivers a page or control (spec 14 §2 "M"): M1 merged app, M2 licensing/invites/
	// signup/edge, M3 provision-down, M4 object model/connectors/gates/generator. Hidden when the topbar toggle
	// "Show build milestones" is off.
	import type { Milestone } from '$lib/api/types/core';
	import { prefs } from './prefs.svelte';

	let { milestone, note }: { milestone: Milestone; note?: string } = $props();
	const VARIANT: Record<Milestone, string> = { M1: 'ok', M2: 'cust', M3: 'tool', M4: 'work' };
	const TITLE: Record<Milestone, string> = {
		M1: 'M1 — merged app',
		M2: 'M2 — licensing, invites, signup, edge',
		M3: 'M3 — provision-down',
		M4: 'M4 — object model, connectors, gates, generator',
	};
</script>

{#if prefs.showMilestones}
	<span class="chip {VARIANT[milestone]} milestone" title={TITLE[milestone]} data-testid="milestone"
		>{note ?? milestone}</span
	>
{/if}

<style>
	.milestone {
		vertical-align: middle;
		font-family: var(--mono);
		font-size: 11px;
	}
</style>
