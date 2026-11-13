<script lang="ts">
	// P10 lifecycle diagram (11 §1.3, §3.2): the object's state machine drawn left → right from its initial status,
	// terminal "ways out" reached from several statuses in a bottom lane. Agent operations are solid arrows, backend
	// events (things the system of record does) dashed. Selecting a status or a verb highlights what it allows and
	// which statuses refuse it (`invalid_state:<s>`). The same facts follow as text for screen readers.
	import type { ObjectDef } from '$lib/api/types/model';
	import { isBackendEvent, verbStates } from './logic';
	import { lifecycleLayout, NODE_H, NODE_W, shortVerb } from './objectDetailP10';

	let {
		def,
		showEvents = true,
		selectedState = $bindable<string | null>(null),
		selectedVerb = $bindable<string | null>(null),
	}: {
		def: Pick<ObjectDef, 'object' | 'states' | 'transitions'>;
		showEvents?: boolean;
		selectedState?: string | null;
		selectedVerb?: string | null;
	} = $props();

	const uid = $props.id();
	const layout = $derived(lifecycleLayout(def.states, def.transitions));
	const edges = $derived(def.transitions.filter((t) => showEvents || !isBackendEvent(t.verb)));
	const verbInfo = $derived(verbStates(def).find((v) => v.verb === selectedVerb) ?? null);
	const labelOf = (s: string) => def.states.find((x) => x.status === s)?.label ?? s;

	/** Edge geometry: forward bezier, a loop for a verb that keeps the status, a drop into the exit lane, a back arc. */
	function geom(from: string, to: string) {
		const a = layout.pos.get(from);
		const b = layout.pos.get(to);
		if (!a || !b) return null;
		if (from === to) {
			const x = a.x + NODE_W / 2;
			return {
				d: `M${x - 12} ${a.y} C${x - 30} ${a.y - 34} ${x + 30} ${a.y - 34} ${x + 12} ${a.y}`,
				lx: x,
				ly: a.y - 28,
			};
		}
		if (b.lane === 'exit' && a.lane === 'main') {
			const x1 = a.x + NODE_W / 2,
				y1 = a.y + NODE_H,
				x2 = b.x + NODE_W / 2,
				y2 = b.y;
			const my = (y1 + y2) / 2;
			return {
				d: `M${x1} ${y1} C${x1} ${my} ${x2} ${my} ${x2} ${y2}`,
				lx: x2 + NODE_W / 2 + 30,
				ly: y2 + NODE_H / 2 + 4,
			};
		}
		if (b.x > a.x) {
			const x1 = a.x + NODE_W,
				y1 = a.y + NODE_H / 2,
				x2 = b.x,
				y2 = b.y + NODE_H / 2;
			const mx = (x1 + x2) / 2;
			// Crossing diagonals share a midpoint: label a sloped edge near its target instead.
			const t = y1 === y2 ? 0.5 : 0.74;
			const bez = (p0: number, p1: number, p2: number, p3: number) =>
				(1 - t) ** 3 * p0 + 3 * (1 - t) ** 2 * t * p1 + 3 * (1 - t) * t ** 2 * p2 + t ** 3 * p3;
			return {
				d: `M${x1} ${y1} C${mx} ${y1} ${mx} ${y2} ${x2} ${y2}`,
				lx: bez(x1, mx, mx, x2),
				ly: bez(y1, y1, y2, y2) - 6,
			};
		}
		// Backward (or same column): leave from the left, swing below, come back in from below.
		const x1 = a.x,
			y1 = a.y + NODE_H * 0.7,
			x2 = b.x + NODE_W * 0.75,
			y2 = b.y + NODE_H;
		return {
			d: `M${x1} ${y1} C${x1 - 30} ${y1 + 26} ${x2} ${y2 + 30} ${x2} ${y2}`,
			lx: (x1 + x2) / 2 - 4,
			ly: Math.max(y1, y2) + 18,
		};
	}

	/** Label each (verb, target) once — `cancel` from three statuses into Cancelled is one label, not three. */
	const labelled = $derived.by(() => {
		const seen = new Set<string>();
		return edges.map((t) => {
			const k = `${t.verb}\u0000${t.to_status}`;
			if (seen.has(k)) return false;
			seen.add(k);
			return true;
		});
	});

	function edgeClass(verb: string, from: string) {
		const ev = isBackendEvent(verb);
		const on = selectedVerb ? verb === selectedVerb : selectedState ? from === selectedState : false;
		const dim =
			(selectedVerb && verb !== selectedVerb) || (!selectedVerb && selectedState && from !== selectedState);
		return `edge ${ev ? 'event' : 'op'}${on ? ' on' : ''}${dim ? ' dim' : ''}`;
	}

	/** Multi-word verbs (`payment received`) wrap so they fit between two columns. */
	function labelLines(verb: string, guard: string | null) {
		const words = shortVerb(verb).split(' ');
		const lines: string[] = [];
		for (const w of words) {
			const last = lines[lines.length - 1];
			if (last !== undefined && (last + ' ' + w).length <= 11) lines[lines.length - 1] = `${last} ${w}`;
			else lines.push(w);
		}
		if (guard) lines.push(`[${guard}]`);
		return lines;
	}

	function pick(status: string) {
		selectedState = selectedState === status && !selectedVerb ? null : status;
		selectedVerb = null;
	}
