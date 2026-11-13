<script lang="ts">
	// J2: containment and handover share over time (rolling), the north-star target, and release markers —
	// "did things move when a release went live?". Two small multiples sharing the x-axis (never two y-scales on one plot); hover shows the day.
	import { fmt } from '$lib/format';
	import type { TrendPoint } from '$lib/runtime/J2-insights';

	let {
		points,
		target,
		markers,
		span,
	}: {
		points: TrendPoint[];
		target: number | null;
		markers: { id: string; index: number; current: boolean }[];
		span: number;
	} = $props();

	type K = 'containment' | 'handover';
	const W = 720,
		L = 40,
		R = 96,
		T = 34,
		PH = 104,
		GAP = 40,
		B = 26;
	const iw = W - L - R;
	const H = T + PH + GAP + PH + B;
	const x = (i: number) => L + (i * iw) / Math.max(1, points.length - 1);
	/** Small multiples: each measure gets its own focused 0–100 % slice, so a 5-point move is visible. */
	const panels = $derived.by(() =>
		(['containment', 'handover'] as K[]).map((key, n) => {
			const vals = points.map((p) => p[key]).filter((v): v is number => v != null);
			if (key === 'containment' && target != null) vals.push(target);
			const lo = Math.max(0, Math.floor((Math.min(...vals, 1) - 0.05) * 10) / 10);
			const hi = Math.min(1, Math.ceil((Math.max(...vals, 0) + 0.05) * 10) / 10);
			const top = T + n * (PH + GAP);
			const y = (v: number) => top + PH - ((v - lo) / Math.max(0.01, hi - lo)) * PH;
			let d = '';
			points.forEach((p, i) => {
				const v = p[key];
				if (v != null) d += `${d ? 'L' : 'M'}${x(i).toFixed(1)} ${y(v).toFixed(1)}`;
			});
			const ticks = [lo, (lo + hi) / 2, hi];
			return {
				key,
				top,
				y,
				d,
				ticks,
				title: key === 'containment' ? 'Resolved without a person' : 'Handed to staff',
				cls: key === 'containment' ? 'c' : 'h',
				last: points.at(-1)?.[key] ?? null,
				first: points.find((p) => p[key] != null)?.[key] ?? null,
			};
		}),
	);
	const step = $derived(Math.max(1, Math.ceil(points.length / 6)));
	let hover = $state<number | null>(null);
	/** On a phone the plot scrolls sideways; start at the newest days, where the releases are. */
	function toLatest(el: HTMLElement) {
		el.scrollLeft = el.scrollWidth;
	}

	function move(e: PointerEvent) {
		const svg = (e.currentTarget as SVGElement).ownerSVGElement ?? (e.currentTarget as SVGSVGElement);
		const r = svg.getBoundingClientRect();
		const px = ((e.clientX - r.left) / r.width) * W;
		const i = Math.round(((px - L) / iw) * (points.length - 1));
		hover = Math.max(0, Math.min(points.length - 1, i));
	}
	const hp = $derived(hover == null ? null : points[hover]);
	const tipLeft = $derived(hover == null ? 0 : (x(hover) / W) * 100);
</script>

