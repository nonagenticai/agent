<script lang="ts">
	// J12: where a procedure sits in a conversation (03 §1, 12 §2.1): message → qualifier fills the context space's
	// required slots → the procedure's steps → R1 checks the reply → resolved / not eligible / handed over.
	// Labels only; the data is in the cards below. Doubles as the legend of the step-kind tags.
	import { app } from '$lib/app.svelte';
	import KindTag from './KindTag.svelte';
	import { STEP_KIND, type StepKind } from './view';

	const KINDS = Object.keys(STEP_KIND) as StepKind[];
	const j = $derived(`/admin/jobs/${app.job}`);
</script>

<section class="rp" aria-label="How a request becomes action">
	<ol class="rp-path">
		<li>
			<span class="rp-n">1</span>
			<b>Customer asks</b>
			<span class="muted">A message arrives in a channel.</span>
		</li>
		<li>
			<span class="rp-n">2</span>
			<b>Context space</b>
			<span class="muted"
				>Gates pick the request type; the qualifier asks for its <em>required slots</em>.
				<a href="{j}/gates">Gates</a></span
			>
		</li>
		<li class="rp-main">
			<span class="rp-n">3</span>
			<b>Procedure runs</b>
			<span class="muted"
				>The live version’s steps (the one the live release carries), each one operation inside the activity’s
				grant.
				<a href="{j}/toolsets">Toolsets</a></span
			>
			<span class="rp-legend">
				{#each KINDS as k (k)}<KindTag kind={k} wide />{/each}
				<span class="muted rp-gate"><span aria-hidden="true">◆</span> gate: “no” ends the case</span>
			</span>
		</li>
		<li>
			<span class="rp-n">4</span>
			<b>Outcome</b>
			<span class="muted"
				>Resolved · not eligible (a gate said no) · handed over (a step failed, or nothing is live).
				<a href="{j}/handover">Handover queue</a></span
			>
		</li>
	</ol>
</section>

<style>
	.rp {
		margin: 0 0 14px;
		padding: 12px 14px;
		border: 1px solid var(--line);
		border-radius: var(--r);
		background: var(--panel);
	}
	.rp-path {
		display: grid;
		grid-template-columns: 1fr 1.3fr 2.4fr 1.5fr;
		gap: 10px;
		list-style: none;
		margin: 0;
		padding: 0;
	}
	@media (max-width: 1000px) {
		.rp-path {
			grid-template-columns: 1fr;
		}
	}
	.rp-path li {
		display: grid;
		grid-template-columns: 22px 1fr;
		column-gap: 8px;
		row-gap: 3px;
		align-content: start;
		font-size: 12.5px;
		position: relative;
	}
	.rp-path li > :not(.rp-n) {
		grid-column: 2;
	}
	.rp-n {
		grid-row: span 3;
		display: grid;
		place-items: center;
		width: 22px;
		height: 22px;
		border-radius: 50%;
		font: 600 11px/1 var(--mono);
		color: var(--muted);
		border: 1px solid var(--line);
	}
	.rp-main .rp-n {
		color: var(--accent);
		border-color: var(--accent);
	}
	.rp-path b {
		font-size: 13px;
	}
	.rp-legend {
		display: flex;
		flex-wrap: wrap;
		gap: 5px;
		align-items: center;
		margin-top: 3px;
	}
	.rp-gate {
		font-size: 11.5px;
	}
	.rp-gate span {
		color: var(--work);
	}
</style>
