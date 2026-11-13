<script lang="ts">
	// From cb:web/src/routes/console/AgentStrip.svelte@45ee128: the runtime's agents (doc 12 §2.1, §3.1).
	import type { AgentCard } from '$lib/live/console/bus';
	let {
		cards,
		selected,
		onselect,
	}: { cards: AgentCard[]; selected: string | null; onselect: (id: string) => void } = $props();
	const GROUPS = [
		{ kind: 'front', label: 'Front · edge' },
		{ kind: 'coordination', label: 'Coordination' },
		{ kind: 'runner', label: 'Operations' },
		{ kind: 'gate', label: 'Gates' },
	] as const;
</script>

<section class="panel agents" aria-label="Agents">
	{#each GROUPS as g (g.kind)}
		<div class="group">
			<span class="label">{g.label}</span>
			<div>
				{#each cards.filter((c) => c.kind === g.kind) as a (a.id)}
					<button
						type="button"
						class="agent"
						class:sel={selected === a.id}
						data-s={a.status}
						data-agent={a.id}
						title={a.line}
						onclick={() => onselect(a.id)}
					>
						<span class="dot"></span>
						<span class="aname">{a.name}</span>
						<span class="pill">{a.status}</span>
						<span class="aline">{a.line}</span>
					</button>
				{/each}
			</div>
		</div>
	{/each}
</section>