</script>

<figure class="lifecycle" data-testid="lifecycle">
	<div class="scroll scroll-x">
		<svg
			viewBox="0 0 {layout.width} {layout.height}"
			style="min-width: {Math.min(760, layout.width)}px; max-width: {layout.width}px"
			role="group"
			aria-labelledby="{uid}-t"
		>
			<title id="{uid}-t">{def.object} lifecycle</title>
			<defs>
				{#each ['op', 'event', 'on'] as m (m)}
					<marker
						id="{uid}-{m}"
						viewBox="0 0 10 10"
						refX="9"
						refY="5"
						markerWidth="7"
						markerHeight="7"
						orient="auto-start-reverse"
					>
						<path d="M0 0 10 5 0 10z" class="head {m}" />
					</marker>
				{/each}
			</defs>
			{#if layout.laneY !== null}
				<line class="lane" x1="8" x2={layout.width - 8} y1={layout.laneY - 22} y2={layout.laneY - 22} />
				<text class="lane-label" x="10" y={layout.laneY - 28}>Ways out</text>
			{/if}
			{#each edges as t, i (t.verb + t.from_status)}
				{@const g = geom(t.from_status, t.to_status)}
				{#if g}
					{@const cls = edgeClass(t.verb, t.from_status)}
					<path
						class={cls}
						d={g.d}
						marker-end="url(#{uid}-{cls.includes(' on') ? 'on' : isBackendEvent(t.verb) ? 'event' : 'op'})"
					/>
					{#if labelled[i]}
						{@const lines = labelLines(t.verb, t.guard_rule)}
						<text
							class={cls.replace('edge ', 'lbl ')}
							x={g.lx}
							y={g.ly - (lines.length - 1) * 12}
							text-anchor="middle"
							>{#each lines as ln, li (li)}<tspan x={g.lx} dy={li ? 12 : 0}>{ln}</tspan>{/each}</text
						>
					{/if}
				{/if}
			{/each}
			{#each def.states as s (s.status)}
				{@const p = layout.pos.get(s.status)}
				{#if p}
					{@const ok = verbInfo?.allowed_from.includes(s.status)}
					{@const refused = verbInfo?.invalid_states.includes(s.status)}
					<g
						class="node"
						class:terminal={s.terminal}
						class:sel={s.status === selectedState}
						class:ok
						class:refused
						role="button"
						tabindex="0"
						aria-pressed={s.status === selectedState}
						aria-label="{s.label}{s.terminal ? ' (terminal)' : ''}{refused
							? `, refuses ${selectedVerb}`
							: ok
								? `, allows ${selectedVerb}`
								: ''}"
						onclick={() => pick(s.status)}
						onkeydown={(e) => {
							if (e.key === 'Enter' || e.key === ' ') {
								e.preventDefault();
								pick(s.status);
							}
						}}
					>
						{#if s.initial}
							<circle class="start" cx={p.x - 14} cy={p.y + NODE_H / 2} r="5" />
							<line class="start-line" x1={p.x - 9} x2={p.x} y1={p.y + NODE_H / 2} y2={p.y + NODE_H / 2} />
						{/if}
						<rect x={p.x} y={p.y} width={NODE_W} height={NODE_H} rx="8" />
						{#if s.terminal}<rect
								class="inner"
								x={p.x + 3}
								y={p.y + 3}
								width={NODE_W - 6}
								height={NODE_H - 6}
								rx="6"
							/>{/if}
						<text x={p.x + NODE_W / 2} y={p.y + NODE_H / 2 + 4} text-anchor="middle"
							>{s.label}{s.canonical ? '' : ' *'}</text
						>
						{#if refused}
							<text class="badge" x={p.x + NODE_W - 6} y={p.y + 12} text-anchor="end">✕</text>
						{:else if ok}
							<text class="badge" x={p.x + NODE_W - 6} y={p.y + 12} text-anchor="end">✓</text>
						{/if}
					</g>
				{/if}
			{/each}
		</svg>
	</div>
	<div class="legend-row" aria-label="Legend">
		<span
			><svg width="26" height="8" aria-hidden="true"
				><line class="edge op" x1="0" x2="26" y1="4" y2="4" /></svg
			>
			agent operation</span
		>
		<span
			><svg width="26" height="8" aria-hidden="true"
				><line class="edge event" x1="0" x2="26" y1="4" y2="4" /></svg
			>
			backend event (system of record)</span
		>
		<span><span class="sw dbl" aria-hidden="true"></span> terminal</span>
		<span><span class="sw dot" aria-hidden="true"></span> initial</span>
		{#if def.states.some((s) => !s.canonical)}<span>* connector-only status</span>{/if}
	</div>
	<details class="sr-list">
		<summary>Transitions as a list</summary>
		<ul>
			{#each def.transitions as t (t.verb + t.from_status)}
				<li>
					{labelOf(t.from_status)} → {labelOf(t.to_status)} ({t.verb}{t.guard_rule
						? `, guard ${t.guard_rule}`
						: ''})
				</li>
			{:else}
				<li>No transitions.</li>
			{/each}
		</ul>
	</details>
</figure>

<style>
	.lifecycle {
		margin: 0;
		display: grid;
		gap: 8px;
	}
	/* .scroll-x draws the edge fade; a hairline frame instead of a sunk fill so the fade covers match */
	.scroll {
		border: 1px solid var(--line-2);
		border-radius: 8px;
		padding: 6px;
	}
	svg {
		display: block;
		width: 100%;
		height: auto;
		font: 12px var(--sans);
	}
	.edge {
		fill: none;
		stroke-width: 1.5;
		transition: opacity 0.15s;
	}
	.edge.op {
		stroke: var(--accent);
	}
	.edge.event {
		stroke: var(--muted);
		stroke-dasharray: 5 4;
	}
	.edge.on {
		stroke: var(--fg);
		stroke-width: 2.6;
	}
	.edge.dim {
		opacity: 0.18;
	}
	.head.op {
		fill: var(--accent);
	}
	.head.event {
		fill: var(--muted);
	}
	.head.on {
		fill: var(--fg);
	}
	.lbl {
		font: 11px var(--mono);
		paint-order: stroke;
		stroke: var(--sunk);
		stroke-width: 4px;
		stroke-linejoin: round;
	}
	.lbl.op {
		fill: var(--accent);
	}
	.lbl.event {
		fill: var(--muted);
	}
	.lbl.dim {
		opacity: 0.2;
	}
	.lbl.on {
		fill: var(--fg);
		font-weight: 600;
	}
	.lane {
		stroke: var(--line);
		stroke-dasharray: 2 4;
	}
	.lane-label {
		fill: var(--faint);
		font: 600 10.5px var(--cond);
		letter-spacing: 0.06em;
		text-transform: uppercase;
	}
	.node {
		cursor: pointer;
		outline: none;
	}
	.node rect {
		fill: var(--panel);
		stroke: var(--line);
		stroke-width: 1.4;
	}
	.node .inner {
		fill: none;
		stroke: var(--line);
	}
	.node text {
		fill: var(--fg);
		font-weight: 500;
	}
	.node:hover rect:not(.inner),
	.node:focus-visible rect:not(.inner) {
		stroke: var(--accent);
	}
	.node.sel rect:not(.inner) {
		stroke: var(--accent);
		stroke-width: 2.8;
	}
	.node.ok rect:not(.inner) {
		stroke: var(--ok);
		stroke-width: 2.4;
		fill: color-mix(in srgb, var(--ok) 10%, var(--panel));
	}
	.node.refused rect:not(.inner) {
		stroke: var(--err);
		stroke-dasharray: 4 3;
		fill: color-mix(in srgb, var(--err) 8%, var(--panel));
	}
	.node.ok .badge {
		fill: var(--ok);
	}
	.node.refused .badge {
		fill: var(--err);
	}
	.badge {
		font: 700 11px var(--sans);
	}
	.start {
		fill: var(--fg);
	}
	.start-line {
		stroke: var(--fg);
		stroke-width: 1.5;
	}
	.legend-row {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 16px;
		font-size: 12px;
		color: var(--muted);
	}
	.legend-row > span {
		display: inline-flex;
		align-items: center;
		gap: 6px;
	}
	.legend-row svg {
		width: 26px;
	}
	.sw {
		display: inline-block;
		width: 14px;
		height: 10px;
		border-radius: 3px;
	}
	.sw.dbl {
		width: 18px;
		height: 12px;
		border: 3px double var(--muted);
		background: var(--panel);
	}
	.sw.dot {
		width: 9px;
		height: 9px;
		border-radius: 50%;
		background: var(--fg);
	}
	.sr-list {
		font-size: 12.5px;
		color: var(--muted);
	}
</style>