<div class="j2t">
	<ul class="j2t-legend" aria-hidden="true">
		{#if target != null}<li><span class="sw sw-t"></span>Target</li>{/if}
		{#if markers.length}<li><span class="sw sw-r"></span>Release applied</li>{/if}
	</ul>
	<div class="j2t-plot scroll-x" use:toLatest>
		<svg
			viewBox="0 0 {W} {H}"
			role="img"
			aria-label="Share of cases resolved without a person and handed to staff, {span}-day rolling"
		>
			{#each panels as pn (pn.key)}
				<text class="ptitle" x={L} y={pn.top - 10}
					><tspan class="pdot-{pn.cls}">●</tspan>
					{pn.title}{#if pn.first != null && pn.last != null}<tspan class="pdelta">
							{`\u00a0· ${fmt.pct(pn.first, 0)} → ${fmt.pct(pn.last, 0)} over the window`}</tspan
						>{/if}</text
				>
				{#each pn.ticks as g (g)}
					<line class="grid" x1={L} x2={L + iw} y1={pn.y(g)} y2={pn.y(g)} />
					<text class="tick" x={L - 6} y={pn.y(g) + 4} text-anchor="end">{Math.round(g * 100)}%</text>
				{/each}
				{#each markers as m (m.id)}
					<line
						class="rel"
						class:rel-cur={m.current}
						x1={x(m.index)}
						x2={x(m.index)}
						y1={pn.top}
						y2={pn.top + PH}
					/>
				{/each}
				{#if pn.key === 'containment' && target != null}
					<line class="tgt" x1={L} x2={L + iw} y1={pn.y(target)} y2={pn.y(target)} />
					<text class="tgt-label" x={L + 6} y={pn.y(target) - 5}>target {fmt.pct(target, 0)}</text>
				{/if}
				<path class="ln ln-{pn.cls}" d={pn.d} />
				{#if pn.last != null}
					<circle class="dot dot-{pn.cls}" cx={x(points.length - 1)} cy={pn.y(pn.last)} r="4" />
					<text class="end" x={L + iw + 6} y={pn.y(pn.last) + (pn.key === 'containment' ? 14 : 4)}
						>{fmt.pct(pn.last, 0)}</text
					>
				{/if}
				{#if hover != null && hp && hp[pn.key] != null}
					<line class="cross" x1={x(hover)} x2={x(hover)} y1={pn.top} y2={pn.top + PH} />
					<circle class="dot dot-{pn.cls}" cx={x(hover)} cy={pn.y(hp[pn.key]!)} r="4" />
				{/if}
			{/each}
			{#each markers as m (m.id)}
				<text class="rel-label" x={x(m.index) + 4} y={T + PH + GAP - 24}>{m.id.replace('cfg-', '')}</text>
			{/each}
			{#each points as p, i (p.day)}
				{#if i % step === 0 || i === points.length - 1}
					<text class="tick" x={x(i)} y={H - 6} text-anchor="middle">{p.day.slice(5)}</text>
				{/if}
			{/each}
			<rect
				class="hit"
				x={L}
				y={T}
				width={iw}
				height={PH * 2 + GAP}
				role="presentation"
				onpointermove={move}
				onpointerleave={() => (hover = null)}
			/>
		</svg>
		{#if hp}
			<div class="j2t-tip" style:left="{Math.min(80, Math.max(12, tipLeft))}%">
				<b>{hp.day}</b>
				<span>{fmt.n(hp.cases)} cases that day</span>
				<span
					><i class="sw sw-c"></i>{hp.containment == null ? '—' : fmt.pct(hp.containment, 0)} resolved</span
				>
				<span><i class="sw sw-h"></i>{hp.handover == null ? '—' : fmt.pct(hp.handover, 0)} to staff</span>
			</div>
		{/if}
	</div>
	<details class="j2t-data">
		<summary>View data</summary>
		<div class="tbl-wrap">
			<table class="tbl dense">
				<thead
					><tr
						><th>Day</th><th class="r">Cases</th><th class="r">Resolved ({span} d)</th><th class="r"
							>To staff ({span} d)</th
						></tr
					></thead
				>
				<tbody>
					{#each points as p (p.day)}
						<tr>
							<td class="mono">{p.day}</td>
							<td class="r">{fmt.n(p.cases)}</td>
							<td class="r">{p.containment == null ? '—' : fmt.pct(p.containment, 0)}</td>
							<td class="r">{p.handover == null ? '—' : fmt.pct(p.handover, 0)}</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	</details>
</div>

<style>
	.j2t-legend {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 16px;
		margin: 0 0 6px;
		padding: 0;
		list-style: none;
		font-size: 12px;
		color: var(--muted);
	}
	.j2t-legend li {
		display: inline-flex;
		align-items: center;
		gap: 6px;
	}
	.sw {
		display: inline-block;
		width: 14px;
		height: 0;
		border-top: 2px solid currentColor;
		vertical-align: middle;
		margin-right: 4px;
	}
	.sw-c {
		color: var(--accent);
	}
	.sw-h {
		color: var(--work);
		border-top-style: solid;
	}
	.sw-t {
		color: var(--muted);
		border-top-style: dashed;
	}
	.sw-r {
		color: var(--faint);
		border-top-style: dotted;
	}
	.j2t-plot {
		position: relative;
		overflow-x: auto;
	}
	@media (max-width: 640px) {
		.j2t-plot svg {
			min-width: 600px;
		}
	}
	svg {
		display: block;
		width: 100%;
		height: auto;
		overflow: visible;
	}
	.grid {
		stroke: var(--line-2);
		stroke-width: 1;
	}
	.tick {
		fill: var(--faint);
		font-size: 11px;
		font-family: var(--mono);
	}
	.ln {
		fill: none;
		stroke-width: 2;
		stroke-linejoin: round;
		stroke-linecap: round;
	}
	.ln-c {
		stroke: var(--accent);
	}
	.ln-h {
		stroke: var(--work);
	}
	.dot {
		stroke: var(--panel);
		stroke-width: 2;
	}
	.dot-c {
		fill: var(--accent);
	}
	.dot-h {
		fill: var(--work);
	}
	.end,
	.tgt-label,
	.rel-label {
		fill: var(--muted);
		font-size: 11px;
	}
	.end {
		fill: var(--fg);
	}
	.ptitle {
		fill: var(--fg);
		font-size: 12px;
		font-weight: 600;
	}
	.pdelta {
		fill: var(--muted);
		font-weight: 400;
	}
	.pdot-c {
		fill: var(--accent);
	}
	.pdot-h {
		fill: var(--work);
	}
	.rel-label {
		font-family: var(--mono);
		font-size: 10px;
	}
	.tgt {
		stroke: var(--muted);
		stroke-width: 1.5;
		stroke-dasharray: 5 4;
	}
	.rel {
		stroke: var(--faint);
		stroke-width: 1;
		stroke-dasharray: 2 3;
	}
	.rel-cur {
		stroke: var(--fg);
	}
	.cross {
		stroke: var(--line);
		stroke-width: 1;
	}
	.hit {
		fill: transparent;
		cursor: crosshair;
	}
	.j2t-tip {
		position: absolute;
		top: 8px;
		transform: translateX(-50%);
		display: grid;
		gap: 2px;
		padding: 8px 10px;
		background: var(--panel);
		border: 1px solid var(--line);
		border-radius: var(--r);
		box-shadow: var(--shadow);
		font-size: 12px;
		pointer-events: none;
		white-space: nowrap;
	}
	.j2t-tip i {
		display: inline-block;
		width: 10px;
		margin-right: 6px;
	}
	.j2t-data {
		margin-top: 6px;
		font-size: 13px;
	}
	.j2t-data summary {
		cursor: pointer;
		color: var(--muted);
	}
</style>
