<script lang="ts" module>
	export type StepStatus = 'done' | 'current' | 'todo' | 'missing' | 'placeholder' | 'blocked';
	export interface Step {
		key: string;
		label: string;
		status: StepStatus;
		/** One line under the label (what is missing, what the step produced). */
		detail?: string;
		/** Click-through to the failing item. */
		href?: string;
	}
	const CHIP: Record<StepStatus, [string, string]> = {
		done: ['Done', 'ok'],
		current: ['In progress', 'accent'],
		todo: ['To do', 'muted'],
		missing: ['Missing', 'err'],
		placeholder: ['Placeholder', 'warn'],
		blocked: ['Blocked', 'err'],
	};
</script>

<script lang="ts">
	// Numbered steps with a status each: readiness checklist (J3, 10 §5.2 steps 1–10), the discovery generator
	// (P18 Input → … → Package), provisioning progress. Status is spelled out, never colour alone.
	let { steps, label = 'Steps' }: { steps: Step[]; label?: string } = $props();
</script>

<ol class="step-flow" aria-label={label} data-testid="step-flow">
	{#each steps as s, i (s.key)}
		<li class="step {s.status}" aria-current={s.status === 'current' ? 'step' : undefined}>
			<span class="num" aria-hidden="true">{i + 1}</span>
			<div class="body">
				<div class="head">
					{#if s.href}<a href={s.href}>{s.label}</a>{:else}<b>{s.label}</b>{/if}
					<span class="chip {CHIP[s.status][1]}">{CHIP[s.status][0]}</span>
				</div>
				{#if s.detail}<p class="muted">{s.detail}</p>{/if}
			</div>
		</li>
	{/each}
</ol>

<style>
	.step-flow {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 0;
	}
	.step {
		display: grid;
		grid-template-columns: 28px minmax(0, 1fr);
		gap: 12px;
		padding: 10px 0;
		border-bottom: 1px solid var(--line);
	}
	.step:last-child {
		border-bottom: 0;
	}
	.num {
		display: inline-grid;
		place-items: center;
		width: 26px;
		height: 26px;
		border-radius: 50%;
		border: 1.5px solid var(--line-2);
		font: 600 12px var(--mono);
		color: var(--muted);
	}
	.done .num {
		border-color: var(--ok);
		color: var(--ok);
	}
	.current .num {
		border-color: var(--accent);
		color: var(--accent);
	}
	.missing .num,
	.blocked .num {
		border-color: var(--err);
		color: var(--err);
	}
	.placeholder .num {
		border-color: var(--warn);
		color: var(--warn);
	}
	.head {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: center;
	}
	.body {
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.body p {
		margin: 4px 0 0;
		font-size: 13px;
	}
</style>
